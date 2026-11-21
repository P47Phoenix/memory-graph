//! Read scoping (ADR 0013 D5): which repos and orgs a read may see.
//!
//! A [`ReadScope`] is plain data (name sets), compiled once per request by
//! the server from its policy set for one principal and one read action
//! (`read` or `search`). The store never sees a token or a policy: it tests
//! membership per repo.
//!
//! The two rules of D5:
//! - **Named reads** (a request that names an org, a repo, a path or a node
//!   id) check **existence first, then scope**: a resource that does not
//!   exist now keeps today's answer; one that exists and is hidden is
//!   [`StoreError::PermissionDenied`] (the message names only the action).
//! - **Aggregate reads** (naming nothing) are filtered inside the query,
//!   before ranking, `limit` and paging, and never refused.
use crate::{RepoInfo, StoreError};
type Result<T> = std::result::Result<T, StoreError>;
use std::collections::BTreeSet;
use std::sync::Arc;

/// One principal's grants for one read action, as name sets (ADR 0013 D3).
///
/// A repo `(o, r)` is visible iff it is allowed (`any`, or `o` in `orgs`,
/// or `(o, r)` in `repos`) and not denied (`deny_all`, `o` in `deny_orgs`,
/// `(o, r)` in `deny_repos`). The org itself is *granted* iff it is allowed
/// (`any` or `o` in `orgs`) and not denied (`deny_all` or `o` in
/// `deny_orgs`); a `deny_repos` entry never denies the org itself, exactly
/// as a `forbid` on `Repo::"o/r"` does not match `Org::"o"`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeRules {
    /// A `permit` on any resource.
    pub any: bool,
    /// `permit` on `Org::"o"` (`==` or `in`).
    pub orgs: BTreeSet<String>,
    /// `permit` on `Repo::"o/r"`, as `(o, r)`.
    pub repos: BTreeSet<(String, String)>,
    /// A `forbid` on any resource.
    pub deny_all: bool,
    /// `forbid` on `Org::"o"` with `in` (the org and every repo in it);
    /// `forbid ... resource == Org::"o"` goes in `deny_org_only`.
    pub deny_orgs: BTreeSet<String>,
    /// `forbid ... resource == Org::"o"`: the org itself, not its repos.
    pub deny_org_only: BTreeSet<String>,
    /// `forbid` on `Repo::"o/r"`.
    pub deny_repos: BTreeSet<(String, String)>,
    /// `permit ... resource == Org::"o"`: the org itself, not its repos.
    pub org_only: BTreeSet<String>,
}

impl ScopeRules {
    fn repo_visible(&self, org: &str, repo: &str) -> bool {
        let key = || (org.to_owned(), repo.to_owned());
        let allowed = self.any || self.orgs.contains(org) || self.repos.contains(&key());
        allowed
            && !self.deny_all
            && !self.deny_orgs.contains(org)
            && !self.deny_repos.contains(&key())
    }

    fn org_granted(&self, org: &str) -> bool {
        let allowed = self.any || self.orgs.contains(org) || self.org_only.contains(org);
        allowed
            && !self.deny_all
            && !self.deny_orgs.contains(org)
            && !self.deny_org_only.contains(org)
    }

    /// Nothing is ever denied and everything is allowed: the same as
    /// [`ReadScope::All`].
    fn is_everything(&self) -> bool {
        self.any
            && !self.deny_all
            && self.deny_orgs.is_empty()
            && self.deny_org_only.is_empty()
            && self.deny_repos.is_empty()
    }
}

/// What a read may see (ADR 0013 D5). The default, `All`, is today's
/// behaviour exactly: no policy file, or a principal allowed the action on
/// any resource with no `forbid`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ReadScope {
    #[default]
    All,
    /// Visible only where **every** rule set allows it (one set per
    /// principal decision; more than one after [`ReadScope::intersect`],
    /// e.g. a snapshot handle's stored scope and the current one, D8).
    Only(Arc<Vec<ScopeRules>>),
}

impl ReadScope {
    /// A scope from one principal's rules: `All` when the rules allow
    /// everything and deny nothing.
    pub fn from_rules(rules: ScopeRules) -> Self {
        if rules.is_everything() {
            ReadScope::All
        } else {
            ReadScope::Only(Arc::new(vec![rules]))
        }
    }

    /// The zero-grant scope: nothing is visible.
    pub fn nothing() -> Self {
        ReadScope::Only(Arc::new(vec![ScopeRules::default()]))
    }

    pub fn is_all(&self) -> bool {
        matches!(self, ReadScope::All)
    }

