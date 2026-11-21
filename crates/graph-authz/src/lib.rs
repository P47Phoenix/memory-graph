//! The policy language of ADR 0013: a strict, scope-only subset of
//! [Cedar](https://www.cedarpolicy.com/), with a hand-written parser, a
//! printer and an indexed evaluator. No dependencies (D3: `cedar-policy`
//! fails the pure-Rust gate).
//!
//! * [`parse`] reads a policy file (D2): `permit`/`forbid` over
//!   `principal`, `action` and `resource` scopes, annotations, `//` and
//!   `/* */` comments. Anything else (a condition, `is`, `like`, a template
//!   slot, an unknown entity type or action, a namespaced type) is an error,
//!   never ignored. Every accepted file is valid Cedar with the same meaning
//!   (checked against `cedar-policy` by the dev-only oracle, story 72a).
//! * [`PolicySet::decide`] answers one request: default deny, allowed iff a
//!   `permit` matches and no `forbid` does. It walks only the principal's
//!   buckets (its token, its groups, its role, and the wildcard), and counts
//!   the policies it visits (the deterministic CI gate, D10).
//! * The model (D1): principals `Token::"id"` in `Group::"g"` and
//!   `Role::"read"`/`Role::"write"`; resources `Org::"o"` ⊃ `Repo::"o/r"`
//!   (split at the first `/`), and `Cluster::"this"`.
use std::collections::{BTreeSet, HashMap};
use std::fmt;

/// The largest policy file accepted (exactly this many bytes is fine).
pub const MAX_FILE_BYTES: usize = 1 << 20;
/// The most policies a file may hold (exactly this many is fine).
pub const MAX_POLICIES: usize = 10_000;

// ---------------------------------------------------------------------------
// Actions.

/// An action (D1). Each gRPC method and MCP tool maps to exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    Read,
    Search,
    Index,
    Prune,
    Delete,
    Snapshot,
    Status,
    Admin,
}

/// A set of actions, one bit each.
pub type ActionSet = u16;

impl Action {
    pub const ALL: [Action; 8] = [
        Action::Read,
        Action::Search,
        Action::Index,
        Action::Prune,
        Action::Delete,
        Action::Snapshot,
        Action::Status,
        Action::Admin,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Action::Read => "read",
            Action::Search => "search",
            Action::Index => "index",
            Action::Prune => "prune",
            Action::Delete => "delete",
            Action::Snapshot => "snapshot",
            Action::Status => "status",
            Action::Admin => "admin",
        }
    }

    /// Case-sensitive (`Read` is unknown, D2).
    pub fn parse(s: &str) -> Option<Action> {
        Action::ALL.into_iter().find(|a| a.name() == s)
    }

    pub fn bit(self) -> ActionSet {
        1 << (self as u16)
    }
}

/// Every action.
pub const ALL_ACTIONS: ActionSet = (1 << 8) - 1;

/// The actions reads use: the PR 1 interim rule is about these.
pub const READ_ACTIONS: ActionSet = 1 << (Action::Read as u16) | 1 << (Action::Search as u16);

/// A token's role: the ceiling its policies grant within (D4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Read,
    Write,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
        }
    }

    /// The actions this role may ever be granted: a `read` token never
    /// gets `index`, `prune`, `delete`, `snapshot` or `admin`.
    pub fn ceiling(self) -> ActionSet {
        match self {
            Role::Read => READ_ACTIONS | Action::Status.bit(),
            Role::Write => ALL_ACTIONS,
        }
    }
}

// ---------------------------------------------------------------------------
// Entities and scopes.

/// The entity types the subset knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Ty {
    Token,
    Group,
    Role,
    Org,
    Repo,
    Cluster,
}

impl Ty {
    pub fn name(self) -> &'static str {
        match self {
            Ty::Token => "Token",
            Ty::Group => "Group",
            Ty::Role => "Role",
            Ty::Org => "Org",
            Ty::Repo => "Repo",
            Ty::Cluster => "Cluster",
        }
    }

    fn parse(s: &str) -> Option<Ty> {
        [
            Ty::Token,
            Ty::Group,
            Ty::Role,
            Ty::Org,
            Ty::Repo,
            Ty::Cluster,
        ]
        .into_iter()
        .find(|t| t.name() == s)
    }

    fn principal_side(self) -> bool {
        matches!(self, Ty::Token | Ty::Group | Ty::Role)
    }
}

