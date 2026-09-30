//! The in-serve MCP backend (ADR 0005 D3): a [`StoreRead`] whose every
//! method is one call of this node's own `memory_graph.v1.Store` service
//! implementation, made in process with a built `tonic::Request`. So an
//! MCP read takes exactly the path a gRPC client's does: the same
//! [`StoreSlot`](crate::StoreSlot) access, the linearizable barrier
//! (`Admin.ReadIndex` forwarded to the leader from a follower), the
//! `ReadMeta` of the answer, and the snapshot-install `UNAVAILABLE`.
//!
//! The tool core is synchronous, so a call runs on the blocking pool and
//! drives the async service with [`Handle::block_on`] (allowed there; never
//! from a runtime worker). `RemoteStore` is not used inside `serve`.
use crate::services::store::StoreService;
use graph_core::{Node, NodeId, NodeKind};
use graph_proto::convert::enum_i32;
use graph_proto::pb::store_server::Store as StoreRpc;
use graph_proto::{pb, status_to_store_error, ConvertError, ReadMeta, View};
use graph_store::{Hit, Page, Query, RepoInfo, StoreError, StoreRead, SymbolHit, SymbolQuery};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status};

type Result<T> = std::result::Result<T, StoreError>;

/// `LOCAL` or `LINEARIZABLE` reads through the in-process Store service.
pub struct InProcessStore {
    svc: Arc<StoreService>,
    rt: Handle,
    view: View,
    /// Reads so far whose `ReadMeta` said `stale_possible`.
    stale: Arc<AtomicU64>,
}

fn conv<T, M: TryInto<T, Error = ConvertError>>(m: M) -> Result<T> {
    m.try_into().map_err(StoreError::from)
}

fn nodes(v: Vec<pb::Node>) -> Result<Vec<Node>> {
    v.into_iter().map(conv).collect()
}

fn err(s: Status) -> StoreError {
    status_to_store_error(&s)
}

impl InProcessStore {
    pub fn new(svc: Arc<StoreService>, rt: Handle, view: View) -> Self {
        Self {
            svc,
            rt,
            view,
            stale: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The counter of `stale_possible` reads, for
    /// [`graph_mcp::StoreBackend::remote`].
    pub fn stale_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.stale)
    }

    fn view(&self) -> Option<pb::View> {
        Some(self.view.into())
    }

