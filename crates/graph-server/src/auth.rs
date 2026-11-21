//! Authentication and roles (ADR 0012).
//!
//! * [`AuthLayer`] is a tower layer in front of every gRPC service. It reads
//!   the request's `:path` and its `authorization: Bearer <value>` header,
//!   checks the credential and the method's entry in the deny-by-default
//!   role table ([`need`]), and refuses the call before any service code
//!   runs (so before the protocol-version check, and before the first body
//!   frame of a streaming call). An accepted call runs with its [`Caller`]
//!   in the request extensions and in the [`CALLER`] task-local.
//! * Client tokens (`mgt_...`) are checked against the SHA-256 digests in
//!   the token file (`--auth-tokens`); the cluster secret (`mgc_...`,
//!   `--cluster-secret-file`) against its own one or two digests. Every
//!   comparison is constant time over every stored digest.
//! * Both files live in an `ArcSwap` and are reloaded by content hash, on a
//!   5 s poll and on SIGHUP (Unix), and by [`Auth::reload_now`] in tests. A
//!   file that fails to read, parse or validate keeps the previous set.
//! * Nothing here ever formats a credential, a header or a digest into a
//!   log line, an error or a status.
use crate::conn::ConnInfo;
use arc_swap::ArcSwap;
use graph_proto::auth::{self as wire, CredKind};
use graph_store::StoreError;
use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime};
use tonic::codegen::http;
use tonic::metadata::{Ascii, MetadataMap, MetadataValue};
use zeroize::Zeroizing;

/// How often the token and secret files are re-read (content hash).
pub const RELOAD_POLL: Duration = Duration::from_secs(5);

/// At most one refusal line per peer address in this window.
pub const REFUSAL_LOG_WINDOW: Duration = Duration::from_secs(10);

/// The message of every `UNAUTHENTICATED` answer (D1: never which case).
pub const UNAUTHENTICATED_MSG: &str = "missing or invalid credentials";

// ---------------------------------------------------------------------------
// Roles and the method table.

/// A client token's role (D5). `Write` implies `Read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    Read,
    Write,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "read" => Some(Role::Read),
            "write" => Some(Role::Write),
            _ => None,
        }
    }

    /// Whether this role may call a method that needs `need`.
    pub fn covers(self, need: Role) -> bool {
        self >= need
    }
}

/// What a method requires (D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// No credential (`Store.Hello`, trimmed; health).
    Open,
    /// A `read` or `write` token.
    Read,
    /// A `write` token.
    Write,
    /// The cluster secret only.
    Node,
}

impl Need {
    fn as_str(self) -> &'static str {
        match self {
            Need::Open => "",
            Need::Read => "read",
            Need::Write => "write",
            Need::Node => "node",
        }
    }
}

const STORE: &str = "/memory_graph.v1.Store/";
const WRITE: &str = "/memory_graph.v1.Write/";
const ADMIN: &str = "/memory_graph.v1.Admin/";
const RAFT: &str = "/memory_graph.v1.Raft/";
const HEALTH: &str = "/grpc.health.v1.Health/";

/// The role table (D5). `None`: the method is not listed, and is refused
/// for every credential (a new RPC fails closed until it is classified;
/// `every_rpc_is_classified` enumerates the generated services).
pub fn need(path: &str) -> Option<Need> {
    if let Some(m) = path.strip_prefix(HEALTH) {
        return matches!(m, "Check" | "Watch" | "List").then_some(Need::Open);
    }
    if let Some(m) = path.strip_prefix(STORE) {
        return match m {
            "Hello" => Some(Need::Open),
            "Get" | "Parent" | "CountNodes" | "Roots" | "Children" | "ChildrenPage"
            | "Descendants" | "DescendantsPage" | "Ancestors" | "FileTokens" | "Describe"
            | "DescribeByScan" | "ExtractorGaps" | "SearchSymbols" | "Search" | "OpenSnapshot"
            | "CloseSnapshot" | "SnapshotStats" => Some(Need::Read),
            _ => None,
        };
    }
    if let Some(m) = path.strip_prefix(WRITE) {
        return match m {
            // `Delete` (issue #323) needs `write` even for a dry run, like `Prune`.
            "Index" | "IndexFile" | "IngestExtraction" | "Prune" | "Vacuum" | "Delete" => {
                Some(Need::Write)
            }
            _ => None,
        };
    }
    if let Some(m) = path.strip_prefix(ADMIN) {
        return match m {
            "Status" | "Members" | "Leader" | "SysInfo" | "Metrics" | "ReadIndex"
            | "ListBackups" => Some(Need::Read),
            "AddLearner" | "Promote" | "Remove" | "TransferLeader" | "TriggerElect"
            | "TriggerSnapshot" | "UploadSnapshot" | "Compact" | "Shutdown" => Some(Need::Write),
            "Join" | "UpdateAdvertise" => Some(Need::Node),
            _ => None,
        };
    }
    if let Some(m) = path.strip_prefix(RAFT) {
        return match m {
            "AppendEntries" | "Vote" | "InstallSnapshot" => Some(Need::Node),
            _ => None,
        };
    }
    None
}

/// The two `read` methods the cluster secret satisfies directly: the join
/// probes (D3).
fn secret_reads(path: &str) -> bool {
    matches!(
        path,
        "/memory_graph.v1.Admin/Status" | "/memory_graph.v1.Admin/Members"
    )
}

// ---------------------------------------------------------------------------
// The caller.

/// Who made an accepted call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallerKind {
    /// No valid credential (only on an open method).
    Anonymous,
    /// A client token: its id (never its value) and role.
    Token { id: Arc<str>, role: Role },
    /// The cluster secret. `role`: the role a forwarding node granted the
    /// original caller (`mg-forwarded-role`), on a forwarded call; `None`
    /// on a node-to-node method or a join probe.
    Node {
        role: Option<Role>,
        forwarded_token_id: Option<String>,
    },
}

/// An accepted call's identity, in the request extensions and the
/// [`CALLER`] task-local while its handler runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub kind: CallerKind,
    /// The connection it arrived on.
    pub conn: u64,
    /// The peer address, when known (the per-peer rate limit of audit
    /// denial lines, ADR 0013 D9).
    pub peer: Option<IpAddr>,
}

impl Caller {
    /// Whether the caller presented a valid credential.
    pub fn authenticated(&self) -> bool {
        !matches!(self.kind, CallerKind::Anonymous)
    }

    /// The role this caller acts with (what a forward grants the leader).
    pub fn role(&self) -> Option<Role> {
        match &self.kind {
            CallerKind::Anonymous => None,
            CallerKind::Token { role, .. } => Some(*role),
            CallerKind::Node { role, .. } => *role,
        }
    }

    /// The token id for logs and snapshot ownership; `None` for an
    /// anonymous caller.
    pub fn token_id(&self) -> Option<String> {
        match &self.kind {
            CallerKind::Anonymous => None,
            CallerKind::Token { id, .. } => Some(id.to_string()),
            CallerKind::Node {
                forwarded_token_id, ..
            } => Some(format!(
                "node:{}",
                forwarded_token_id.as_deref().unwrap_or("")
            )),
        }
    }
}

tokio::task_local! {
    /// The caller of the RPC whose handler is running (auth on only).
    pub static CALLER: Caller;
}

/// The current handler's caller; `None` with auth off, or outside a
/// handler (a background task).
pub fn current_caller() -> Option<Caller> {
    CALLER.try_with(Clone::clone).ok()
}

// ---------------------------------------------------------------------------
// The token file.

/// One `[[token]]` of the token file.
#[derive(Clone)]
pub struct TokenEntry {
    pub id: Arc<str>,
    pub role: Role,
    digest: [u8; 32],
    pub expires: Option<SystemTime>,
    /// The token's groups (`groups = [...]`, ADR 0013 D1), deduplicated, in
    /// file order; empty when the key is absent.
    pub groups: Vec<String>,
}

impl std::fmt::Debug for TokenEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenEntry")
            .field("id", &self.id)
            .field("role", &self.role)
            .field("expires", &self.expires)
            .field("groups", &self.groups)
            .finish_non_exhaustive()
    }
}

/// The parsed token file.
#[derive(Debug, Clone, Default)]
pub struct TokenSet {
    pub entries: Vec<TokenEntry>,
}

/// What a presented token matched.
enum Lookup<'a> {
    Found(&'a TokenEntry),
    Expired,
    Unknown,
}

/// The index of the stored digest equal to `d`, comparing every digest in
/// constant time with no early exit.
fn ct_find(digests: impl Iterator<Item = [u8; 32]>, d: &[u8; 32]) -> Option<usize> {
    use subtle::{ConditionallySelectable, ConstantTimeEq as _};
    let mut found = subtle::Choice::from(0);
    let mut at = 0u64;
    for (i, stored) in digests.enumerate() {
        let eq = stored.ct_eq(d);
        at = u64::conditional_select(&at, &(i as u64), eq);
        found |= eq;
    }
    bool::from(found).then_some(at as usize)
}