/// An entity literal, `Type::"id"`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Entity {
    pub ty: Ty,
    pub id: String,
}

impl Entity {
    /// A `Repo::` id split at the first `/` (D1).
    pub fn repo_parts(&self) -> Option<(&str, &str)> {
        (self.ty == Ty::Repo).then(|| self.id.split_once('/'))?
    }
}

/// `principal`, `principal == E`, `principal in E` (and the same for
/// `resource`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    Any,
    Eq(Entity),
    In(Entity),
}

/// `action`, `action == A`, `action in [A, ...]`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionScope {
    Any,
    Eq(Action),
    In(Vec<Action>),
}

impl ActionScope {
    pub fn set(&self) -> ActionSet {
        match self {
            ActionScope::Any => ALL_ACTIONS,
            ActionScope::Eq(a) => a.bit(),
            ActionScope::In(v) => v.iter().fold(0, |s, a| s | a.bit()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Effect {
    Permit,
    Forbid,
}

/// One policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// The `@id` annotation, or `policyN` (N: its position, as Cedar names
    /// policies without one).
    pub id: String,
    /// Every annotation in file order (`@id` included), value optional.
    pub annotations: Vec<(String, Option<String>)>,
    pub effect: Effect,
    pub principal: Scope,
    pub action: ActionScope,
    pub resource: Scope,
    /// The 1-based line the policy starts on (not part of its meaning).
    pub line: usize,
}

impl Policy {
    pub fn actions(&self) -> ActionSet {
        self.action.set()
    }

    fn res_matches(&self, res: &Resource<'_>) -> bool {
        scope_matches(&self.resource, res)
    }
}

/// Whether a resource scope covers `res`. `Org::"o"` contains every
/// `Repo::"o/..."`; nothing else has members.
fn scope_matches(scope: &Scope, res: &Resource<'_>) -> bool {
    match scope {
        Scope::Any => true,
        Scope::Eq(e) => entity_is(e, res),
        Scope::In(e) => {
            entity_is(e, res)
                || matches!((e.ty, res), (Ty::Org, Resource::Repo(o, _)) if e.id == *o)
        }
    }
}

fn entity_is(e: &Entity, res: &Resource<'_>) -> bool {
    match (e.ty, res) {
        (Ty::Org, Resource::Org(o)) => e.id == *o,
        (Ty::Repo, Resource::Repo(o, r)) => e.repo_parts() == Some((o, r)),
        (Ty::Cluster, Resource::Cluster) => true,
        _ => false,
    }
}

/// The resource of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource<'a> {
    Org(&'a str),
    /// (org, repo).
    Repo(&'a str, &'a str),
    Cluster,
}

impl fmt::Display for Resource<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Resource::Org(o) => write!(f, "Org::\"{}\"", escape(o)),
            Resource::Repo(o, r) => write!(f, "Repo::\"{}\"", escape(&format!("{o}/{r}"))),
            Resource::Cluster => f.write_str("Cluster::\"this\""),
        }
    }
}

/// The principal of a request: a token, its groups and its role.
#[derive(Debug, Clone, Copy)]
pub struct Principal<'a> {
    pub token: &'a str,
    pub groups: &'a [String],
    pub role: Role,
}

// ---------------------------------------------------------------------------
// The policy set and the evaluator.

/// A parsed, indexed policy set.
#[derive(Debug, Clone, Default)]
pub struct PolicySet {
    policies: Vec<Policy>,
    by_token: HashMap<String, Vec<usize>>,
    by_group: HashMap<String, Vec<usize>>,
    /// `principal in Role::"read"` / `Role::"write"`.
    by_role: [Vec<usize>; 2],
    any: Vec<usize>,
}

/// The answer to one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub allowed: bool,
    /// A `permit` matched.
    pub permitted: bool,
    /// A `forbid` matched.
    pub forbidden: bool,
    /// The `@id`s of every policy that matched, permits and forbids.
    pub matched: Vec<String>,
    /// How many policies were looked at (the D10 gate).
    pub visited: usize,
}