    fn note(&self, meta: Option<ReadMeta>) {
        if meta.is_some_and(|m| m.stale_possible) {
            self.stale.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Run one unary call, note its `ReadMeta`, return the message.
    fn unary<M, F>(&self, f: F) -> Result<M>
    where
        F: std::future::Future<Output = std::result::Result<Response<M>, Status>>,
    {
        let r = self.rt.block_on(f).map_err(err)?;
        self.note(ReadMeta::from_metadata(r.metadata()));
        Ok(r.into_inner())
    }

    /// Run one streaming call and drain it; `None` when the first batch
    /// says `not_found`.
    fn streamed<S, F>(&self, f: F) -> Result<Option<Vec<Node>>>
    where
        S: tokio_stream::Stream<Item = std::result::Result<pb::NodeBatch, Status>> + Unpin,
        F: std::future::Future<Output = std::result::Result<Response<S>, Status>>,
    {
        self.rt.block_on(async {
            let r = f.await.map_err(err)?;
            self.note(ReadMeta::from_metadata(r.metadata()));
            let mut stream = r.into_inner();
            let mut out = Vec::new();
            let mut first = true;
            while let Some(b) = stream.next().await {
                let b = b.map_err(err)?;
                if first && b.not_found {
                    return Ok(None);
                }
                first = false;
                for n in b.nodes {
                    out.push(conv(n)?);
                }
            }
            Ok(Some(out))
        })
    }

    /// `search` / `search_symbols` without a limit return every row, as
    /// embedded: when the service applied its default limit, the rows are
    /// fetched again page by page under one snapshot handle (closed
    /// afterwards), like `RemoteStore` does.
    fn all_pages<T, F>(
        &self,
        offset: Option<usize>,
        first: (Vec<T>, bool),
        page: F,
    ) -> Result<Vec<T>>
    where
        F: Fn(View, usize, usize) -> Result<Vec<T>>,
    {
        let (rows, applied) = first;
        if !applied {
            return Ok(rows);
        }
        let size = rows.len().max(1);
        let mut req = Request::new(pb::OpenSnapshotRequest {});
        if self.view == View::Linearizable {
            req.metadata_mut().insert(
                graph_proto::READ_MODE_HEADER,
                tonic::metadata::MetadataValue::from_static("linearizable"),
            );
        }
        let id = self
            .rt
            .block_on(self.svc.open_snapshot(req))
            .map_err(err)?
            .into_inner()
            .snapshot_id;
        let result = (|| {
            let mut out = Vec::new();
            let mut at = offset.unwrap_or(0);
            loop {
                let p = page(View::Snapshot(id), size, at)?;
                let n = p.len();
                out.extend(p);
                if n < size {
                    return Ok(out);
                }
                at += n;
            }
        })();
        let _ = self.rt.block_on(
            self.svc
                .close_snapshot(Request::new(pb::CloseSnapshotRequest { snapshot_id: id })),
        );
        result
    }

    fn search_page(&self, view: View, q: &Query) -> Result<(Vec<Hit>, bool)> {
        let r = self.unary(self.svc.search(Request::new(pb::SearchRequest {
            view: Some(view.into()),
            query: Some(q.clone().into()),
        })))?;
        let hits = r.hits.into_iter().map(conv).collect::<Result<Vec<_>>>()?;
        Ok((hits, r.applied_default_limit))
    }

    fn symbols_page(&self, view: View, q: &SymbolQuery) -> Result<(Vec<SymbolHit>, bool)> {
        let r = self.unary(
            self.svc
                .search_symbols(Request::new(pb::SearchSymbolsRequest {
                    view: Some(view.into()),
                    query: Some(q.clone().into()),
                })),
        )?;
        let hits = r.hits.into_iter().map(conv).collect::<Result<Vec<_>>>()?;
        Ok((hits, r.applied_default_limit))
    }
}

impl StoreRead for InProcessStore {
    fn get(&self, id: NodeId) -> Result<Option<Node>> {
        let r = self.unary(self.svc.get(Request::new(pb::GetRequest {
            view: self.view(),
            id,
        })))?;
        r.node.map(conv).transpose()
    }

    fn parent(&self, id: NodeId) -> Result<Option<Node>> {
        let r = self.unary(self.svc.parent(Request::new(pb::ParentRequest {
            view: self.view(),
            id,
        })))?;
        r.node.map(conv).transpose()
    }

    fn count_nodes(&self, kind: NodeKind) -> Result<usize> {
        let r = self.unary(self.svc.count_nodes(Request::new(pb::CountNodesRequest {
            view: self.view(),
            kind: enum_i32::<_, pb::NodeKind>(kind),
        })))?;
        usize::try_from(r.count)
            .map_err(|_| StoreError::Protocol("count does not fit usize".into()))
    }

    fn roots(&self) -> Result<Vec<Node>> {
        let r = self.unary(
            self.svc
                .roots(Request::new(pb::RootsRequest { view: self.view() })),
        )?;
        nodes(r.nodes)
    }

    fn children(&self, id: NodeId) -> Result<Vec<Node>> {
        let r = self.unary(self.svc.children(Request::new(pb::ChildrenRequest {
            view: self.view(),
            id,
        })))?;
        nodes(r.nodes)
    }

    fn descendants(&self, id: NodeId) -> Result<Vec<Node>> {
        Ok(self
            .streamed(self.svc.descendants(Request::new(pb::DescendantsRequest {
                view: self.view(),
                id,
            })))?
            .unwrap_or_default())
    }

    fn ancestors(&self, id: NodeId) -> Result<Vec<Node>> {
        let r = self.unary(self.svc.ancestors(Request::new(pb::AncestorsRequest {
            view: self.view(),
            id,
        })))?;
        nodes(r.nodes)
    }

    fn children_page(&self, id: NodeId, offset: usize, limit: usize) -> Result<Page<Node>> {
        let r = self.unary(
            self.svc
                .children_page(Request::new(pb::ChildrenPageRequest {
                    view: self.view(),
                    id,
                    offset: offset as u64,
                    limit: limit as u64,
                })),
        )?;
        conv(r)
    }

    fn descendants_page(&self, id: NodeId, offset: usize, limit: usize) -> Result<Page<Node>> {
        let r = self.unary(self.svc.descendants_page(Request::new(
            pb::DescendantsPageRequest {
                view: self.view(),
                id,
                offset: offset as u64,
                limit: limit as u64,
            },
        )))?;
        conv(r)
    }

    fn file_tokens(&self, org: &str, repo: &str, path: &str) -> Result<Option<Vec<Node>>> {
        self.streamed(self.svc.file_tokens(Request::new(pb::FileTokensRequest {
            view: self.view(),
            org: org.into(),
            repo: repo.into(),
            path: path.into(),
        })))
    }

    fn describe(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>> {
        let r = self.unary(self.svc.describe(Request::new(pb::DescribeRequest {
            view: self.view(),
            org: org.map(str::to_string),
            repo: repo.map(str::to_string),
        })))?;
        r.repos.into_iter().map(conv).collect()
    }

    fn describe_by_scan(&self, org: Option<&str>, repo: Option<&str>) -> Result<Vec<RepoInfo>> {
        let r = self.unary(self.svc.describe_by_scan(Request::new(pb::DescribeRequest {
            view: self.view(),
            org: org.map(str::to_string),
            repo: repo.map(str::to_string),
        })))?;
        r.repos.into_iter().map(conv).collect()
    }

    fn search_symbols(&self, q: &SymbolQuery) -> Result<Vec<SymbolHit>> {
        let first = self.symbols_page(self.view, q)?;
        self.all_pages(q.offset, first, |view, size, at| {
            let mut pq = q.clone();
            pq.limit = Some(size);
            pq.offset = Some(at);
            self.symbols_page(view, &pq).map(|(h, _)| h)
        })
    }

    fn search(&self, q: &Query) -> Result<Vec<Hit>> {
        let first = self.search_page(self.view, q)?;
        self.all_pages(q.offset, first, |view, size, at| {
            let mut pq = q.clone();
            pq.limit = Some(size);
            pq.offset = Some(at);
            self.search_page(view, &pq).map(|(h, _)| h)
        })
    }
}