impl TokenSet {
    fn lookup(&self, d: &[u8; 32], now: SystemTime) -> Lookup<'_> {
        match ct_find(self.entries.iter().map(|e| e.digest), d) {
            Some(i) => {
                let e = &self.entries[i];
                if e.expires.is_some_and(|t| now >= t) {
                    Lookup::Expired
                } else {
                    Lookup::Found(e)
                }
            }
            None => Lookup::Unknown,
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    token: Vec<toml::Spanned<RawToken>>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawToken {
    id: String,
    role: String,
    sha256: String,
    #[serde(default)]
    expires: Option<toml::Value>,
    #[serde(default)]
    scope: Option<toml::Value>,
    #[serde(default)]
    groups: Option<Vec<String>>,
}

fn line_of(text: &str, at: usize) -> usize {
    text.as_bytes()[..at.min(text.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

fn parse_hex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// Parse and validate a token file (D2). Every refusal names the line and
/// never a value; the whole file is refused on any problem. An empty file
/// or one with no tokens is refused (it would lock everyone out).
pub fn parse_token_file(text: &str) -> Result<TokenSet, String> {
    let raw: RawFile = toml::from_str(text).map_err(|e| {
        // The error's own text quotes the offending line, which may hold a
        // digest: only its position and message are kept.
        let line = e.span().map_or(0, |s| line_of(text, s.start));
        let msg = e.message().lines().next().unwrap_or("").to_string();
        format!("line {line}: {msg}")
    })?;
    if raw.token.is_empty() {
        return Err("the file defines no [[token]]; refusing a set that admits nobody".into());
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut digests = std::collections::BTreeSet::new();
    let mut entries = Vec::with_capacity(raw.token.len());
    for t in raw.token {
        let line = line_of(text, t.span().start);
        let t = t.into_inner();
        let at = |what: &str| format!("line {line} (token `{}`): {what}", t.id);
        if t.scope.is_some() {
            return Err(at(
                "`scope` is not supported; per-org/repo permissions are policies in a separate \
                 file (`serve --auth-policies`, ADR 0013); refusing the file rather than \
                 ignoring it",
            ));
        }
        if t.id.trim().is_empty() {
            return Err(format!("line {line}: a token has an empty `id`"));
        }
        let role = Role::parse(&t.role)
            .ok_or_else(|| at("unknown `role` (expected `read` or `write`)"))?;
        let digest = parse_hex(&t.sha256).ok_or_else(|| {
            at("`sha256` must be 64 hexadecimal characters (the SHA-256 of the token)")
        })?;
        let expires = match &t.expires {
            None => None,
            Some(toml::Value::String(s)) => Some(
                parse_rfc3339(s)
                    .ok_or_else(|| at("`expires` is not an RFC 3339 date-time with an offset"))?,
            ),
            Some(toml::Value::Datetime(d)) => Some(
                parse_rfc3339(&d.to_string())
                    .ok_or_else(|| at("`expires` is not an RFC 3339 date-time with an offset"))?,
            ),
            Some(_) => return Err(at("`expires` is not an RFC 3339 date-time")),
        };
        if !ids.insert(t.id.clone()) {
            return Err(at("duplicate `id`"));
        }
        if !digests.insert(digest) {
            return Err(at("duplicate `sha256` (two entries for one token)"));
        }
        let mut groups: Vec<String> = Vec::new();
        for g in t.groups.iter().flatten() {
            if g.is_empty() {
                return Err(at("an empty name in `groups`"));
            }
            if !groups.contains(g) {
                groups.push(g.clone());
            }
        }
        entries.push(TokenEntry {
            id: Arc::from(t.id.as_str()),
            role,
            digest,
            expires,
            groups,
        });
    }
    Ok(TokenSet { entries })
}

/// `YYYY-MM-DDTHH:MM:SS[.frac](Z|+HH:MM|-HH:MM)`, as a `SystemTime`.
pub fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    if b.len() < 20 || !s.is_ascii() {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        t.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| t.parse().ok())?
    };
    if b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut i = 19;
    let mut nanos = 0u32;
    if b.get(i) == Some(&b'.') {
        let start = i + 1;
        let mut j = start;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j == start {
            return None;
        }
        let frac = &s[start..j.min(start + 9)];
        nanos = frac.parse::<u32>().ok()? * 10u32.pow(9 - frac.len() as u32);
        i = j;
    }
    let offset = match &s[i..] {
        "Z" | "z" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) && &o[3..4] == ":" => {
            let oh: i64 = o[1..3].parse().ok()?;
            let om: i64 = o[4..6].parse().ok()?;
            if oh > 23 || om > 59 {
                return None;
            }
            let v = oh * 3600 + om * 60;
            if o.starts_with('-') {
                -v
            } else {
                v
            }
        }
        _ => return None,
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset;
    let base = if secs >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))?
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))?
    };
    base.checked_add(Duration::from_nanos(u64::from(nanos)))
}

// ---------------------------------------------------------------------------
// The cluster secret file.

/// The cluster secret (D3): the digests a node accepts (current, and the
/// next one during a rotation) and the value it sends (the first line).
pub struct SecretSet {
    digests: Vec<[u8; 32]>,
    send: Zeroizing<String>,
}

impl std::fmt::Debug for SecretSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretSet")
            .field("accepted", &self.digests.len())
            .finish_non_exhaustive()
    }
}

/// Parse a cluster-secret file: one or two `mgc_` lines (current, then
/// next); blank lines and `#` comments are ignored.
pub fn parse_secret_file(text: &str) -> Result<SecretSet, String> {
    let mut lines = Vec::new();
    for (n, l) in text.lines().enumerate() {
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if wire::check_format(l) != Some(CredKind::Secret) {
            return Err(format!(
                "line {}: not a cluster secret (`mgc_` followed by 43 base64url characters; \
                 create one with `memory-graph auth new-secret`)",
                n + 1
            ));
        }
        lines.push(l);
    }
    match lines.len() {
        0 => Err("the file holds no cluster secret".into()),
        1 | 2 => Ok(SecretSet {
            digests: lines.iter().map(|l| wire::digest(l)).collect(),
            send: Zeroizing::new(lines[0].to_string()),
        }),
        n => Err(format!(
            "the file holds {n} secrets; at most two (current, then next) are allowed"
        )),
    }
}

/// A fresh credential: `prefix` and 32 bytes from the OS RNG in unpadded
/// base64url (D1).
pub fn new_credential(prefix: &str) -> Zeroizing<String> {
    use base64::Engine as _;
    use rand::RngCore;
    let mut bytes = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng.fill_bytes(&mut *bytes);
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(*bytes);
    Zeroizing::new(format!("{prefix}{body}"))
}

// ---------------------------------------------------------------------------
// Reloadable files.

/// Parses the file(s) of a [`Source`]: their bytes, in path order.
type ParseFn<T> = Box<dyn Fn(&[Vec<u8>]) -> Result<T, String> + Send + Sync>;

/// One reloadable set, read from one file or from a pair of files that are
/// validated together (the token and policy files, ADR 0013 D2): if either
/// fails, both keep the last good pair.
struct Source<T> {
    what: &'static str,
    paths: Vec<PathBuf>,
    cur: ArcSwap<T>,
    /// The content hash of the loaded file(s), and of the last content that
    /// failed (logged once); the lock also serializes reloads.
    state: Mutex<SourceState>,
    parse: ParseFn<T>,
}

/// Parse one UTF-8 text file with `f`.
fn text_parser<T: 'static>(f: fn(&str) -> Result<T, String>) -> ParseFn<T> {
    Box::new(move |files: &[Vec<u8>]| {
        let text =
            std::str::from_utf8(&files[0]).map_err(|_| "the file is not UTF-8".to_string())?;
        f(text)
    })
}

#[derive(Default)]
struct SourceState {
    loaded: [u8; 32],
    failed: Option<[u8; 32]>,
    read_failed: bool,
}

/// What a reload did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reload {
    /// The file's content is the one loaded.
    Unchanged,
    /// A new set was swapped in.
    Reloaded,
    /// The file could not be read, parsed or validated: the old set stays.
    Kept,
}

fn read_file(path: &Path) -> std::io::Result<Vec<u8>> {
    // Follows symlinks on every read (a Kubernetes `..data` swap).
    std::fs::read(path)
}