/// The two built-in policies (D4), equal to ADR 0012 D5.
pub const BUILTINS: &str = "@id(\"builtin:read\")\npermit(principal in Role::\"read\", action in [Action::\"read\", Action::\"search\", Action::\"status\"], resource);\n@id(\"builtin:write\")\npermit(principal in Role::\"write\", action, resource);\n";

impl PolicySet {
    pub fn new(policies: Vec<Policy>) -> Self {
        let mut s = PolicySet {
            policies,
            ..Default::default()
        };
        for (i, p) in s.policies.iter().enumerate() {
            match &p.principal {
                Scope::Any => s.any.push(i),
                // `principal in Token::"x"` means `principal == Token::"x"`.
                Scope::Eq(e) | Scope::In(e) if e.ty == Ty::Token => {
                    s.by_token.entry(e.id.clone()).or_default().push(i)
                }
                Scope::In(e) if e.ty == Ty::Group => {
                    s.by_group.entry(e.id.clone()).or_default().push(i)
                }
                Scope::In(e) if e.ty == Ty::Role => s.by_role[usize::from(e.id == "write")].push(i),
                // `principal == Group::"g"` / `Role::"r"`: a principal is
                // always a token, so these never match and sit in no bucket.
                _ => {}
            }
        }
        s
    }

    /// The built-ins alone (no policy file, D4).
    pub fn builtins() -> Self {
        PolicySet::new(parse(BUILTINS.as_bytes()).expect("the built-ins parse"))
    }

    /// This set plus the built-ins (`serve --auth-builtins`).
    pub fn with_builtins(mut self) -> Self {
        let mut p = std::mem::take(&mut self.policies);
        p.extend(parse(BUILTINS.as_bytes()).expect("the built-ins parse"));
        PolicySet::new(p)
    }

    pub fn policies(&self) -> &[Policy] {
        &self.policies
    }

