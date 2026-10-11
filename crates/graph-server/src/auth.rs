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
            "Index" | "IndexFile" | "IngestExtraction" | "Prune" | "Vacuum" => Some(Need::Write),
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
}

impl std::fmt::Debug for TokenEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenEntry")
            .field("id", &self.id)
            .field("role", &self.role)
            .field("expires", &self.expires)
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
                "`scope` is reserved for per-org/repo permissions (#289) and not supported in \
                 this version; refusing the file rather than ignoring it",
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
        entries.push(TokenEntry {
            id: Arc::from(t.id.as_str()),
            role,
            digest,
            expires,
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

struct Source<T> {
    what: &'static str,
    path: PathBuf,
    cur: ArcSwap<T>,
    /// The content hash of the loaded file, and of the last file that
    /// failed (logged once); the lock also serializes reloads.
    state: Mutex<SourceState>,
    parse: fn(&str) -> Result<T, String>,
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

impl<T> Source<T> {
    fn load(
        what: &'static str,
        path: &Path,
        parse: fn(&str) -> Result<T, String>,
    ) -> Result<Self, String> {
        let bytes =
            read_file(path).map_err(|e| format!("cannot read `{}`: {e}", path.display()))?;
        let text = String::from_utf8(bytes.clone())
            .map_err(|_| format!("`{}` is not UTF-8", path.display()))?;
        let set = parse(&text).map_err(|e| format!("`{}`: {e}", path.display()))?;
        warn_if_readable_by_others(what, path);
        Ok(Self {
            what,
            path: path.to_path_buf(),
            cur: ArcSwap::from_pointee(set),
            state: Mutex::new(SourceState {
                loaded: content_hash(&bytes),
                ..SourceState::default()
            }),
            parse,
        })
    }

    fn reload(&self) -> Reload {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let bytes = match read_file(&self.path) {
            Ok(b) => b,
            Err(e) => {
                if !st.read_failed {
                    tracing::error!(
                        file = %self.path.display(),
                        error = %e,
                        "{}: cannot read the file; keeping the loaded set",
                        self.what
                    );
                }
                st.read_failed = true;
                return Reload::Kept;
            }
        };
        st.read_failed = false;
        let h = content_hash(&bytes);
        if h == st.loaded {
            st.failed = None;
            return Reload::Unchanged;
        }
        let parsed = std::str::from_utf8(&bytes)
            .map_err(|_| "the file is not UTF-8".to_string())
            .and_then(|t| (self.parse)(t));
        match parsed {
            Ok(set) => {
                self.cur.store(Arc::new(set));
                st.loaded = h;
                st.failed = None;
                tracing::info!(file = %self.path.display(), "{}: reloaded", self.what);
                Reload::Reloaded
            }
            Err(e) => {
                if st.failed != Some(h) {
                    tracing::error!(
                        file = %self.path.display(),
                        error = %e,
                        "{}: the file failed validation; keeping the loaded set",
                        self.what
                    );
                }
                st.failed = Some(h);
                Reload::Kept
            }
        }
    }
}

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
            tracing::warn!(file = %path.display(), "{what}: readable by group or others; restrict it (chmod 600)");
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
}

/// Refusal classes, as `mg_auth_refusals_total{reason}` labels.
pub const REASONS: [&str; 8] = [
    "missing",
    "malformed",
    "unknown",
    "expired",
    "role",
    "node_only",
    "secret_direct",
    "unlisted",
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
}

impl Refusal {
    fn unauth(reason: Reason) -> Self {
        Self {
            reason,
            required: Need::Open,
        }
    }

    fn denied(reason: Reason, required: Need) -> Self {
        Self { reason, required }
    }

    /// The reason class (a metric label).
    pub fn reason(&self) -> &'static str {
        REASONS[self.reason.index()]
    }

    /// The error answered: `UNAUTHENTICATED` or `PERMISSION_DENIED`, with
    /// a fixed message.
    pub fn error(&self) -> StoreError {
        match self.reason {
            Reason::Missing
            | Reason::Malformed
            | Reason::Unknown
            | Reason::Expired
            | Reason::NodeOnly => StoreError::Unauthenticated(UNAUTHENTICATED_MSG.into()),
            Reason::Role => StoreError::PermissionDenied {
                msg: format!("this method needs the `{}` role", self.required.as_str()),
                required_role: self.required.as_str().into(),
            },
            Reason::SecretDirect => StoreError::PermissionDenied {
                msg: "the cluster secret is accepted here only on a forwarded call".into(),
                required_role: self.required.as_str().into(),
            },
            Reason::Unlisted => StoreError::PermissionDenied {
                msg: "this method is not in the server's role table".into(),
                required_role: String::new(),
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
    tokens: Option<Source<TokenSet>>,
    secret: Option<Source<SecretSet>>,
    refusals: [AtomicU64; REASONS.len()],
    limiter: Mutex<HashMap<Option<IpAddr>, (Instant, u64)>>,
    reload_lock: tokio::sync::Mutex<()>,
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
        })
    }

    /// Load both files (refused at start on any problem). `clustered`: a
    /// `--data-dir` node, which needs the cluster secret when auth is on.
    pub fn load(cfg: &AuthConfig, clustered: bool) -> Result<Arc<Self>, StoreError> {
        let rejected = |flag: &str, e: String| StoreError::Rejected(format!("{flag} {e}"));
        let tokens = match &cfg.tokens {
            Some(p) => Some(
                Source::load("auth token file", p, parse_token_file)
                    .map_err(|e| rejected("--auth-tokens", e))?,
            ),
            None => None,
        };
        let secret = match &cfg.cluster_secret {
            Some(p) => Some(
                Source::load("cluster secret file", p, parse_secret_file)
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
        }))
    }

    /// Whether calls are checked (`--auth-tokens` given).
    pub fn enabled(&self) -> bool {
        self.tokens.is_some()
    }

    /// Re-read both files now; returns what happened to (tokens, secret).
    /// A file that cannot be read or validated keeps the loaded set.
    pub fn reload_now(&self) -> (Option<Reload>, Option<Reload>) {
        (
            self.tokens.as_ref().map(Source::reload),
            self.secret.as_ref().map(Source::reload),
        )
    }

    /// The reloader: every [`RELOAD_POLL`] and on SIGHUP (Unix), until
    /// `shutdown`. A poll and a signal that land together run one after
    /// the other (one lock), and identical content is a no-op.
    pub fn spawn_reloader(self: &Arc<Self>, shutdown: crate::ShutdownHandle) {
        if self.tokens.is_none() && self.secret.is_none() {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            #[cfg(unix)]
            let mut hup =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
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
        self.tokens.as_ref().map(|s| s.cur.load_full())
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
        let Some(tokens) = self.tokens.as_ref() else {
            return Ok(Caller {
                kind: CallerKind::Anonymous,
                conn,
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
        let cred = self.credential(raw, &tokens, secret.as_ref().map(|s| &***s), now);
        let caller = |kind| Caller { kind, conn };
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
                    Cred::Token(e) if e.role.covers(required) => Ok(caller(CallerKind::Token {
                        id: Arc::clone(&e.id),
                        role: e.role,
                    })),
                    Cred::Token(_) => Err(Refusal::denied(Reason::Role, need)),
                    Cred::Secret if need == Need::Read && secret_reads(path) => {
                        Ok(caller(CallerKind::Node {
                            role: None,
                            forwarded_token_id: None,
                        }))
                    }
                    Cred::Secret => {
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
        match self.credential(raw, &tokens, None, SystemTime::now()) {
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
        match self.auth.check(
            &path,
            req.headers(),
            conn.as_ref().map_or(0, |c| c.id),
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