    /// Visible where both are.
    pub fn intersect(&self, other: &ReadScope) -> ReadScope {
        match (self, other) {
            (ReadScope::All, o) | (o, ReadScope::All) => o.clone(),
            (ReadScope::Only(a), ReadScope::Only(b)) => {
                let mut v: Vec<ScopeRules> = a.as_ref().clone();
                for r in b.iter() {
                    if !v.contains(r) {
                        v.push(r.clone());
                    }
                }
                ReadScope::Only(Arc::new(v))
            }
        }
    }

    /// Whether repo `repo` of org `org` is visible.
    pub fn repo_visible(&self, org: &str, repo: &str) -> bool {
        match self {
            ReadScope::All => true,
            ReadScope::Only(v) => v.iter().all(|r| r.repo_visible(org, repo)),
        }
    }

    /// Whether the principal is allowed the action on the `Org` itself
    /// (which makes the org visible even with no visible repo, D1).
    pub fn org_granted(&self, org: &str) -> bool {
        match self {
            ReadScope::All => true,
            ReadScope::Only(v) => v.iter().all(|r| r.org_granted(org)),
        }
    }

    /// Whether org `org` is visible, given the names of its existing repos
    /// (D1): a visible existing repo, or a grant on the org itself. Under
    /// `All` every existing org is visible.
    pub fn org_visible<'a>(&self, org: &str, repos: impl IntoIterator<Item = &'a str>) -> bool {
        self.is_all()
            || self.org_granted(org)
            || repos.into_iter().any(|r| self.repo_visible(org, r))
    }

    /// The named-read check (D5) for an org and/or repo filter, given
    /// `existing`: the unscoped `describe(org, repo)` rows, i.e. every
    /// existing repo the filter matches now. No filter (an aggregate read)
    /// and no existing match (today's answer) pass; otherwise the request
    /// is refused unless something it names is visible:
    /// - a repo filter (with or without the org): at least one existing
    ///   match is visible (a repo-only filter is then answered with the
    ///   visible matches, the push-down filters the rest);
    /// - an org filter alone: the org is visible (D1).
    pub fn check_named(
        &self,
        org: Option<&str>,
        repo: Option<&str>,
        existing: &[RepoInfo],
        action: &str,
    ) -> Result<()> {
        if self.is_all() || (org.is_none() && repo.is_none()) || existing.is_empty() {
            return Ok(());
        }
        let visible = match (org, repo) {
            (Some(o), None) => self.org_visible(o, existing.iter().map(|i| i.repo.as_str())),
            _ => existing.iter().any(|i| self.repo_visible(&i.org, &i.repo)),
        };
        if visible {
            Ok(())
        } else {
            Err(permission_denied(action))
        }
    }

    /// `rows` without the hidden repos.
    pub fn filter_rows<T>(&self, rows: Vec<T>, key: impl Fn(&T) -> (&str, &str)) -> Vec<T> {
        if self.is_all() {
            return rows;
        }
        rows.into_iter()
            .filter(|t| {
                let (o, r) = key(t);
                self.repo_visible(o, r)
            })
            .collect()
    }
}

/// The policy refusal of a named read (D9): it names only the action,
/// never the resource or a policy id. The same text the server's own
/// refusals use.
pub fn permission_denied(action: &str) -> StoreError {
    StoreError::PermissionDenied {
        msg: format!("permission denied for action `{action}`"),
        required_role: String::new(),
        action: action.to_owned(),
    }
}

/// Options for the scoped forms of the describe family (ADR 0013 D5): a
/// struct so the trait stays object-safe and can grow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadOpts {
    pub scope: ReadScope,
}

impl ReadOpts {
    pub fn scoped(scope: ReadScope) -> Self {
        Self { scope }
    }
}

/// The describe family under a scope, from the unscoped rows `rows` of
/// the same `(org, repo)` filter: the named check, then the filter.
pub(crate) fn describe_scoped(
    rows: Vec<RepoInfo>,
    org: Option<&str>,
    repo: Option<&str>,
    opts: &ReadOpts,
) -> Result<Vec<RepoInfo>> {
    opts.scope.check_named(org, repo, &rows, "read")?;
    Ok(opts.scope.filter_rows(rows, |i| (&i.org, &i.repo)))
}

/// A scoped view of a [`StoreRead`](crate::StoreRead) (ADR 0013 D5, story
/// 74b): every read through it sees only what `scope` makes visible.
///
/// - A node id names its repo (an org node names its org): an existing node
///   of a hidden repo or org is `PermissionDenied`; an id that does not
///   exist (in the inner store: for a snapshot, in its snapshot) keeps
///   today's answer.
/// - `roots`, `children`/`descendants` of a visible org and their page
///   totals list only visible repos and orgs; `count_nodes` sums the
///   visible repos' catalog counts (O(repos)).
/// - Name-filtered reads follow [`ReadScope::check_named`].
///
/// With [`ReadScope::All`] every call goes straight to the inner store.
pub struct ScopedView<'a> {
    inner: &'a dyn crate::StoreRead,
    scope: ReadScope,
    action: &'static str,
}