    pub fn len(&self) -> usize {
        self.policies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    /// The buckets a principal's request walks, each policy once.
    fn buckets<'s>(&'s self, p: &Principal<'_>) -> Vec<&'s [usize]> {
        let mut out: Vec<&[usize]> = Vec::new();
        if let Some(b) = self.by_token.get(p.token) {
            out.push(b);
        }
        let mut seen = BTreeSet::new();
        for g in p.groups {
            if seen.insert(g.as_str()) {
                if let Some(b) = self.by_group.get(g) {
                    out.push(b);
                }
            }
        }
        out.push(&self.by_role[0]);
        if p.role == Role::Write {
            out.push(&self.by_role[1]);
        }
        out.push(&self.any);
        out
    }

    /// Decide one request (D1, D3). The role is a ceiling: an action above
    /// it is denied without looking at any policy.
    pub fn decide(&self, p: &Principal<'_>, action: Action, res: &Resource<'_>) -> Decision {
        let mut d = Decision {
            allowed: false,
            permitted: false,
            forbidden: false,
            matched: Vec::new(),
            visited: 0,
        };
        if p.role.ceiling() & action.bit() == 0 {
            return d;
        }
        for bucket in self.buckets(p) {
            for &i in bucket {
                d.visited += 1;
                let pol = &self.policies[i];
                if pol.actions() & action.bit() != 0 && pol.res_matches(res) {
                    match pol.effect {
                        Effect::Permit => d.permitted = true,
                        Effect::Forbid => d.forbidden = true,
                    }
                    d.matched.push(pol.id.clone());
                }
            }
        }
        d.allowed = d.permitted && !d.forbidden;
        d
    }

    /// An org-wide `prune` or `delete` (D6): decided on policy alone. It
    /// needs a permit on the `Org` itself or on `Cluster::"this"`, and no
    /// forbid on the org, the cluster, or any `Repo` inside the org. Per-repo
    /// permits never add up to an org-wide grant.
    pub fn decide_org_wide(&self, p: &Principal<'_>, action: Action, org: &str) -> Decision {
        let mut on_org = self.decide(p, action, &Resource::Org(org));
        let on_cluster = self.decide(p, action, &Resource::Cluster);
        let mut repo_forbid = false;
        if p.role.ceiling() & action.bit() != 0 {
            for bucket in self.buckets(p) {
                for &i in bucket {
                    on_org.visited += 1;
                    let pol = &self.policies[i];
                    if pol.effect != Effect::Forbid || pol.actions() & action.bit() == 0 {
                        continue;
                    }
                    if let Scope::Eq(e) | Scope::In(e) = &pol.resource {
                        if e.repo_parts().is_some_and(|(o, _)| o == org) {
                            repo_forbid = true;
                            on_org.matched.push(pol.id.clone());
                        }
                    }
                }
            }
        }
        on_org.permitted |= on_cluster.permitted;
        on_org.forbidden |= on_cluster.forbidden || repo_forbid;
        on_org.visited += on_cluster.visited;
        for m in on_cluster.matched {
            if !on_org.matched.contains(&m) {
                on_org.matched.push(m);
            }
        }
        on_org.allowed = on_org.permitted && !on_org.forbidden;
        on_org
    }

    /// The PR 1 interim rule (ADR 0013 Rollout, Q9): until reads are
    /// scoped (PR 3), a `permit` or `forbid` whose actions include `read`
    /// or `search` must not constrain `resource`, or it would be accepted
    /// and silently not enforced on reads. The error names the line and
    /// the `@id`.
    pub fn check_interim_read_rule(&self) -> Result<(), ParseError> {
        for p in &self.policies {
            if p.actions() & READ_ACTIONS != 0 && p.resource != Scope::Any {
                return Err(ParseError {
                    line: p.line,
                    id: Some(p.id.clone()),
                    msg: "a policy for `read` or `search` must not constrain `resource` in this \
                          version (reads are all-or-nothing per principal until scoped reads \
                          land, ADR 0013 PR 3)"
                        .into(),
                });
            }
        }
        Ok(())
    }

    /// Warnings for an accepted set (D2), given the token file's ids and
    /// groups: unknown principals (the policy matches nothing; revocation
    /// must never fail), a set with only `forbid`s, `action in []`, and
    /// names that may not be NFC.
    pub fn lint(
        &self,
        known_token: &dyn Fn(&str) -> bool,
        known_group: &dyn Fn(&str) -> bool,
    ) -> Vec<String> {
        let mut w = Vec::new();
        for p in &self.policies {
            let at = format!("line {} (@id `{}`)", p.line, p.id);
            if let Scope::Eq(e) | Scope::In(e) = &p.principal {
                match e.ty {
                    Ty::Token if !known_token(&e.id) => w.push(format!(
                        "{at}: `Token::\"{}\"` is not in the token file; the policy matches nothing",
                        escape(&e.id)
                    )),
                    Ty::Group if !known_group(&e.id) => w.push(format!(
                        "{at}: no token is in `Group::\"{}\"`; the policy matches nothing",
                        escape(&e.id)
                    )),
                    Ty::Group | Ty::Role if matches!(p.principal, Scope::Eq(_)) => w.push(format!(
                        "{at}: `principal == {}::...` never matches (a principal is a token); \
                         did you mean `in`?",
                        e.ty.name()
                    )),
                    _ => {}
                }
            }
            if p.action == ActionScope::In(Vec::new()) {
                w.push(format!("{at}: `action in []` matches no action"));
            }
            for e in [&p.principal, &p.resource]
                .into_iter()
                .filter_map(|s| match s {
                    Scope::Eq(e) | Scope::In(e) => Some(e),
                    Scope::Any => None,
                })
            {
                if maybe_not_nfc(&e.id) {
                    w.push(format!(
                        "{at}: the name of `{}::...` has combining marks and may not be NFC; names \
                         are compared byte for byte",
                        e.ty.name()
                    ));
                }
            }
        }
        if !self.policies.is_empty() && self.policies.iter().all(|p| p.effect == Effect::Forbid) {
            w.push("the file holds only `forbid` policies; it grants nothing".into());
        }
        w
    }

    /// The set printed in a canonical order (for `policy_hash`): the
    /// printed policies, sorted.
    pub fn normalized(&self) -> String {
        let mut v: Vec<String> = self.policies.iter().map(|p| p.to_string()).collect();
        v.sort();
        v.join("\n")
    }
}

/// A heuristic without a Unicode table dependency: a name with a combining
/// mark is likely decomposed (NFD).
fn maybe_not_nfc(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
    })
}