fn content_hash(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

/// One hash over every file of a source (each length-prefixed, so moving
/// bytes from one file to the other changes it).
fn files_hash(files: &[Vec<u8>]) -> [u8; 32] {
    if let [one] = files {
        return content_hash(one);
    }
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    for f in files {
        h.update((f.len() as u64).to_le_bytes());
        h.update(f);
    }
    h.finalize().into()
}

impl<T> Source<T> {
    fn load(what: &'static str, paths: &[&Path], parse: ParseFn<T>) -> Result<Self, String> {
        let mut files = Vec::with_capacity(paths.len());
        for p in paths {
            files.push(read_file(p).map_err(|e| format!("cannot read `{}`: {e}", p.display()))?);
        }
        let set = parse(&files).map_err(|e| match paths {
            [one] => format!("`{}`: {e}", one.display()),
            _ => e,
        })?;
        for p in paths {
            warn_if_readable_by_others(what, p);
        }
        Ok(Self {
            what,
            paths: paths.iter().map(|p| p.to_path_buf()).collect(),
            cur: ArcSwap::from_pointee(set),
            state: Mutex::new(SourceState {
                loaded: files_hash(&files),
                ..SourceState::default()
            }),
            parse,
        })
    }

    fn file_label(&self) -> String {
        self.paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" + ")
    }

    fn reload(&self) -> Reload {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut files = Vec::with_capacity(self.paths.len());
        for p in &self.paths {
            match read_file(p) {
                Ok(b) => files.push(b),
                Err(e) => {
                    if !st.read_failed {
                        tracing::error!(
                            file = %p.display(),
                            error = %e,
                            what = self.what,
                            "cannot read the file; keeping the loaded set"
                        );
                    }
                    st.read_failed = true;
                    return Reload::Kept;
                }
            }
        }
        st.read_failed = false;
        let h = files_hash(&files);
        if h == st.loaded {
            st.failed = None;
            return Reload::Unchanged;
        }
        match (self.parse)(&files) {
            Ok(set) => {
                self.cur.store(Arc::new(set));
                st.loaded = h;
                st.failed = None;
                tracing::info!(file = %self.file_label(), what = self.what, "reloaded");
                Reload::Reloaded
            }
            Err(e) => {
                if st.failed != Some(h) {
                    tracing::error!(
                        file = %self.file_label(),
                        error = %e,
                        what = self.what,
                        "the file failed validation; keeping the loaded set"
                    );
                }
                st.failed = Some(h);
                Reload::Kept
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The token and policy files as one pair (ADR 0013 D2).

/// The token file and, with `--auth-policies`, the policy set it pairs
/// with: loaded, validated and swapped together.
#[derive(Debug, Clone, Default)]
pub struct AuthState {
    pub tokens: TokenSet,
    /// `None`: no `--auth-policies` (ADR 0012 behaviour, nothing evaluated).
    pub policies: Option<Arc<graph_authz::PolicySet>>,
    /// SHA-256 (hex) of the normalised policy set, the built-ins switch and
    /// the token file's id/role/groups (never a digest); empty without
    /// `--auth-policies`.
    pub policy_hash: String,
}

impl AuthState {
    /// The token entry with this id, if it is in the file and not expired.
    pub fn entry(&self, id: &str, now: SystemTime) -> Option<&TokenEntry> {
        self.tokens
            .entries
            .iter()
            .find(|e| &*e.id == id)
            .filter(|e| !e.expires.is_some_and(|t| now >= t))
    }
}

/// Parse a token file and a policy file together (D2). The policy file is
/// refused on any parse error and, until scoped reads land, on the interim
/// read rule (Rollout, Q9); unknown principals only warn.
pub fn parse_pair(tokens: &str, policies: &[u8], builtins: bool) -> Result<AuthState, String> {
    let set = parse_token_file(tokens).map_err(|e| format!("token file: {e}"))?;
    let (state, warnings) = pair_from(set, policies, builtins)?;
    for w in warnings {
        tracing::warn!(warning = %w, "auth policy file");
    }
    Ok(state)
}

/// [`parse_pair`] without logging: the state and the warnings (for `auth
/// check`).
pub fn pair_from(
    tokens: TokenSet,
    policies: &[u8],
    builtins: bool,
) -> Result<(AuthState, Vec<String>), String> {
    let parsed = graph_authz::parse(policies).map_err(|e| format!("policy file: {e}"))?;
    let mut set = graph_authz::PolicySet::new(parsed);
    set.check_interim_read_rule()
        .map_err(|e| format!("policy file: {e}"))?;
    let warnings = set.lint(&|t| tokens.entries.iter().any(|e| &*e.id == t), &|g| {
        tokens
            .entries
            .iter()
            .any(|e| e.groups.iter().any(|x| x == g))
    });
    if builtins {
        set = set.with_builtins();
    }
    let policy_hash = {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(set.normalized().as_bytes());
        h.update(if builtins {
            "\n#builtins on\n"
        } else {
            "\n#builtins off\n"
        });
        let mut ids: Vec<String> = tokens
            .entries
            .iter()
            .map(|e| format!("{:?} {} {:?}", &*e.id, e.role.as_str(), e.groups))
            .collect();
        ids.sort();
        for l in ids {
            h.update(l.as_bytes());
            h.update(b"\n");
        }
        h.finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    Ok((
        AuthState {
            tokens,
            policies: Some(Arc::new(set)),
            policy_hash,
        },
        warnings,
    ))
}

/// The policy action of a method that is decided on `Cluster::"this"` in
/// the auth layer (ADR 0013 D1): `status`, `snapshot` and `admin`.
pub fn cluster_action(path: &str) -> Option<graph_authz::Action> {
    use graph_authz::Action as A;
    if let Some(m) = path.strip_prefix(ADMIN) {
        return match m {
            // `ListBackups` is `status` (ADR 0013 D1, corrected 2026-11-21):
            // ADR 0012 lets a `read` token list backups, and the built-in
            // role policies must equal ADR 0012's roles exactly.
            "Status" | "Members" | "Leader" | "SysInfo" | "Metrics" | "ReadIndex"
            | "ListBackups" => Some(A::Status),
            "TriggerSnapshot" => Some(A::Snapshot),
            "AddLearner" | "Promote" | "Remove" | "TransferLeader" | "TriggerElect"
            | "UploadSnapshot" | "Compact" | "Shutdown" => Some(A::Admin),
            _ => None,
        };
    }
    (path == "/memory_graph.v1.Write/Vacuum").then_some(A::Admin)
}

/// The policy action of every client method (D1): cluster actions, the
/// per-request write actions (decided in the handler, on the request's
/// repo or org) and the read actions.
pub fn method_action(path: &str) -> Option<graph_authz::Action> {
    use graph_authz::Action as A;
    if let Some(a) = cluster_action(path) {
        return Some(a);
    }
    if let Some(m) = path.strip_prefix(WRITE) {
        return match m {
            "Index" | "IndexFile" | "IngestExtraction" => Some(A::Index),
            "Prune" => Some(A::Prune),
            "Delete" => Some(A::Delete),
            _ => None,
        };
    }
    match need(path) {
        Some(Need::Read) if path.starts_with(STORE) => Some(match path {
            "/memory_graph.v1.Store/Search" | "/memory_graph.v1.Store/SearchSymbols" => A::Search,
            _ => A::Read,
        }),
        _ => None,
    }
}

/// The message of a policy refusal: the action only, never a resource or a
/// policy `@id` (D5, D9).
pub fn policy_denied(action: graph_authz::Action) -> StoreError {
    StoreError::PermissionDenied {
        msg: format!("permission denied for action `{}`", action.name()),
        required_role: String::new(),
        action: action.name().into(),
    }
}

// ---------------------------------------------------------------------------
// Forwarded principals (ADR 0013 D7).

/// The identity a forwarding node claims on a cluster-secret call, parsed
/// strictly (with `--auth-policies` only; ADR 0012's lenient reading is
/// unchanged without a policy file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forwarded {
    /// `mg-forwarded-role`: the role the forwarding node granted.
    pub role: Role,
    /// `mg-forwarded-token-id`: looked up in the receiving node's own file.
    pub token_id: String,
    /// `mg-forwarded-action`: the read barrier's action (`read`/`search`).
    pub action: Option<graph_authz::Action>,
}

/// Why a set of `mg-forwarded-*` headers was refused (for the audit line;
/// never sent to the client, never a header value).
pub type ForwardedRefusal = &'static str;

/// Parse the `mg-forwarded-*` headers of a cluster-secret call (D7): each
/// present exactly once (`mg-forwarded-action` is optional, but at most
/// once), non-empty, printable ASCII, at most [`wire::FORWARDED_HEADER_MAX`]
/// bytes; `mg-forwarded-by` a node id (digits), `mg-forwarded-role` `read`
/// or `write`, `mg-forwarded-token-id` with no surrounding blanks (the
/// token file's id syntax; it must then match an id exactly),
/// `mg-forwarded-action` `read` or `search`. Anything else is refused,
/// never read as another principal.
pub fn parse_forwarded(headers: &http::HeaderMap) -> Result<Forwarded, ForwardedRefusal> {
    fn one<'a>(
        h: &'a http::HeaderMap,
        name: &str,
        required: bool,
    ) -> Result<Option<&'a str>, ForwardedRefusal> {
        let mut all = h.get_all(name).iter();
        let Some(v) = all.next() else {
            return if required {
                Err("a forwarded header is missing")
            } else {
                Ok(None)
            };
        };
        if all.next().is_some() {
            return Err("a forwarded header is repeated");
        }
        let b = v.as_bytes();
        if b.is_empty() {
            return Err("a forwarded header is empty");
        }
        if b.len() > wire::FORWARDED_HEADER_MAX {
            return Err("a forwarded header is too long");
        }
        if !b.iter().all(|c| (0x20..0x7f).contains(c)) {
            return Err("a forwarded header is not printable ASCII");
        }
        Ok(std::str::from_utf8(b).ok())
    }
    let by = one(headers, crate::forward::FORWARDED_BY_HEADER, true)?.unwrap_or_default();
    if !by.bytes().all(|c| c.is_ascii_digit()) {
        return Err("mg-forwarded-by is not a node id");
    }
    let role = one(headers, wire::FORWARDED_ROLE_HEADER, true)?
        .and_then(Role::parse)
        .ok_or("mg-forwarded-role is not a role")?;
    let token_id = one(headers, wire::FORWARDED_TOKEN_ID_HEADER, true)?.unwrap_or_default();
    if token_id.trim() != token_id {
        return Err("mg-forwarded-token-id is malformed");
    }
    let action = match one(headers, wire::FORWARDED_ACTION_HEADER, false)? {
        None => None,
        Some("read") => Some(graph_authz::Action::Read),
        Some("search") => Some(graph_authz::Action::Search),
        Some(_) => return Err("mg-forwarded-action is not read or search"),
    };
    Ok(Forwarded {
        role,
        token_id: token_id.to_string(),
        action,
    })
}

/// `Admin.ReadIndex`: standalone (`status`), or a read barrier with the
/// `mg-forwarded-action` marker on a cluster-secret call (D7).
const READ_INDEX: &str = "/memory_graph.v1.Admin/ReadIndex";

#[cfg(unix)]
fn warn_if_readable_by_others(what: &str, path: &Path) {
    use std::os::unix::fs::MetadataExt;
    if let Ok(m) = std::fs::metadata(path) {
        if m.mode() & 0o077 != 0 {
            eprintln!(
                "memory-graph serve: WARNING: {what} `{}` is readable by group or others; \
                 restrict it (chmod 600)",
                path.display()
            );
            tracing::warn!(
                file = %path.display(),
                what,
                "readable by group or others; restrict it (chmod 600)"
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_readable_by_others(_what: &str, _path: &Path) {}

// ---------------------------------------------------------------------------
// The server's auth state.

/// `serve --auth-tokens` and `--cluster-secret-file`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthConfig {
    /// The token file; `None`: auth off.
    pub tokens: Option<PathBuf>,
    /// The cluster secret file (required with `tokens` in `--data-dir`
    /// mode; sent by every internal client).
    pub cluster_secret: Option<PathBuf>,
    /// `--auth-policies` (ADR 0013 D2): the policy file paired with the
    /// token file; `None`: ADR 0012 roles, no policy evaluated.
    pub policies: Option<PathBuf>,
    /// `--auth-builtins` (D4, Q1): with a policy file, also load the two
    /// built-in role policies.
    pub builtins: bool,
}

/// Refusal classes, as `mg_auth_refusals_total{reason}` labels.
pub const REASONS: [&str; 9] = [
    "missing",
    "malformed",
    "unknown",
    "expired",
    "role",
    "node_only",
    "secret_direct",
    "unlisted",
    // Not a refusal: an invalid credential on an open method (`Hello`,
    // health), which is answered anyway; counted so a client's typo
    // leaves a signal.
    "invalid_on_open",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    Missing,
    Malformed,
    Unknown,
    Expired,
    Role,
    NodeOnly,
    SecretDirect,
    Unlisted,
    InvalidOnOpen,
}

impl Reason {
    fn index(self) -> usize {
        self as usize
    }
}

/// A refused call.
#[derive(Debug)]
pub struct Refusal {
    reason: Reason,
    required: Need,
    /// A policy refusal (ADR 0013): the action denied. Not counted in
    /// `mg_auth_refusals_total` (its label set is ADR 0012's); audited on
    /// `mg::audit` when decided.
    policy: Option<graph_authz::Action>,
}

impl Refusal {
    fn unauth(reason: Reason) -> Self {
        Self {
            reason,
            required: Need::Open,
            policy: None,
        }
    }

    fn denied(reason: Reason, required: Need) -> Self {
        Self {
            reason,
            required,
            policy: None,
        }
    }

    fn policy(action: graph_authz::Action) -> Self {
        Self {
            reason: Reason::Role,
            required: Need::Write,
            policy: Some(action),
        }
    }

    /// The reason class (a metric label).
    pub fn reason(&self) -> &'static str {
        REASONS[self.reason.index()]
    }

    /// The error answered: `UNAUTHENTICATED` or `PERMISSION_DENIED`, with
    /// a fixed message.
    pub fn error(&self) -> StoreError {
        if let Some(a) = self.policy {
            return policy_denied(a);
        }
        match self.reason {
            Reason::Missing
            | Reason::Malformed
            | Reason::Unknown
            | Reason::Expired
            | Reason::NodeOnly => StoreError::Unauthenticated(UNAUTHENTICATED_MSG.into()),
            Reason::Role => StoreError::PermissionDenied {
                msg: format!("this method needs the `{}` role", self.required.as_str()),
                required_role: self.required.as_str().into(),
                action: String::new(),
            },
            Reason::SecretDirect => StoreError::PermissionDenied {
                msg: "the cluster secret is accepted here only on a forwarded call".into(),
                required_role: self.required.as_str().into(),
                action: String::new(),
            },
            Reason::Unlisted | Reason::InvalidOnOpen => StoreError::PermissionDenied {
                msg: "this method is not in the server's role table".into(),
                required_role: String::new(),
                action: String::new(),
            },
        }
    }

    pub fn status(&self) -> tonic::Status {
        graph_proto::store_error_to_status(&self.error())
    }
}

/// What the presented header was.
enum Cred<'a> {
    None,
    Malformed,
    Unknown,
    Expired,
    Token(&'a TokenEntry),
    Secret,
}

/// The node's auth state: the token set, the cluster secret, the refusal
/// counters and the log rate limiter.
pub struct Auth {
    tokens: Option<Source<AuthState>>,
    secret: Option<Source<SecretSet>>,
    refusals: [AtomicU64; REASONS.len()],
    limiter: Mutex<HashMap<Option<IpAddr>, (Instant, u64)>>,
    reload_lock: tokio::sync::Mutex<()>,
    authz: AuthzStats,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("enabled", &self.enabled())
            .field("cluster_secret", &self.secret.is_some())
            .finish_non_exhaustive()
    }
}

impl Auth {
    /// Auth off: every call passes, internal clients send no secret.
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            tokens: None,
            secret: None,
            refusals: Default::default(),
            limiter: Mutex::default(),
            reload_lock: tokio::sync::Mutex::new(()),
            authz: AuthzStats::default(),
        })
    }

    /// Load both files (refused at start on any problem). `clustered`: a
    /// `--data-dir` node, which needs the cluster secret when auth is on.
    pub fn load(cfg: &AuthConfig, clustered: bool) -> Result<Arc<Self>, StoreError> {
        let rejected = |flag: &str, e: String| StoreError::Rejected(format!("{flag} {e}"));
        if cfg.tokens.is_none() && cfg.policies.is_some() {
            return Err(StoreError::Rejected(
                "--auth-policies needs --auth-tokens (authentication is off without a token \
                 file)"
                    .into(),
            ));
        }
        if cfg.policies.is_none() && cfg.builtins {
            return Err(StoreError::Rejected(
                "--auth-builtins needs --auth-policies (without a policy file the built-in \
                 role policies are all there is)"
                    .into(),
            ));
        }
        let tokens = match (&cfg.tokens, &cfg.policies) {
            (Some(p), None) => Some(
                Source::load(
                    "auth token file",
                    &[p.as_path()],
                    text_parser(|t| {
                        parse_token_file(t).map(|tokens| AuthState {
                            tokens,
                            ..AuthState::default()
                        })
                    }),
                )
                .map_err(|e| rejected("--auth-tokens", e))?,
            ),
            (Some(p), Some(pol)) => {
                let builtins = cfg.builtins;
                Some(
                    Source::load(
                        "auth token and policy files",
                        &[p.as_path(), pol.as_path()],
                        Box::new(move |files: &[Vec<u8>]| {
                            let t = std::str::from_utf8(&files[0])
                                .map_err(|_| "token file: the file is not UTF-8".to_string())?;
                            parse_pair(t, &files[1], builtins)
                        }),
                    )
                    .map_err(|e| rejected("--auth-tokens/--auth-policies", e))?,
                )
            }
            (None, _) => None,
        };
        let secret = match &cfg.cluster_secret {
            Some(p) => Some(
                Source::load(
                    "cluster secret file",
                    &[p.as_path()],
                    text_parser(parse_secret_file),
                )
                .map_err(|e| rejected("--cluster-secret-file", e))?,
            ),
            None => None,
        };
        if tokens.is_none() && secret.is_some() {
            return Err(StoreError::Rejected(
                "--cluster-secret-file needs --auth-tokens (authentication is off without a \
                 token file)"
                    .into(),
            ));
        }
        if tokens.is_some() && secret.is_none() && clustered {
            return Err(StoreError::Rejected(
                "--auth-tokens on a --data-dir node needs --cluster-secret-file: every node of \
                 the cluster authenticates to the others with it (ADR 0012 D3)"
                    .into(),
            ));
        }
        Ok(Arc::new(Self {
            tokens,
            secret,
            refusals: Default::default(),
            limiter: Mutex::default(),
            reload_lock: tokio::sync::Mutex::new(()),
            authz: AuthzStats::default(),
        }))
    }

    /// Whether calls are checked (`--auth-tokens` given).
    pub fn enabled(&self) -> bool {
        self.tokens.is_some()
    }

    /// Re-read both files now; returns what happened to (tokens, secret).
    /// A file that cannot be read or validated keeps the loaded set.
    pub fn reload_now(&self) -> (Option<Reload>, Option<Reload>) {
        let tokens = self.tokens.as_ref().map(Source::reload);
        // `mg_authz_reload_total{result}` counts the token/policy pair only.
        if self.policies_on() {
            match tokens {
                Some(Reload::Reloaded) => {
                    self.authz.reloads[0].fetch_add(1, Ordering::Relaxed);
                }
                Some(Reload::Kept) => {
                    self.authz.reloads[1].fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            }
        }
        (tokens, self.secret.as_ref().map(Source::reload))
    }

    /// The reloader: every [`RELOAD_POLL`] and on SIGHUP (Unix), until
    /// `shutdown`. A poll and a signal that land together run one after
    /// the other (one lock), and identical content is a no-op.
    pub fn spawn_reloader(self: &Arc<Self>, shutdown: crate::ShutdownHandle) {
        if self.tokens.is_none() && self.secret.is_none() {
            return;
        }
        let me = Arc::clone(self);
        // Registered here, before the task is spawned, so the listener
        // exists by the time `serve` reports it is running. Registered
        // inside the task it could lag behind: a SIGHUP sent in that gap is
        // taken by tokio's process-wide handler (installed by any earlier
        // listener) and broadcast before this one subscribed, so it was lost.
        #[cfg(unix)]
        let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
        tokio::spawn(async move {
            loop {
                #[cfg(unix)]
                let sighup = async {
                    match hup.as_mut() {
                        Some(h) => {
                            h.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                };
                #[cfg(not(unix))]
                let sighup = std::future::pending::<()>();
                tokio::select! {
                    _ = shutdown.wait() => return,
                    _ = tokio::time::sleep(RELOAD_POLL) => {}
                    _ = sighup => tracing::info!("SIGHUP: reloading the auth files"),
                }
                let _one = me.reload_lock.lock().await;
                let me2 = Arc::clone(&me);
                let _ = tokio::task::spawn_blocking(move || me2.reload_now()).await;
            }
        });
    }

    /// The `authorization` value every internal client sends (the first
    /// line of the cluster secret file); `None` without one.
    pub fn secret_header(&self) -> Option<MetadataValue<Ascii>> {
        let s = self.secret.as_ref()?.cur.load();
        wire::bearer_value(&s.send)
    }

    /// The token set as loaded now (tests).
    pub fn tokens(&self) -> Option<Arc<TokenSet>> {
        self.tokens
            .as_ref()
            .map(|s| Arc::new(s.cur.load().tokens.clone()))
    }

    /// The token/policy pair as loaded now.
    pub fn state(&self) -> Option<Arc<AuthState>> {
        self.tokens.as_ref().map(|s| s.cur.load_full())
    }

    /// Whether policies are evaluated (`--auth-policies`).
    pub fn policies_on(&self) -> bool {
        self.tokens
            .as_ref()
            .is_some_and(|s| s.cur.load().policies.is_some())
    }

    /// The `policy_hash` reported by `Admin.Status` (D2); empty without
    /// `--auth-policies`.
    pub fn policy_hash(&self) -> String {
        self.tokens
            .as_ref()
            .map(|s| s.cur.load().policy_hash.clone())
            .unwrap_or_default()
    }

    /// Decide `action` on `res` for a client-token caller against the
    /// current pair (ADR 0013, PR 1). Allowed with policies off, inside an
    /// [`InProcessGuard`] (no caller: denied otherwise) and for a node caller (a forwarded call:
    /// until the leader re-check lands, PR 2, the leader keeps ADR 0012's
    /// `mg-forwarded-role` check). A token removed from the file, or
    /// expired, since the call started is `UNAUTHENTICATED`.
    pub fn authorize(
        &self,
        action: graph_authz::Action,
        res: graph_authz::Resource<'_>,
    ) -> Result<(), StoreError> {
        self.authorize_with(action, &res, |set, p| set.decide(p, action, &res))
    }

    /// An org-wide `prune` or `delete` (D6): on policy alone.
    pub fn authorize_org_wide(
        &self,
        action: graph_authz::Action,
        org: &str,
    ) -> Result<(), StoreError> {
        let res = graph_authz::Resource::Org(org);
        self.authorize_with(action, &res, |set, p| set.decide_org_wide(p, action, org))
    }

    fn authorize_with(
        &self,
        action: graph_authz::Action,
        res: &graph_authz::Resource<'_>,
        decide: impl FnOnce(
            &graph_authz::PolicySet,
            &graph_authz::Principal<'_>,
        ) -> graph_authz::Decision,
    ) -> Result<(), StoreError> {
        let Some(src) = self.tokens.as_ref() else {
            return Ok(());
        };
        let state = src.cur.load();
        let Some(set) = state.policies.as_ref() else {
            return Ok(());
        };
        let Some(caller) = current_caller() else {
            return if no_caller_allowed() {
                Ok(())
            } else {
                Err(policy_denied(action))
            };
        };
        let (e, role) = self.principal_entry(&state, &caller, action)?;
        let p = principal_with(e, role);
        let d = self.decide_timed(|| decide(set, &p));
        self.audit(e, &p, action, res, &d, caller.peer);
        if d.allowed {
            Ok(())
        } else {
            Err(policy_denied(action))
        }
    }

    /// The token entry and effective role a policy decision is made for
    /// (ADR 0013 D7): a client token's own entry; on a forwarded call the
    /// entry this node's token file has for `mg-forwarded-token-id` (an id
    /// it does not know, or that has expired here, is denied), with the role
    /// ceiling the intersection of this node's role for it and the
    /// forwarded role. Re-read from the current pair on every check, so a
    /// revocation or expiry takes effect at the next file of a stream.
    fn principal_entry<'s>(
        &self,
        state: &'s AuthState,
        caller: &Caller,
        action: graph_authz::Action,
    ) -> Result<(&'s TokenEntry, Role), StoreError> {
        let now = SystemTime::now();
        match &caller.kind {
            CallerKind::Token { id, .. } => match state.entry(id, now) {
                Some(e) => Ok((e, e.role)),
                None => Err(StoreError::Unauthenticated(UNAUTHENTICATED_MSG.into())),
            },
            CallerKind::Node {
                role: Some(r),
                forwarded_token_id: Some(id),
            } => match state.entry(id, now) {
                Some(e) => Ok((e, e.role.min(*r))),
                None => {
                    self.count_decision(action, false);
                    self.audit_unknown(action, caller.peer);
                    Err(policy_denied(action))
                }
            },
            // A node caller with no forwarded principal (the cluster secret
            // alone), or an anonymous one: never a scoped action (D7).
            _ => {
                self.count_decision(action, false);
                self.audit_unknown(action, caller.peer);
                Err(policy_denied(action))
            }
        }
    }

    /// The leader side of a forwarded call with `--auth-policies` (ADR 0013
    /// D7): strict header parsing, the principal from this node's own token
    /// file (groups never travel), the role ceiling as the intersection,
    /// and the cluster action decided here. The handler re-checks repo and
    /// org writes with the same principal ([`Self::principal_entry`]).
    #[allow(clippy::too_many_arguments)]
    fn check_forwarded(
        &self,
        state: &AuthState,
        set: &graph_authz::PolicySet,
        path: &str,
        headers: &http::HeaderMap,
        need: Need,
        peer: Option<IpAddr>,
        now: SystemTime,
    ) -> Result<CallerKind, Refusal> {
        let deny = |a: Option<graph_authz::Action>| match a {
            Some(a) => Refusal::policy(a),
            None => Refusal::denied(Reason::Role, need),
        };
        let fwd = match parse_forwarded(headers) {
            Ok(f) => f,
            Err(why) => {
                let a = method_action(path);
                if let Some(a) = a {
                    self.count_decision(a, false);
                }
                if self.audit_limited(peer) {
                    tracing::info!(
                        target: "mg::audit",
                        action = a.map_or("", graph_authz::Action::name),
                        decision = "deny",
                        reason = why,
                        "authz: forwarded headers refused"
                    );
                }
                return Err(deny(a));
            }
        };
        // `ReadIndex` with the marker is the barrier of a read: admitted for
        // any principal whose role covers `read` (D1, D7), scoped by the
        // read itself. Without it, standalone: `status`.
        let (action, decide_here) = if path == READ_INDEX {
            match fwd.action {
                Some(a) => (Some(a), None),
                None => (
                    Some(graph_authz::Action::Status),
                    Some(graph_authz::Action::Status),
                ),
            }
        } else {
            (method_action(path), cluster_action(path))
        };
        let Some(e) = state.entry(&fwd.token_id, now) else {
            if let Some(a) = action {
                self.count_decision(a, false);
                self.audit_unknown(a, peer);
            }
            return Err(deny(action));
        };
        let role = e.role.min(fwd.role);
        let required = if need == Need::Read {
            Role::Read
        } else {
            Role::Write
        };
        if !role.covers(required) {
            return Err(Refusal::denied(Reason::Role, need));
        }
        if let Some(a) = decide_here {
            let p = principal_with(e, role);
            let d = self.decide_timed(|| set.decide(&p, a, &graph_authz::Resource::Cluster));
            self.audit(e, &p, a, &graph_authz::Resource::Cluster, &d, peer);
            if !d.allowed {
                return Err(Refusal::policy(a));
            }
        }
        Ok(CallerKind::Node {
            role: Some(role),
            forwarded_token_id: Some(fwd.token_id),
        })
    }

    /// Run one decision, timed into `mg_authz_decision_seconds` and counted
    /// into `mg_authz_decisions_total`.
    fn decide_timed(&self, f: impl FnOnce() -> graph_authz::Decision) -> graph_authz::Decision {
        let t = Instant::now();
        let d = f();
        let secs = t.elapsed().as_secs_f64();
        self.authz
            .seconds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .observe(secs);
        if let Some(o) = self.authz.otlp.get() {
            o.record_authz(secs);
        }
        d
    }

    fn count_decision(&self, action: graph_authz::Action, allowed: bool) {
        self.authz.decisions[action as usize][usize::from(allowed)].fetch_add(1, Ordering::Relaxed);
    }

    /// Whether a denial line from `peer` may be logged now (ADR 0012's
    /// per-peer window); suppressed lines are counted into the next one.
    fn audit_limited(&self, peer: Option<IpAddr>) -> bool {
        let now = Instant::now();
        let mut g = self
            .authz
            .limiter
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if g.len() > 10_000 {
            g.retain(|_, (at, _)| now.duration_since(*at) < REFUSAL_LOG_WINDOW);
        }
        match g.get_mut(&peer) {
            Some((at, n)) if now.duration_since(*at) < REFUSAL_LOG_WINDOW => {
                *n += 1;
                false
            }
            Some(e) => {
                *e = (now, 0);
                true
            }
            None => {
                g.insert(peer, (now, 0));
                true
            }
        }
    }

    /// The audit line of a decision (D9, target `mg::audit`): token id,
    /// groups, action, resource, decision and the matched `@id`s; never a
    /// token, a digest or policy text. Denials at `info` (rate-limited per
    /// peer), allowed writes and cluster actions at `info`, allowed reads at
    /// `debug`. Also records `authz.action`/`authz.decision` on the current
    /// span and counts the decision.
    fn audit(
        &self,
        e: &TokenEntry,
        p: &graph_authz::Principal<'_>,
        action: graph_authz::Action,
        res: &graph_authz::Resource<'_>,
        d: &graph_authz::Decision,
        peer: Option<IpAddr>,
    ) {
        use graph_authz::Action as A;
        self.count_decision(action, d.allowed);
        let decision = if d.allowed { "allow" } else { "deny" };
        let span = tracing::Span::current();
        span.record("authz.action", action.name());
        span.record("authz.decision", decision);
        let role = p.role.name();
        if d.allowed && matches!(action, A::Read | A::Search) {
            tracing::debug!(target: "mg::audit", token_id = %e.id, groups = ?e.groups, role, action = action.name(), resource = %res, decision, matched = ?d.matched, "authz");
        } else if d.allowed || self.audit_limited(peer) {
            tracing::info!(target: "mg::audit", token_id = %e.id, groups = ?e.groups, role, action = action.name(), resource = %res, decision, matched = ?d.matched, "authz");
        }
    }

    /// The audit line of a forwarded call whose principal this node does
    /// not know (D7): no token id is echoed (it came from the wire).
    fn audit_unknown(&self, action: graph_authz::Action, peer: Option<IpAddr>) {
        let span = tracing::Span::current();
        span.record("authz.action", action.name());
        span.record("authz.decision", "deny");
        if self.audit_limited(peer) {
            tracing::info!(
                target: "mg::audit",
                action = action.name(),
                decision = "deny",
                reason = "no known principal",
                "authz"
            );
        }
    }

    /// `mg_authz_decisions_total{action,decision}`, every pair.
    pub fn authz_decisions(&self) -> Vec<(&'static str, &'static str, u64)> {
        let mut out = Vec::new();
        for a in graph_authz::Action::ALL {
            for (i, d) in ["deny", "allow"].iter().enumerate() {
                out.push((
                    a.name(),
                    *d,
                    self.authz.decisions[a as usize][i].load(Ordering::Relaxed),
                ));
            }
        }
        out
    }

    /// `mg_authz_policies`: policies in the loaded set (0 without
    /// `--auth-policies`).
    pub fn authz_policies(&self) -> u64 {
        self.tokens
            .as_ref()
            .and_then(|s| s.cur.load().policies.as_ref().map(|p| p.len() as u64))
            .unwrap_or(0)
    }

    /// `mg_authz_reload_total{result}`: `ok` (a new pair swapped in) and
    /// `error` (a pair that failed to read or validate; the last good one
    /// stays). Zero without `--auth-policies`.
    pub fn authz_reloads(&self) -> [(&'static str, u64); 2] {
        [
            ("ok", self.authz.reloads[0].load(Ordering::Relaxed)),
            ("error", self.authz.reloads[1].load(Ordering::Relaxed)),
        ]
    }

    /// `mg_authz_decision_seconds`.
    pub fn authz_seconds(&self) -> crate::observe::Histogram {
        self.authz
            .seconds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Record `mg_authz_decision_seconds` over OTLP too (ADR 0009 D6).
    pub fn set_otlp(&self, r: crate::telemetry::OtlpRecorders) {
        let _ = self.authz.otlp.set(r);
    }

    /// The PR 1 interim read rule: whether the caller may run reads of
    /// `action` at all (an unconstrained read permit and no read forbid;
    /// the load rule makes every read policy resource-unconstrained, so the
    /// decision on `Cluster::"this"` is the decision on every resource).
    /// `true` with policies off, in-process and for node callers.
    pub fn read_allowed(&self, action: graph_authz::Action) -> bool {
        let Some(src) = self.tokens.as_ref() else {
            return true;
        };
        let state = src.cur.load();
        let Some(set) = state.policies.as_ref() else {
            return true;
        };
        let Some(caller) = current_caller() else {
            return no_caller_allowed();
        };
        // A forwarded read (`ExtractorGaps`) is decided for the principal
        // this node resolves (D7); the cluster secret alone reads nothing.
        let Ok((e, role)) = self.principal_entry(&state, &caller, action) else {
            return false;
        };
        let p = principal_with(e, role);
        let res = graph_authz::Resource::Cluster;
        let d = self.decide_timed(|| set.decide(&p, action, &res));
        self.audit(e, &p, action, &res, &d, caller.peer);
        d.allowed
    }

    /// The caller of an MCP HTTP request, for its tool's in-process service
    /// calls (ADR 0013 PR 2): the token in `headers` as a client-token
    /// caller, so every policy check and every forward to the leader treat
    /// the tool call exactly as a gRPC call of that token. `None` with auth
    /// off or without a valid token (the endpoint answers 401 first).
    pub fn mcp_caller(&self, headers: &http::HeaderMap) -> Option<Caller> {
        let state = self.tokens.as_ref()?.cur.load();
        let raw = headers
            .get(http::header::AUTHORIZATION)
            .map(|v| v.as_bytes());
        match self.credential(raw, &state.tokens, None, SystemTime::now()) {
            Cred::Token(e) => Some(Caller {
                kind: CallerKind::Token {
                    id: Arc::clone(&e.id),
                    role: e.role,
                },
                conn: 0,
                peer: None,
            }),
            _ => None,
        }
    }

    /// With `--auth-policies`, decide an MCP read tool call for the token in
    /// `headers` under the PR 1 interim read rule (all or nothing, on
    /// `Cluster::"this"`) until the read tools are scoped (PR 3). The write
    /// tools (`index_path`, `prune`) are decided per repo by the Write
    /// service they call, with the token as the caller (PR 2): `None` here.
    /// `None`: allowed, or policies off.
    pub fn mcp_tool_denied(&self, headers: &http::HeaderMap, tool: &str) -> Option<StoreError> {
        use graph_authz::Action as A;
        let state = self.tokens.as_ref()?.cur.load();
        let set = state.policies.as_ref()?;
        let action = match tool {
            "search" | "find_symbols" => A::Search,
            t if graph_mcp::is_write_tool(t) => return None,
            _ => A::Read,
        };
        let raw = headers
            .get(http::header::AUTHORIZATION)
            .map(|v| v.as_bytes());
        let Cred::Token(e) = self.credential(raw, &state.tokens, None, SystemTime::now()) else {
            return Some(StoreError::Unauthenticated(UNAUTHENTICATED_MSG.into()));
        };
        let p = principal_of(e);
        let res = graph_authz::Resource::Cluster;
        let d = self.decide_timed(|| set.decide(&p, action, &res));
        self.audit(e, &p, action, &res, &d, None);
        (!d.allowed).then(|| policy_denied(action))
    }

    fn credential<'a>(
        &self,
        raw: Option<&[u8]>,
        tokens: &'a TokenSet,
        secret: Option<&SecretSet>,
        now: SystemTime,
    ) -> Cred<'a> {
        let Some(raw) = raw else {
            return Cred::None;
        };
        let Ok((kind, value)) = wire::parse_bearer(raw) else {
            return Cred::Malformed;
        };
        let d = wire::digest(value);
        match kind {
            CredKind::Token => match tokens.lookup(&d, now) {
                Lookup::Found(e) => Cred::Token(e),
                Lookup::Expired => Cred::Expired,
                Lookup::Unknown => Cred::Unknown,
            },
            CredKind::Secret => match secret {
                Some(s) if ct_find(s.digests.iter().copied(), &d).is_some() => Cred::Secret,
                _ => Cred::Unknown,
            },
        }
    }

    /// Decide one call to `path` with these request headers (auth on).
    pub fn check(
        &self,
        path: &str,
        headers: &http::HeaderMap,
        conn: u64,
        now: SystemTime,
    ) -> Result<Caller, Refusal> {
        self.check_from(path, headers, conn, None, now)
    }

    /// [`check`](Self::check) for a call from `peer`.
    pub fn check_from(
        &self,
        path: &str,
        headers: &http::HeaderMap,
        conn: u64,
        peer: Option<IpAddr>,
        now: SystemTime,
    ) -> Result<Caller, Refusal> {
        let Some(tokens) = self.tokens.as_ref() else {
            return Ok(Caller {
                kind: CallerKind::Anonymous,
                conn,
                peer,
            });
        };
        let Some(need) = need(path) else {
            return Err(Refusal::denied(Reason::Unlisted, Need::Open));
        };
        // One snapshot of each set for the whole decision.
        let tokens = tokens.cur.load();
        let secret = self.secret.as_ref().map(|s| s.cur.load());
        let raw = headers
            .get(http::header::AUTHORIZATION)
            .map(|v| v.as_bytes());
        let cred = self.credential(raw, &tokens.tokens, secret.as_ref().map(|s| &***s), now);
        let caller = |kind| Caller { kind, conn, peer };
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let unauth = |c: &Cred<'_>| {
            Refusal::unauth(match c {
                Cred::None => Reason::Missing,
                Cred::Malformed => Reason::Malformed,
                Cred::Expired => Reason::Expired,
                _ => Reason::Unknown,
            })
        };
        match need {
            Need::Open => Ok(caller(match cred {
                Cred::Malformed | Cred::Unknown | Cred::Expired => {
                    self.refusals[Reason::InvalidOnOpen.index()].fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(
                        method = path,
                        "auth: an invalid credential on an open method; answered as anonymous"
                    );
                    CallerKind::Anonymous
                }
                Cred::Token(e) => CallerKind::Token {
                    id: Arc::clone(&e.id),
                    role: e.role,
                },
                Cred::Secret => CallerKind::Node {
                    role: None,
                    forwarded_token_id: None,
                },
                _ => CallerKind::Anonymous,
            })),
            Need::Node => match cred {
                Cred::Secret => Ok(caller(CallerKind::Node {
                    role: None,
                    forwarded_token_id: None,
                })),
                // D3: a client token never authorises a node method.
                Cred::Token(_) => Err(Refusal::unauth(Reason::NodeOnly)),
                c => Err(unauth(&c)),
            },
            Need::Read | Need::Write => {
                let required = if need == Need::Read {
                    Role::Read
                } else {
                    Role::Write
                };
                match cred {
                    // A client token's forwarding headers are ignored.
                    Cred::Token(e) if e.role.covers(required) => {
                        // ADR 0013: with a policy file, a cluster action is
                        // decided here, on `Cluster::"this"`; writes on a
                        // repo or org are decided by their handler.
                        if let (Some(set), Some(action)) =
                            (tokens.policies.as_ref(), cluster_action(path))
                        {
                            let p = principal_of(e);
                            let d = self.decide_timed(|| {
                                set.decide(&p, action, &graph_authz::Resource::Cluster)
                            });
                            self.audit(e, &p, action, &graph_authz::Resource::Cluster, &d, peer);
                            if !d.allowed {
                                return Err(Refusal::policy(action));
                            }
                        }
                        Ok(caller(CallerKind::Token {
                            id: Arc::clone(&e.id),
                            role: e.role,
                        }))
                    }
                    Cred::Token(_) => Err(Refusal::denied(Reason::Role, need)),
                    // The join probes (no `mg-forwarded-by`). With a policy
                    // file, a forwarded call is never one of them (D7).
                    Cred::Secret
                        if need == Need::Read
                            && secret_reads(path)
                            && !(tokens.policies.is_some()
                                && headers.contains_key(crate::forward::FORWARDED_BY_HEADER)) =>
                    {
                        Ok(caller(CallerKind::Node {
                            role: None,
                            forwarded_token_id: None,
                        }))
                    }
                    Cred::Secret => {
                        if !headers.contains_key(crate::forward::FORWARDED_BY_HEADER) {
                            return Err(Refusal::denied(Reason::SecretDirect, need));
                        }
                        if let Some(set) = tokens.policies.as_ref() {
                            return self
                                .check_forwarded(&tokens, set, path, headers, need, peer, now)
                                .map(caller);
                        }
                        if header(crate::forward::FORWARDED_BY_HEADER).is_none() {
                            return Err(Refusal::denied(Reason::SecretDirect, need));
                        }
                        match header(wire::FORWARDED_ROLE_HEADER)
                            .and_then(|r: String| Role::parse(&r))
                        {
                            Some(r) if r.covers(required) => Ok(caller(CallerKind::Node {
                                role: Some(r),
                                forwarded_token_id: header(wire::FORWARDED_TOKEN_ID_HEADER),
                            })),
                            _ => Err(Refusal::denied(Reason::Role, need)),
                        }
                    }
                    c => Err(unauth(&c)),
                }
            }
        }
    }

    /// Count a refusal, and log it at most once per peer per
    /// [`REFUSAL_LOG_WINDOW`] (with the number suppressed since).
    fn note_refusal(&self, r: &Refusal, peer: Option<std::net::SocketAddr>, path: &str) {
        if r.policy.is_some() {
            return;
        }
        self.refusals[r.reason.index()].fetch_add(1, Ordering::Relaxed);
        let ip = peer.map(|p| p.ip());
        let now = Instant::now();
        let suppressed = {
            let mut g = self.limiter.lock().unwrap_or_else(PoisonError::into_inner);
            if g.len() > 10_000 {
                g.retain(|_, (at, _)| now.duration_since(*at) < REFUSAL_LOG_WINDOW);
            }
            match g.get_mut(&ip) {
                Some((at, n)) if now.duration_since(*at) < REFUSAL_LOG_WINDOW => {
                    *n += 1;
                    return;
                }
                Some(e) => {
                    let n = e.1;
                    *e = (now, 0);
                    n
                }
                None => {
                    g.insert(ip, (now, 0));
                    0
                }
            }
        };
        // The method path is one of the known RPCs or a short label: a
        // client-chosen path is not logged verbatim.
        let method = if need(path).is_some() {
            path
        } else {
            "<unlisted>"
        };
        tracing::warn!(
            peer = ?peer,
            method,
            reason = r.reason(),
            suppressed,
            "auth: call refused"
        );
    }

    /// `mg_auth_refusals_total{reason}`.
    pub fn refusal_counts(&self) -> Vec<(&'static str, u64)> {
        REASONS
            .iter()
            .zip(&self.refusals)
            .map(|(r, n)| (*r, n.load(Ordering::Relaxed)))
            .collect()
    }

    /// Check an MCP HTTP request (D9): the role of a valid `read` or
    /// `write` token, or `None` (answer 401). The cluster secret is not a
    /// client credential here.
    pub fn mcp_role(&self, headers: &http::HeaderMap) -> Option<Role> {
        let tokens = self.tokens.as_ref()?.cur.load();
        let raw = headers
            .get(http::header::AUTHORIZATION)
            .map(|v| v.as_bytes());
        match self.credential(raw, &tokens.tokens, None, SystemTime::now()) {
            Cred::Token(e) => Some(e.role),
            c => {
                let reason = match c {
                    Cred::None => Reason::Missing,
                    Cred::Malformed => Reason::Malformed,
                    Cred::Expired => Reason::Expired,
                    _ => Reason::Unknown,
                };
                self.refusals[reason.index()].fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }
}

thread_local! {
    /// Set while this thread runs a deliberately in-process call (the MCP
    /// endpoint's tool calls, which are decided for their token before
    /// they run): the only calls that reach a policy check with no caller.
    static IN_PROCESS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks the current thread as running in-process calls until dropped
/// (ADR 0013 PR 1). The MCP endpoint enters it around a tool call it has
/// already decided ([`Auth::mcp_tool_denied`]); nothing else may.
pub struct InProcessGuard(bool);

impl InProcessGuard {
    pub fn enter() -> Self {
        InProcessGuard(IN_PROCESS.with(|c| c.replace(true)))
    }
}

impl Drop for InProcessGuard {
    fn drop(&mut self) {
        let prev = self.0;
        IN_PROCESS.with(|c| c.set(prev));
    }
}

/// With policies on, a policy check that finds no caller fails closed,
/// except inside an [`InProcessGuard`]. Every gRPC call runs with its
/// caller set by the auth layer, so reaching this otherwise is a bug
/// (debug builds assert).
fn no_caller_allowed() -> bool {
    let ok = IN_PROCESS.with(|c| c.get());
    if !ok {
        tracing::error!("authz: a policy check with no caller outside the in-process path; denied");
        debug_assert!(
            ok,
            "a policy check with no caller outside the in-process path"
        );
    }
    ok
}

/// The policy principal of a token entry.
fn principal_of(e: &TokenEntry) -> graph_authz::Principal<'_> {
    graph_authz::Principal {
        token: &e.id,
        groups: &e.groups,
        role: match e.role {
            Role::Read => graph_authz::Role::Read,
            Role::Write => graph_authz::Role::Write,
        },
    }
}

/// The principal of a token entry acting with `role` (its own role, or on
/// a forwarded call the intersection with the forwarded role, D7).
fn principal_with(e: &TokenEntry, role: Role) -> graph_authz::Principal<'_> {
    graph_authz::Principal {
        role: match role {
            Role::Read => graph_authz::Role::Read,
            Role::Write => graph_authz::Role::Write,
        },
        ..principal_of(e)
    }
}

/// The decision counters and timings behind the `mg_authz_*` metrics
/// (ADR 0013 D9): no per-token, group, resource or policy label.
#[derive(Default)]
struct AuthzStats {
    /// `[action][deny=0 | allow=1]`.
    decisions: [[AtomicU64; 2]; 8],
    /// `[ok, error]` reloads of the token/policy pair.
    reloads: [AtomicU64; 2],
    seconds: Mutex<crate::observe::Histogram>,
    otlp: std::sync::OnceLock<crate::telemetry::OtlpRecorders>,
    /// The per-peer window of audit denial lines.
    limiter: Mutex<HashMap<Option<IpAddr>, (Instant, u64)>>,
}

/// Stamp the cluster secret (when this node has one) on outgoing metadata.
pub fn stamp_secret(auth: &Auth, md: &mut MetadataMap) {
    if let Some(v) = auth.secret_header() {
        md.insert(wire::AUTHORIZATION, v);
    }
}

/// The interceptor of a node's own calls to another node (join probes,
/// `Join`, `UpdateAdvertise`, the leader's `Status` probe): the protocol
/// version, the trace context and the cluster secret.
#[derive(Clone)]
pub struct NodeHeaders {
    pub auth: Arc<Auth>,
    /// Mark the call as forwarded by this node id with role `write` (a
    /// node-originated membership change, `--bootstrap-or-join`'s
    /// `Remove`).
    pub forwarded_by: Option<u64>,
}

impl NodeHeaders {
    pub fn new(auth: &Arc<Auth>) -> Self {
        Self {
            auth: Arc::clone(auth),
            forwarded_by: None,
        }
    }
}

impl tonic::service::Interceptor for NodeHeaders {
    fn call(&mut self, req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        let mut req = graph_proto::SendVersion.call(req)?;
        let md = req.metadata_mut();
        stamp_secret(&self.auth, md);
        if let Some(me) = self.forwarded_by {
            md.insert(crate::forward::FORWARDED_BY_HEADER, MetadataValue::from(me));
            md.insert(
                wire::FORWARDED_ROLE_HEADER,
                MetadataValue::from_static(Role::Write.as_str()),
            );
        }
        Ok(req)
    }
}

// ---------------------------------------------------------------------------
// The tower layer.

/// The auth layer (D1): wraps the whole router.
#[derive(Clone)]
pub struct AuthLayer {
    auth: Arc<Auth>,
}

impl AuthLayer {
    pub fn new(auth: Arc<Auth>) -> Self {
        Self { auth }
    }
}

impl<S> tower_layer::Layer<S> for AuthLayer {
    type Service = AuthService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        AuthService {
            inner,
            auth: Arc::clone(&self.auth),
        }
    }
}

#[derive(Clone)]
pub struct AuthService<S> {
    inner: S,
    auth: Arc<Auth>,
}

type BoxFut<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send>>;

impl<S, B, RB> tower_service::Service<http::Request<B>> for AuthService<S>
where
    S: tower_service::Service<http::Request<B>, Response = http::Response<RB>>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    B: Send + 'static,
    RB: Default + Send + 'static,
{
    type Response = http::Response<RB>;
    type Error = S::Error;
    type Future = BoxFut<Self::Response, Self::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        if !self.auth.enabled() {
            return Box::pin(inner.call(req));
        }
        let conn = req.extensions().get::<ConnInfo>().cloned();
        let path = req.uri().path().to_string();
        match self.auth.check_from(
            &path,
            req.headers(),
            conn.as_ref().map_or(0, |c| c.id),
            conn.as_ref().and_then(|c| c.peer).map(|p| p.ip()),
            SystemTime::now(),
        ) {
            Ok(caller) => {
                if let Some(id) = caller.token_id() {
                    tracing::debug!(token_id = %id, role = ?caller.role(), "auth: accepted");
                }
                // OTel/log span fields: the token id and role only (D8).
                let span = tracing::Span::current();
                if let Some(id) = caller.token_id() {
                    span.record("auth.token_id", id.as_str());
                }
                if let Some(r) = caller.role() {
                    span.record("auth.role", r.as_str());
                }
                req.extensions_mut().insert(caller.clone());
                Box::pin(CALLER.scope(caller, inner.call(req)))
            }
            Err(refusal) => {
                self.auth
                    .note_refusal(&refusal, conn.and_then(|c| c.peer), &path);
                let resp = refusal.status().into_http::<RB>();
                Box::pin(std::future::ready(Ok(resp)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(n: u8) -> String {
        format!("mgt_{}", "A".repeat(42) + &((b'A' + n) as char).to_string())
    }

    fn file(entries: &[(&str, &str, &str)]) -> String {
        entries
            .iter()
            .map(|(id, role, t)| {
                format!(
                    "[[token]]\nid = \"{id}\"\nrole = \"{role}\"\nsha256 = \"{}\"\n",
                    wire::digest_hex(t)
                )
            })
            .collect()
    }

    #[test]
    fn every_rpc_is_classified() {
        for p in graph_proto::rpc_paths() {
            assert!(
                need(p).is_some(),
                "{p} is not in the role table (ADR 0012 D5)"
            );
        }
        assert_eq!(need("/memory_graph.v1.Store/Nope"), None);
        assert_eq!(need("/x.y/Z"), None);
        assert_eq!(need("/grpc.health.v1.Health/Check"), Some(Need::Open));
        assert_eq!(need("/memory_graph.v1.Store/Hello"), Some(Need::Open));
        assert_eq!(need("/memory_graph.v1.Raft/Vote"), Some(Need::Node));
        assert_eq!(need("/memory_graph.v1.Write/Index"), Some(Need::Write));
        assert_eq!(need("/memory_graph.v1.Admin/ReadIndex"), Some(Need::Read));
    }

    #[test]
    fn the_token_file_is_validated_and_errors_never_quote_a_digest() {
        let t = token(1);
        let ok = file(&[("ci", "write", &t), ("bot", "read", &token(2))]);
        let set = parse_token_file(&ok).unwrap();
        assert_eq!(set.entries.len(), 2);
        let digest = wire::digest_hex(&t);
        let cases = [
            (format!("{ok}scope = []\n"), "scope"),
            (ok.replace("\"read\"", "\"admin\""), "role"),
            (
                file(&[("a", "read", &t), ("a", "read", &token(3))]),
                "duplicate `id`",
            ),
            (
                file(&[("a", "read", &t), ("b", "read", &t)]),
                "duplicate `sha256`",
            ),
            (
                ok.replacen(&digest, &digest.replace('0', "g"), 1),
                "64 hexadecimal",
            ),
            (ok.replacen(&digest, &digest[..62], 1), "64 hexadecimal"),
            (format!("{ok}colour = \"x\"\n"), "unknown field"),
            (String::new(), "no [[token]]"),
            ("# nothing\n".to_string(), "no [[token]]"),
            (format!("{ok}expires = \"tomorrow\"\n"), "expires"),
            (
                format!("[[token]]\nid = \"x\"\nrole = \"read\"\nsha256 = \"{digest}"),
                "line",
            ),
        ];
        for (text, want) in cases {
            let e = parse_token_file(&text).unwrap_err();
            assert!(e.contains(want), "{want}: {e}");
            assert!(e.contains("line") || want.contains("token"), "{e}");
            assert!(!e.contains(&digest) && !e.contains(&digest[..20]), "{e}");
        }
    }

    #[test]
    fn rfc3339() {
        let t = parse_rfc3339("1970-01-01T00:00:10Z").unwrap();
        assert_eq!(t, SystemTime::UNIX_EPOCH + Duration::from_secs(10));
        let t = parse_rfc3339("2027-01-01T01:00:00+01:00").unwrap();
        assert_eq!(t, parse_rfc3339("2027-01-01T00:00:00Z").unwrap());
        assert_eq!(
            parse_rfc3339("2000-03-01T00:00:00.5Z").unwrap(),
            SystemTime::UNIX_EPOCH + Duration::from_millis(951_868_800_500)
        );
        for bad in [
            "",
            "2027-01-01",
            "2027-13-01T00:00:00Z",
            "2027-01-01T00:00:00",
        ] {
            assert!(parse_rfc3339(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_secret_file_holds_one_or_two_secrets() {
        let a = new_credential(wire::SECRET_PREFIX);
        let b = new_credential(wire::SECRET_PREFIX);
        let s = parse_secret_file(&format!("# c\n{}\n\n{}\n", *a, *b)).unwrap();
        assert_eq!(s.digests.len(), 2);
        assert_eq!(*s.send, *a);
        assert!(parse_secret_file("").is_err());
        assert!(parse_secret_file(&format!("{}\n{}\n{}\n", *a, *b, *a)).is_err());
        let e = parse_secret_file(&token(1)).unwrap_err();
        assert!(!e.contains(&token(1)), "{e}");
        assert!(!format!("{s:?}").contains(&a[4..]));
    }

    #[test]
    fn new_credentials_have_the_format() {
        let t = new_credential(wire::TOKEN_PREFIX);
        assert_eq!(wire::check_format(&t), Some(CredKind::Token));
        assert_ne!(*t, *new_credential(wire::TOKEN_PREFIX));
    }

    fn auth_with(tokens: &str, secret: Option<&str>) -> (Arc<Auth>, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let tp = d.path().join("tokens.toml");
        std::fs::write(&tp, tokens).unwrap();
        let sp = secret.map(|s| {
            let p = d.path().join("secret");
            std::fs::write(&p, s).unwrap();
            p
        });
        let a = Auth::load(
            &AuthConfig {
                tokens: Some(tp),
                cluster_secret: sp,
                ..AuthConfig::default()
            },
            false,
        )
        .unwrap();
        (a, d)
    }

    fn hdrs(pairs: &[(&str, String)]) -> http::HeaderMap {
        let mut h = http::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    fn bearer(v: &str) -> (&'static str, String) {
        ("authorization", format!("Bearer {v}"))
    }

    #[test]
    fn decisions_follow_the_role_table() {
        let (w, r) = (token(1), token(2));
        let secret = new_credential(wire::SECRET_PREFIX);
        let (a, _d) = auth_with(
            &file(&[("w", "write", &w), ("r", "read", &r)]),
            Some(&secret),
        );
        let now = SystemTime::now();
        let check = |p: &str, h: &[(&str, String)]| {
            a.check(p, &hdrs(h), 1, now).map_err(|e| e.status().code())
        };
        use tonic::Code;
        let idx = "/memory_graph.v1.Write/Index";
        let get = "/memory_graph.v1.Store/Get";
        let vote = "/memory_graph.v1.Raft/Vote";
        assert!(check(idx, &[bearer(&w)]).is_ok());
        assert_eq!(check(idx, &[bearer(&r)]), Err(Code::PermissionDenied));
        assert!(check(get, &[bearer(&r)]).is_ok());
        assert_eq!(check(get, &[]), Err(Code::Unauthenticated));
        assert_eq!(check(get, &[bearer(&token(9))]), Err(Code::Unauthenticated));
        assert_eq!(
            check(get, &[("authorization", "Basic x".into())]),
            Err(Code::Unauthenticated)
        );
        // Node methods: the secret only; a client token is UNAUTHENTICATED.
        assert!(check(vote, &[bearer(&secret)]).is_ok());
        assert_eq!(check(vote, &[bearer(&w)]), Err(Code::Unauthenticated));
        // The secret: join probes yes, a direct write no, a forward yes.
        assert!(check("/memory_graph.v1.Admin/Status", &[bearer(&secret)]).is_ok());
        assert_eq!(check(idx, &[bearer(&secret)]), Err(Code::PermissionDenied));
        let fwd = |role: &str| {
            vec![
                bearer(&secret),
                ("mg-forwarded-by", "2".into()),
                ("mg-forwarded-role", role.into()),
            ]
        };
        assert!(check(idx, &fwd("write")).is_ok());
        assert_eq!(check(idx, &fwd("read")), Err(Code::PermissionDenied));
        assert!(check(get, &fwd("read")).is_ok());
        // A client token's forwarding headers are ignored.
        let spoof = vec![
            bearer(&r),
            ("mg-forwarded-by", "2".into()),
            ("mg-forwarded-role", "write".into()),
        ];
        assert_eq!(check(idx, &spoof), Err(Code::PermissionDenied));
        assert_eq!(
            check(idx, &spoof[1..]),
            Err(Code::Unauthenticated),
            "no credential at all"
        );
        // Unlisted: refused for every credential.
        for h in [vec![], vec![bearer(&w)], vec![bearer(&secret)]] {
            assert_eq!(
                check("/memory_graph.v1.Store/Nope", &h),
                Err(Code::PermissionDenied)
            );
        }
        // Open methods never refuse, and say who called.
        let c = check("/memory_graph.v1.Store/Hello", &[bearer(&token(9))]).unwrap();
        assert!(!c.authenticated());
        let c = check("/memory_graph.v1.Store/Hello", &[bearer(&w)]).unwrap();
        assert_eq!(c.role(), Some(Role::Write));
    }

    #[test]
    fn expiry_is_checked_on_every_call() {
        let t = token(1);
        let text = format!(
            "{}expires = \"2030-01-01T00:00:00Z\"\n",
            file(&[("x", "read", &t)])
        );
        let (a, _d) = auth_with(&text, None);
        let at = parse_rfc3339("2030-01-01T00:00:00Z").unwrap();
        let get = "/memory_graph.v1.Store/Get";
        let h = hdrs(&[bearer(&t)]);
        assert!(a.check(get, &h, 1, at - Duration::from_secs(1)).is_ok());
        assert_eq!(a.check(get, &h, 1, at).unwrap_err().reason(), "expired");
        assert!(a.check(get, &h, 1, at + Duration::from_secs(1)).is_err());
    }

    #[test]
    fn a_reload_swaps_or_keeps_the_set() {
        let (old, new) = (token(1), token(2));
        let (a, d) = auth_with(&file(&[("old", "read", &old)]), None);
        let p = d.path().join("tokens.toml");
        let get = "/memory_graph.v1.Store/Get";
        let ok = |t: &str| {
            a.check(get, &hdrs(&[bearer(t)]), 1, SystemTime::now())
                .is_ok()
        };
        assert_eq!(a.reload_now().0, Some(Reload::Unchanged));
        // Both at once during a rotation.
        std::fs::write(&p, file(&[("old", "read", &old), ("new", "read", &new)])).unwrap();
        assert_eq!(a.reload_now().0, Some(Reload::Reloaded));
        assert!(ok(&old) && ok(&new));
        // Removed: refused on the next call.
        std::fs::write(&p, file(&[("new", "read", &new)])).unwrap();
        a.reload_now();
        assert!(!ok(&old) && ok(&new));
        // Half-written, invalid, deleted: the old set stays.
        let full = file(&[("x", "read", &old)]);
        std::fs::write(&p, &full[..full.len() / 2]).unwrap();
        assert_eq!(a.reload_now().0, Some(Reload::Kept));
        std::fs::write(&p, format!("{full}scope = []\n")).unwrap();
        assert_eq!(a.reload_now().0, Some(Reload::Kept));
        std::fs::remove_file(&p).unwrap();
        assert_eq!(a.reload_now().0, Some(Reload::Kept));
        assert!(!ok(&old) && ok(&new));
    }

    #[test]
    fn groups_in_the_token_file() {
        let t = token(1);
        let base = file(&[("ci", "write", &t)]);
        let set = parse_token_file(&format!("{base}groups = [\"a\", \"b\", \"a\"]\n")).unwrap();
        assert_eq!(
            set.entries[0].groups,
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(parse_token_file(&base).unwrap().entries[0]
            .groups
            .is_empty());
        let e = parse_token_file(&format!("{base}groups = [\"\"]\n")).unwrap_err();
        assert!(e.contains("empty name"), "{e}");
        let e = parse_token_file(&format!("{base}scope = []\n")).unwrap_err();
        assert!(e.contains("--auth-policies"), "{e}");
    }

    #[test]
    fn every_client_rpc_has_an_action() {
        for p in graph_proto::rpc_paths() {
            match need(p) {
                Some(Need::Read | Need::Write) => {
                    assert!(method_action(p).is_some(), "{p} has no ADR 0013 action")
                }
                _ => assert!(method_action(p).is_none(), "{p}"),
            }
        }
        use graph_authz::Action as A;
        assert_eq!(
            method_action("/memory_graph.v1.Write/Vacuum"),
            Some(A::Admin)
        );
        assert_eq!(
            method_action("/memory_graph.v1.Admin/UploadSnapshot"),
            Some(A::Admin)
        );
        assert_eq!(
            method_action("/memory_graph.v1.Admin/ListBackups"),
            Some(A::Status)
        );
        assert_eq!(
            method_action("/memory_graph.v1.Admin/ReadIndex"),
            Some(A::Status)
        );
        assert_eq!(
            method_action("/memory_graph.v1.Store/Search"),
            Some(A::Search)
        );
        assert_eq!(method_action("/memory_graph.v1.Store/Get"), Some(A::Read));
        assert_eq!(
            method_action("/memory_graph.v1.Write/Delete"),
            Some(A::Delete)
        );
    }

    /// With policies on, a check with no caller fails closed (and debug
    /// builds assert), except inside the deliberate in-process guard.
    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "no caller"))]
    fn a_missing_caller_fails_closed_with_policies() {
        let t = token(1);
        let (a, _d) = auth_with_policies(
            &file(&[("w", "write", &t)]),
            "permit(principal, action, resource);",
            false,
        );
        {
            let _g = InProcessGuard::enter();
            assert!(a
                .authorize(graph_authz::Action::Index, graph_authz::Resource::Cluster)
                .is_ok());
            assert!(a.read_allowed(graph_authz::Action::Read));
        }
        // Policies off: allowed with no caller, as before.
        let (off, _d2) = auth_with(&file(&[("w", "write", &t)]), None);
        assert!(off
            .authorize(graph_authz::Action::Index, graph_authz::Resource::Cluster)
            .is_ok());
        // Outside the guard: denied (release) / asserts (debug).
        assert!(!a.read_allowed(graph_authz::Action::Read));
        assert!(a
            .authorize(graph_authz::Action::Index, graph_authz::Resource::Cluster)
            .is_err());
    }

    /// ADR 0013 D4: with `--auth-builtins` (and nothing else), every client
    /// RPC is allowed for exactly the roles ADR 0012's table allows it.
    #[test]
    fn builtins_equal_adr_0012_roles_on_every_rpc() {
        let set = graph_authz::PolicySet::builtins();
        let groups: Vec<String> = Vec::new();
        for p in graph_proto::rpc_paths() {
            let Some(need) = need(p) else { continue };
            if !matches!(need, Need::Read | Need::Write) {
                continue;
            }
            let action = method_action(p).expect("every client RPC has an action");
            for (role, arole) in [
                (Role::Read, graph_authz::Role::Read),
                (Role::Write, graph_authz::Role::Write),
            ] {
                let want = role.covers(if need == Need::Read {
                    Role::Read
                } else {
                    Role::Write
                });
                let who = graph_authz::Principal {
                    token: "t",
                    groups: &groups,
                    role: arole,
                };
                for res in [
                    graph_authz::Resource::Cluster,
                    graph_authz::Resource::Org("o"),
                    graph_authz::Resource::Repo("o", "r"),
                ] {
                    assert_eq!(
                        set.decide(&who, action, &res).allowed,
                        want,
                        "{p} for a {} token on {res}",
                        role.as_str()
                    );
                }
            }
        }
    }

    fn auth_with_policies(
        tokens: &str,
        policies: &str,
        builtins: bool,
    ) -> (Arc<Auth>, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let tp = d.path().join("tokens.toml");
        let pp = d.path().join("policies.cedar");
        std::fs::write(&tp, tokens).unwrap();
        std::fs::write(&pp, policies).unwrap();
        let a = Auth::load(
            &AuthConfig {
                tokens: Some(tp),
                policies: Some(pp),
                builtins,
                ..AuthConfig::default()
            },
            false,
        )
        .unwrap();
        (a, d)
    }

    #[test]
    fn cluster_actions_are_decided_in_the_layer() {
        let (w, r) = (token(1), token(2));
        let toks = file(&[("w", "write", &w), ("r", "read", &r)]);
        let (a, _d) = auth_with_policies(
            &toks,
            "permit(principal == Token::\"w\", action == Action::\"status\", resource);",
            false,
        );
        let now = SystemTime::now();
        let check = |p: &str, t: &str| {
            a.check(p, &hdrs(&[bearer(t)]), 1, now)
                .map_err(|e| e.error())
        };
        assert!(check("/memory_graph.v1.Admin/Status", &w).is_ok());
        let e = check("/memory_graph.v1.Admin/Compact", &w).unwrap_err();
        assert!(
            e.to_string()
                .contains("permission denied for action `admin`"),
            "{e}"
        );
        assert!(check("/memory_graph.v1.Admin/Status", &r).is_err());
        // Reads and per-request writes are admitted here by role.
        assert!(check("/memory_graph.v1.Store/Get", &r).is_ok());
        assert!(check("/memory_graph.v1.Write/Index", &w).is_ok());
        // The role table still refuses a read token's write.
        assert_eq!(
            check("/memory_graph.v1.Write/Index", &r)
                .unwrap_err()
                .to_string(),
            Refusal::denied(Reason::Role, Need::Write)
                .error()
                .to_string()
        );
        // A policy refusal is not an ADR 0012 refusal count.
        assert!(a.refusal_counts().iter().all(|(_, n)| *n == 0));
        assert_eq!(a.policy_hash().len(), 64);
        assert!(a.policies_on());
    }

    #[test]
    fn the_policy_hash_tracks_policies_tokens_and_builtins() {
        let t = token(1);
        let toks = file(&[("w", "write", &t)]);
        let p = "permit(principal, action == Action::\"index\", resource);";
        let h = |toks: &str, p: &str, b: bool| auth_with_policies(toks, p, b).0.policy_hash();
        let base = h(&toks, p, false);
        assert_eq!(
            base,
            h(&toks, &format!("// c\n{p}\n"), false),
            "formatting is normalised"
        );
        assert_ne!(base, h(&toks, p, true));
        assert_ne!(
            base,
            h(
                &toks,
                "permit(principal, action == Action::\"prune\", resource);",
                false
            )
        );
        assert_ne!(base, h(&format!("{toks}groups = [\"g\"]\n"), p, false));
        // The digest of a token never enters it: another secret, same hash.
        assert_eq!(base, h(&file(&[("w", "write", &token(5))]), p, false));
        assert_eq!(auth_with(&toks, None).0.policy_hash(), "");
    }

    proptest::proptest! {
        /// D10: the token-file parser never panics on arbitrary input.
        #[test]
        fn the_token_file_parser_never_panics(s in ".{0,400}") {
            let _ = parse_token_file(&s);
            let _ = parse_secret_file(&s);
            let _ = parse_rfc3339(&s);
        }
    }
}