impl<'a> ScopedView<'a> {
    /// A view of `inner` through `scope`. Its own refusals (node ids,
    /// `file_tokens`, the describe family) name the action `read`; search
    /// and symbol search are refused by the store as `search`. The server
    /// builds one view per call with the scope of that call's action.
    pub fn new(inner: &'a dyn crate::StoreRead, scope: ReadScope) -> Self {
        Self {
            inner,
            scope,
            action: "read",
        }
    }

    /// The repos of org `org` that exist now (the catalog).
    fn org_repos(&self, org: &str) -> Result<Vec<RepoInfo>> {
        self.inner.describe(Some(org), None)
    }

    fn org_visible(&self, org: &str) -> Result<bool> {
        if self.scope.is_all() || self.scope.org_granted(org) {
            return Ok(true);
        }
        let rows = self.org_repos(org)?;
        Ok(self
            .scope
            .org_visible(org, rows.iter().map(|i| i.repo.as_str())))
    }

    /// Whether existing node `n` is visible: an org by D1, anything else by
    /// the repo it belongs to.
    fn node_visible(&self, n: &graph_core::Node) -> Result<bool> {
        use graph_core::NodeKind;
        if n.kind == NodeKind::Org {
            return self.org_visible(&n.name);
        }
        let mut repo: Option<graph_core::Node> = None;
        let mut org: Option<graph_core::Node> = None;
        if n.kind == NodeKind::Repo {
            repo = Some(n.clone());
        }
        for a in self.inner.ancestors(n.id)? {
            match a.kind {
                NodeKind::Repo if repo.is_none() => repo = Some(a),
                NodeKind::Org => org = Some(a),
                _ => {}
            }
        }
        match (org, repo) {
            (Some(o), Some(r)) => Ok(self.scope.repo_visible(&o.name, &r.name)),
            _ => Err(StoreError::Corrupt(format!(
                "node {} has no repo and org above it",
                n.id
            ))),
        }
    }

    /// The node-id rule: `Ok(None)` for an id that does not exist,
    /// `PermissionDenied` for a hidden one, the node for a visible one.
    fn gate(&self, id: graph_core::NodeId) -> Result<Option<graph_core::Node>> {
        let Some(n) = self.inner.get(id)? else {
            return Ok(None);
        };
        if self.scope.is_all() || self.node_visible(&n)? {
            Ok(Some(n))
        } else {
            Err(permission_denied(self.action))
        }
    }

    /// `children` of an org node, visible repos only.
    fn org_children(&self, org: &graph_core::Node) -> Result<Vec<graph_core::Node>> {
        let kids = self.inner.children(org.id)?;
        Ok(kids
            .into_iter()
            .filter(|r| self.scope.repo_visible(&org.name, &r.name))
            .collect())
    }

    /// `descendants` of an org node without the hidden repos' subtrees:
    /// each visible repo (in `children` order) followed by its own
    /// descendants, the depth-first order `descendants` documents, built
    /// without assuming how the inner store orders an org's descendants.
    fn org_descendants(&self, org: &graph_core::Node) -> Result<Vec<graph_core::Node>> {
        let mut out = Vec::new();
        for repo in self.org_children(org)? {
            let id = repo.id;
            out.push(repo);
            out.extend(self.inner.descendants(id)?);
        }
        Ok(out)
    }

    fn visible_rows(&self) -> Result<Vec<RepoInfo>> {
        let rows = self.inner.describe(None, None)?;
        Ok(self.scope.filter_rows(rows, |i| (&i.org, &i.repo)))
    }

    fn opts(&self, opts: &ReadOpts) -> ReadOpts {
        ReadOpts::scoped(self.scope.intersect(&opts.scope))
    }
}