// ---------------------------------------------------------------------------
// Printing.

/// A string body in Cedar's escape syntax.
pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '\0' => o.push_str("\\0"),
            c if c.is_control() => o.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

impl fmt::Display for Entity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::\"{}\"", self.ty.name(), escape(&self.id))
    }
}

fn fmt_scope(f: &mut fmt::Formatter<'_>, var: &str, s: &Scope) -> fmt::Result {
    match s {
        Scope::Any => f.write_str(var),
        Scope::Eq(e) => write!(f, "{var} == {e}"),
        Scope::In(e) => write!(f, "{var} in {e}"),
    }
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (k, v) in &self.annotations {
            match v {
                Some(v) => writeln!(f, "@{k}(\"{}\")", escape(v))?,
                None => writeln!(f, "@{k}")?,
            }
        }
        f.write_str(match self.effect {
            Effect::Permit => "permit(",
            Effect::Forbid => "forbid(",
        })?;
        fmt_scope(f, "principal", &self.principal)?;
        f.write_str(", ")?;
        match &self.action {
            ActionScope::Any => f.write_str("action")?,
            ActionScope::Eq(a) => write!(f, "action == Action::\"{}\"", a.name())?,
            ActionScope::In(v) => {
                f.write_str("action in [")?;
                for (i, a) in v.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "Action::\"{}\"", a.name())?;
                }
                f.write_str("]")?;
            }
        }
        f.write_str(", ")?;
        fmt_scope(f, "resource", &self.resource)?;
        f.write_str(");")
    }
}

// ---------------------------------------------------------------------------
// Parsing.

/// A refused policy file: the line, the `@id` when known, and what is
/// wrong. Never quotes the file's text beyond names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub id: Option<String>,
    pub msg: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.id {
            Some(id) => write!(f, "line {} (@id `{}`): {}", self.line, id, self.msg),
            None => write!(f, "line {}: {}", self.line, self.msg),
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    Str(String),
    ColonColon,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Semi,
    At,
    EqEq,
}

fn err(line: usize, msg: impl Into<String>) -> ParseError {
    ParseError {
        line,
        id: None,
        msg: msg.into(),
    }
}

