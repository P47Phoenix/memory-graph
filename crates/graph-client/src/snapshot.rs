//! [`RemoteSnapshot`]: a server-side snapshot handle as a `StoreRead`.
use crate::conn::{block_on, Conn};
use crate::reads;
use graph_core::{Node, NodeId, NodeKind};
use graph_proto::View;
use graph_store::{Hit, Page, Query, RepoInfo, StoreError, StoreRead, SymbolHit, SymbolQuery};
use std::sync::Arc;

type Result<T> = std::result::Result<T, StoreError>;

/// Reads through `View.snapshot_id`: one frozen state, whatever the server
/// commits meanwhile, until the handle expires (the store's snapshot max
/// age; then every read is `SnapshotExpired`). Dropping it closes the
/// handle on the server, best effort.
pub struct RemoteSnapshot {
    conn: Arc<Conn>,
    rt: Arc<tokio::runtime::Runtime>,
    id: u64,
}

impl RemoteSnapshot {
    pub(crate) fn new(conn: Arc<Conn>, rt: Arc<tokio::runtime::Runtime>, id: u64) -> Self {
        Self { conn, rt, id }
    }

    /// The server-side handle id.
    pub fn id(&self) -> u64 {
        self.id
    }

    fn view(&self) -> View {
        View::Snapshot(self.id)
    }

    fn run<F: std::future::Future>(&self, f: F) -> F::Output {
        block_on(&self.rt, f)
    }
}

impl Drop for RemoteSnapshot {
    fn drop(&mut self) {
        // Fire and forget on the runtime: never block in a destructor, and
        // a handle the server did not hear about is reaped by its TTL.
        let conn = Arc::clone(&self.conn);
        let id = self.id;
        self.rt.spawn(async move {
            let _ = reads::close_snapshot(&conn, id).await;
        });
    }
}

impl StoreRead for RemoteSnapshot {
    fn get(&self, id: NodeId) -> Result<Option<Node>> {
        self.run(reads::get(&self.conn, self.view(), id))
    }
    fn parent(&self, id: NodeId) -> Result<Option<Node>> {
        self.run(reads::parent(&self.conn, self.view(), id))
    }
    fn count_nodes(&self, kind: NodeKind) -> Result<usize> {
        self.run(reads::count_nodes(&self.conn, self.view(), kind))
    }
    fn roots(&self) -> Result<Vec<Node>> {
        self.run(reads::roots(&self.conn, self.view()))
    }
    fn children(&self, id: NodeId) -> Result<Vec<Node>> {
        self.run(reads::children(&self.conn, self.view(), id))
    }
    fn descendants(&self, id: NodeId) -> Result<Vec<Node>> {
        self.run(reads::descendants(&self.conn, self.view(), id))
    }
    fn ancestors(&self, id: NodeId) -> Result<Vec<Node>> {
        self.run(reads::ancestors(&self.conn, self.view(), id))
    }
    fn children_page(&self, id: NodeId, offset: usize, limit: usize) -> Result<Page<Node>> {
        self.run(reads::children_page(
            &self.conn,
            self.view(),
            id,
            offset,
            limit,
        ))
    }
    fn descendants_page(&self, id: NodeId, offset: usize, limit: usize) -> Result<Page<Node>> {
        self.run(reads::descendants_page(
            &self.conn,
            self.view(),
            id,
            offset,
            limit,
        ))
    }
    fn file_tokens(&self, org: &str, repo: &str, path: &str) -> Result<Option<Vec<Node>>> {
        self.run(reads::file_tokens(&self.conn, self.view(), org, repo, path))
    }
    fn describe(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>> {
        self.run(reads::describe(&self.conn, self.view(), org, repo, false))
    }
    fn describe_by_scan(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>> {
        self.run(reads::describe(&self.conn, self.view(), org, repo, true))
    }
    fn search_symbols(&self, q: &SymbolQuery) -> Result<Vec<SymbolHit>> {
        self.run(reads::search_symbols(&self.conn, self.view(), q))
    }
    fn search(&self, q: &Query) -> Result<Vec<Hit>> {
        self.run(reads::search(&self.conn, self.view(), q))
    }
}