impl crate::StoreRead for ScopedView<'_> {
    fn get(&self, id: graph_core::NodeId) -> Result<Option<graph_core::Node>> {
        self.gate(id)
    }
    fn parent(&self, id: graph_core::NodeId) -> Result<Option<graph_core::Node>> {
        self.gate(id)?;
        self.inner.parent(id)
    }
    fn count_nodes(&self, kind: graph_core::NodeKind) -> Result<usize> {
        use graph_core::NodeKind;
        if self.scope.is_all() {
            return self.inner.count_nodes(kind);
        }
        Ok(match kind {
            NodeKind::Org => self.roots()?.len(),
            NodeKind::Repo => self.visible_rows()?.len(),
            NodeKind::File => self.visible_rows()?.iter().map(|i| i.files).sum(),
            NodeKind::Symbol => self
                .visible_rows()?
                .iter()
                .flat_map(|i| i.languages.values())
                .map(|l| l.symbols)
                .sum(),
            NodeKind::Token => self
                .visible_rows()?
                .iter()
                .flat_map(|i| i.languages.values())
                .map(|l| l.tokens)
                .sum(),
        })
    }
    fn roots(&self) -> Result<Vec<graph_core::Node>> {
        let roots = self.inner.roots()?;
        if self.scope.is_all() {
            return Ok(roots);
        }
        let rows = self.inner.describe(None, None)?;
        Ok(roots
            .into_iter()
            .filter(|o| {
                self.scope.org_visible(
                    &o.name,
                    rows.iter()
                        .filter(|i| i.org == o.name)
                        .map(|i| i.repo.as_str()),
                )
            })
            .collect())
    }
    fn children(&self, id: graph_core::NodeId) -> Result<Vec<graph_core::Node>> {
        match self.gate(id)? {
            Some(n) if n.kind == graph_core::NodeKind::Org && !self.scope.is_all() => {
                self.org_children(&n)
            }
            _ => self.inner.children(id),
        }
    }
    fn descendants(&self, id: graph_core::NodeId) -> Result<Vec<graph_core::Node>> {
        match self.gate(id)? {
            Some(n) if n.kind == graph_core::NodeKind::Org && !self.scope.is_all() => {
                self.org_descendants(&n)
            }
            _ => self.inner.descendants(id),
        }
    }
    fn ancestors(&self, id: graph_core::NodeId) -> Result<Vec<graph_core::Node>> {
        // The ancestors of a visible node are visible by definition.
        self.gate(id)?;
        self.inner.ancestors(id)
    }
    fn children_page(
        &self,
        id: graph_core::NodeId,
        offset: usize,
        limit: usize,
    ) -> Result<crate::Page<graph_core::Node>> {
        match self.gate(id)? {
            Some(n) if n.kind == graph_core::NodeKind::Org && !self.scope.is_all() => {
                crate::api::page_slice(self.org_children(&n)?, offset, limit)
            }
            _ => self.inner.children_page(id, offset, limit),
        }
    }
    fn descendants_page(
        &self,
        id: graph_core::NodeId,
        offset: usize,
        limit: usize,
    ) -> Result<crate::Page<graph_core::Node>> {
        match self.gate(id)? {
            Some(n) if n.kind == graph_core::NodeKind::Org && !self.scope.is_all() => {
                crate::api::page_slice(self.org_descendants(&n)?, offset, limit)
            }
            _ => self.inner.descendants_page(id, offset, limit),
        }
    }
    fn file_tokens(
        &self,
        org: &str,
        repo: &str,
        path: &str,
    ) -> Result<Option<Vec<graph_core::Node>>> {
        let toks = self.inner.file_tokens(org, repo, path)?;
        if toks.is_some() && !self.scope.repo_visible(org, repo) {
            return Err(permission_denied(self.action));
        }
        Ok(toks)
    }
    fn describe(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>> {
        self.inner
            .describe_with(org, repo, &ReadOpts::scoped(self.scope.clone()))
    }
    fn describe_by_scan(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>> {
        self.inner
            .describe_by_scan_with(org, repo, &ReadOpts::scoped(self.scope.clone()))
    }
    fn describe_with(
        &self,
        org: Option<&str>,
        repo: Option<&str>,
        opts: &ReadOpts,
    ) -> Result<Vec<RepoInfo>> {
        self.inner.describe_with(org, repo, &self.opts(opts))
    }
    fn describe_by_scan_with(
        &self,
        org: Option<&str>,
        repo: Option<&str>,
        opts: &ReadOpts,
    ) -> Result<Vec<RepoInfo>> {
        self.inner
            .describe_by_scan_with(org, repo, &self.opts(opts))
    }
    fn search_symbols(&self, q: &crate::SymbolQuery) -> Result<Vec<crate::SymbolHit>> {
        let mut q = q.clone();
        q.scope = self.scope.intersect(&q.scope);
        self.inner.search_symbols(&q)
    }
    fn search(&self, q: &crate::Query) -> Result<Vec<crate::Hit>> {
        let mut q = q.clone();
        q.scope = self.scope.intersect(&q.scope);
        self.inner.search(&q)
    }
}