fn lex(src: &str) -> Result<Vec<(Tok, usize)>, ParseError> {
    let b: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line = 1;
    while i < b.len() {
        let c = b[i];
        match c {
            '\n' => {
                line += 1;
                i += 1;
            }
            ' ' | '\t' | '\r' => i += 1,
            '/' if b.get(i + 1) == Some(&'/') => {
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
            }
            '/' if b.get(i + 1) == Some(&'*') => {
                let start = line;
                i += 2;
                loop {
                    if i + 1 >= b.len() {
                        return Err(err(start, "unterminated `/*` comment"));
                    }
                    if b[i] == '*' && b[i + 1] == '/' {
                        i += 2;
                        break;
                    }
                    if b[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
            '(' | ')' | '[' | ']' | ',' | ';' | '@' => {
                out.push((
                    match c {
                        '(' => Tok::LParen,
                        ')' => Tok::RParen,
                        '[' => Tok::LBracket,
                        ']' => Tok::RBracket,
                        ',' => Tok::Comma,
                        ';' => Tok::Semi,
                        _ => Tok::At,
                    },
                    line,
                ));
                i += 1;
            }
            ':' if b.get(i + 1) == Some(&':') => {
                out.push((Tok::ColonColon, line));
                i += 2;
            }
            '=' if b.get(i + 1) == Some(&'=') => {
                out.push((Tok::EqEq, line));
                i += 2;
            }
            '?' => {
                return Err(err(
                    line,
                    "template slots (`?principal`, `?resource`) are not supported",
                ))
            }
            '"' => {
                let start = line;
                i += 1;
                let mut s = String::new();
                loop {
                    let Some(&c) = b.get(i) else {
                        return Err(err(start, "unterminated string"));
                    };
                    i += 1;
                    match c {
                        '"' => break,
                        '\n' => {
                            line += 1;
                            s.push(c);
                        }
                        '\\' => {
                            let Some(&e) = b.get(i) else {
                                return Err(err(start, "unterminated string"));
                            };
                            i += 1;
                            match e {
                                '"' => s.push('"'),
                                '\'' => s.push('\''),
                                '\\' => s.push('\\'),
                                'n' => s.push('\n'),
                                'r' => s.push('\r'),
                                't' => s.push('\t'),
                                '0' => s.push('\0'),
                                'x' => {
                                    let h: String =
                                        b.get(i..i + 2).unwrap_or_default().iter().collect();
                                    let v = (h.len() == 2
                                        && h.chars().all(|c| c.is_ascii_hexdigit()))
                                    .then(|| u8::from_str_radix(&h, 16).ok())
                                    .flatten()
                                    .filter(|v| *v <= 0x7f)
                                    .ok_or_else(|| {
                                        err(
                                            line,
                                            "invalid `\\x` escape (two hex digits, at most 7f)",
                                        )
                                    })?;
                                    s.push(char::from(v));
                                    i += 2;
                                }
                                'u' => {
                                    if b.get(i) != Some(&'{') {
                                        return Err(err(
                                            line,
                                            "invalid `\\u` escape (expected `\\u{...}`)",
                                        ));
                                    }
                                    let close = (i + 1..b.len().min(i + 9)).find(|&j| b[j] == '}');
                                    let ch = close
                                        .and_then(|j| {
                                            let h: String = b[i + 1..j].iter().collect();
                                            (!h.is_empty() && h.len() <= 6 && h.chars().all(|c| c.is_ascii_hexdigit()))
                                                .then(|| u32::from_str_radix(&h, 16).ok())
                                                .flatten()
                                                .and_then(char::from_u32)
                                                .map(|c| (c, j))
                                        })
                                        .ok_or_else(|| err(line, "invalid `\\u{...}` escape (1-6 hex digits, a Unicode scalar value)"))?;
                                    s.push(ch.0);
                                    i = ch.1 + 1;
                                }
                                _ => return Err(err(line, "invalid escape in a string")),
                            }
                        }
                        c => s.push(c),
                    }
                }
                out.push((Tok::Str(s), start));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let st = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '_') {
                    i += 1;
                }
                out.push((Tok::Ident(b[st..i].iter().collect()), line));
            }
            _ => return Err(err(line, "unexpected character")),
        }
    }
    Ok(out)
}

struct Parser {
    toks: Vec<(Tok, usize)>,
    pos: usize,
    /// The `@id` of the policy being parsed (for error messages).
    cur_id: Option<String>,
    last_line: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.0)
    }

    fn line(&self) -> usize {
        self.toks.get(self.pos).map_or(self.last_line, |t| t.1)
    }

    fn fail<T>(&self, msg: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError {
            line: self.line(),
            id: self.cur_id.clone(),
            msg: msg.into(),
        })
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).map(|t| t.0.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, want: Tok, what: &str) -> Result<(), ParseError> {
        if self.peek() == Some(&want) {
            self.pos += 1;
            Ok(())
        } else {
            self.fail(format!("expected {what}"))
        }
    }

    fn ident(&mut self, what: &str) -> Result<String, ParseError> {
        match self.peek() {
            Some(Tok::Ident(s)) => {
                let s = s.clone();
                self.pos += 1;
                Ok(s)
            }
            _ => self.fail(format!("expected {what}")),
        }
    }

    fn keyword(&mut self, kw: &str) -> Result<(), ParseError> {
        match self.peek() {
            Some(Tok::Ident(s)) if s == kw => {
                self.pos += 1;
                Ok(())
            }
            _ => self.fail(format!("expected `{kw}`")),
        }
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s == kw)
    }

    /// `Type::"id"`, refusing namespaced types.
    fn entity_path(&mut self) -> Result<(String, String), ParseError> {
        let mut path = vec![self.ident("an entity (`Type::\"id\"`)")?];
        loop {
            self.expect(Tok::ColonColon, "`::`")?;
            match self.next() {
                Some(Tok::Str(s)) => break Ok((path.join("::"), s)),
                Some(Tok::Ident(n)) => path.push(n),
                _ => return self.fail("expected a string id after `::`"),
            }
            if path.len() > 1 && self.peek() != Some(&Tok::ColonColon) {
                return self.fail("expected `::`");
            }
        }
        .and_then(|(ty, id)| {
            if ty.contains("::") {
                self.fail(format!("namespaced type `{ty}` is not supported"))
            } else {
                Ok((ty, id))
            }
        })
    }

    fn entity(&mut self, principal_side: bool) -> Result<Entity, ParseError> {
        let (ty, id) = self.entity_path()?;
        let Some(t) = Ty::parse(&ty) else {
            return self.fail(format!("unknown entity type `{ty}`"));
        };
        if t.principal_side() != principal_side {
            return self.fail(format!(
                "`{ty}::` cannot be a {}",
                if principal_side {
                    "principal"
                } else {
                    "resource"
                }
            ));
        }
        match t {
            Ty::Role if id != "read" && id != "write" => {
                return self.fail("unknown role (`Role::\"read\"` or `Role::\"write\"`)")
            }
            Ty::Cluster if id != "this" => {
                return self.fail("the only cluster resource is `Cluster::\"this\"`")
            }
            Ty::Repo => match id.split_once('/') {
                Some((o, r)) if !o.is_empty() && !r.is_empty() => {}
                _ => return self.fail("a `Repo::` id is `org/repo`, both parts non-empty"),
            },
            _ => {}
        }
        Ok(Entity { ty: t, id })
    }

    fn action_ref(&mut self) -> Result<Action, ParseError> {
        let (ty, id) = self.entity_path()?;
        if ty != "Action" {
            return self.fail(format!("expected `Action::\"...\"`, found `{ty}::`"));
        }
        match Action::parse(&id) {
            Some(a) => Ok(a),
            None => self.fail(format!(
                "unknown action `{}` (actions are lower case: read, search, index, prune, delete, snapshot, status, admin)",
                escape(&id)
            )),
        }
    }

    fn scope(&mut self, var: &str, principal_side: bool) -> Result<Scope, ParseError> {
        self.keyword(var)?;
        if self.peek() == Some(&Tok::EqEq) {
            self.pos += 1;
            return Ok(Scope::Eq(self.entity(principal_side)?));
        }
        if self.is_kw("in") {
            self.pos += 1;
            return Ok(Scope::In(self.entity(principal_side)?));
        }
        if self.is_kw("is") {
            return self.fail("`is` is not supported");
        }
        Ok(Scope::Any)
    }

    fn action_scope(&mut self) -> Result<ActionScope, ParseError> {
        self.keyword("action")?;
        if self.peek() == Some(&Tok::EqEq) {
            self.pos += 1;
            return Ok(ActionScope::Eq(self.action_ref()?));
        }
        if self.is_kw("in") {
            self.pos += 1;
            if self.peek() == Some(&Tok::LBracket) {
                self.pos += 1;
                let mut v = Vec::new();
                if self.peek() == Some(&Tok::RBracket) {
                    self.pos += 1;
                    return Ok(ActionScope::In(v));
                }
                loop {
                    v.push(self.action_ref()?);
                    match self.next() {
                        Some(Tok::Comma) => continue,
                        Some(Tok::RBracket) => break,
                        _ => {
                            self.pos = self.pos.saturating_sub(1);
                            return self.fail("expected `,` or `]`");
                        }
                    }
                }
                return Ok(ActionScope::In(v));
            }
            return Ok(ActionScope::In(vec![self.action_ref()?]));
        }
        if self.is_kw("is") {
            return self.fail("`is` is not supported");
        }
        Ok(ActionScope::Any)
    }

    fn policy(&mut self, n: usize) -> Result<Policy, ParseError> {
        self.cur_id = None;
        let line = self.line();
        let mut annotations: Vec<(String, Option<String>)> = Vec::new();
        while self.peek() == Some(&Tok::At) {
            self.pos += 1;
            let k = self.ident("an annotation name")?;
            let v = if self.peek() == Some(&Tok::LParen) {
                self.pos += 1;
                let v = match self.next() {
                    Some(Tok::Str(s)) => s,
                    _ => {
                        self.pos = self.pos.saturating_sub(1);
                        return self.fail("expected a string annotation value");
                    }
                };
                self.expect(Tok::RParen, "`)`")?;
                Some(v)
            } else {
                None
            };
            if annotations.iter().any(|(a, _)| *a == k) {
                return self.fail(format!("duplicate annotation `@{k}`"));
            }
            if k == "id" {
                self.cur_id = v.clone();
            }
            annotations.push((k, v));
        }
        let effect = match self.peek() {
            Some(Tok::Ident(s)) if s == "permit" => Effect::Permit,
            Some(Tok::Ident(s)) if s == "forbid" => Effect::Forbid,
            _ => return self.fail("expected `permit` or `forbid`"),
        };
        self.pos += 1;
        self.expect(Tok::LParen, "`(`")?;
        let principal = self.scope("principal", true)?;
        self.expect(Tok::Comma, "`,`")?;
        let action = self.action_scope()?;
        self.expect(Tok::Comma, "`,`")?;
        let resource = self.scope("resource", false)?;
        if self.peek() == Some(&Tok::Comma) {
            return self.fail("expected `)`");
        }
        self.expect(Tok::RParen, "`)`")?;
        if self.is_kw("when") || self.is_kw("unless") {
            return self.fail(
                "conditions (`when`/`unless`) are not supported (scope-only subset, ADR 0013 D2)",
            );
        }
        self.expect(Tok::Semi, "`;`")?;
        let id = match annotations.iter().find(|(k, _)| k == "id") {
            Some((_, Some(v))) => v.clone(),
            Some((_, None)) => return self.fail("`@id` needs a value"),
            None => format!("policy{n}"),
        };
        Ok(Policy {
            id,
            annotations,
            effect,
            principal,
            action,
            resource,
            line,
        })
    }
}

/// Parse a policy file (D2). Refused: invalid UTF-8, a NUL byte, a file
/// over [`MAX_FILE_BYTES`], more than [`MAX_POLICIES`] policies, a duplicate
/// `@id`, and anything outside the grammar. A leading UTF-8 BOM is skipped;
/// CRLF and LF are the same; an empty or comments-only file is an empty set.
pub fn parse(bytes: &[u8]) -> Result<Vec<Policy>, ParseError> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(err(0, format!("the file is over {MAX_FILE_BYTES} bytes")));
    }
    let text = std::str::from_utf8(bytes).map_err(|e| {
        let line = bytes[..e.valid_up_to()]
            .iter()
            .filter(|b| **b == b'\n')
            .count()
            + 1;
        err(line, "the file is not valid UTF-8")
    })?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if let Some(at) = text.find('\0') {
        return Err(err(
            text[..at].matches('\n').count() + 1,
            "NUL byte in the file",
        ));
    }
    let toks = lex(text)?;
    let last_line = toks.last().map_or(1, |t| t.1);
    let mut p = Parser {
        toks,
        pos: 0,
        cur_id: None,
        last_line,
    };
    let mut out: Vec<Policy> = Vec::new();
    let mut ids = BTreeSet::new();
    while p.peek().is_some() {
        if out.len() == MAX_POLICIES {
            return Err(err(p.line(), format!("more than {MAX_POLICIES} policies")));
        }
        let pol = p.policy(out.len())?;
        if pol.annotations.iter().any(|(k, _)| k == "id") && !ids.insert(pol.id.clone()) {
            return Err(ParseError {
                line: pol.line,
                id: Some(pol.id),
                msg: "duplicate `@id`".into(),
            });
        }
        out.push(pol);
    }
    Ok(out)
}

/// Parse a resource for `auth check --explain`: `Org::"o"`, `Repo::"o/r"`
/// or `Cluster::"this"`.
pub fn parse_resource(s: &str) -> Result<Entity, ParseError> {
    let mut p = Parser {
        toks: lex(s)?,
        pos: 0,
        cur_id: None,
        last_line: 1,
    };
    let e = p.entity(false)?;
    if p.peek().is_some() {
        return p.fail("trailing input after the resource");
    }
    Ok(e)
}

impl Entity {
    /// This resource entity as a request resource.
    pub fn as_resource(&self) -> Option<Resource<'_>> {
        match self.ty {
            Ty::Org => Some(Resource::Org(&self.id)),
            Ty::Repo => self.repo_parts().map(|(o, r)| Resource::Repo(o, r)),
            Ty::Cluster => Some(Resource::Cluster),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
