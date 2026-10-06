//! `memory_graph.v1.Store`: `Hello`, every `StoreRead` method under a
//! `View`, and the snapshot handles (ADR 0004 D1/D8).
use super::{reply, Ctx};
use crate::conn::conn_id;
use crate::repeats::{QueryKey, ReadRpc};
use crate::{DEFAULT_SEARCH_LIMIT, SERVER_VERSION};
use graph_core::{Node, NodeKind};
use graph_proto::convert::Wire;
use graph_proto::error::WireError;
use graph_proto::{pb, ConvertError, PROTOCOL_VERSION, STREAM_BATCH_NODES};
use graph_store::{Query, StoreError, SymbolQuery};
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

pub struct StoreService {
    pub ctx: Arc<Ctx>,
}

/// The request's [`QueryKey`] with its read view cleared: a `Local` and a
/// `Linearizable` read of the same query at the same applied index give the
/// same answer, and a snapshot read's generation is the handle's frozen
/// applied index (its `ReadMeta`), so the view never changes the answer at a
/// given generation.
macro_rules! view_free_key {
    ($rpc:expr, $r:ident) => {{
        let view = $r.view.take();
        let key = QueryKey::of($rpc, &$r);
        $r.view = view;
        key
    }};
}

type NodeStream = Pin<Box<dyn Stream<Item = Result<pb::NodeBatch, Status>> + Send>>;

fn batches(nodes: Vec<Node>) -> NodeStream {
    let mut out: Vec<Result<pb::NodeBatch, Status>> = Vec::new();
    let mut nodes = nodes.into_iter().peekable();
    if nodes.peek().is_none() {
        out.push(Ok(pb::NodeBatch::from(Vec::<Node>::new())));
    }
    while nodes.peek().is_some() {
        let chunk: Vec<Node> = nodes.by_ref().take(STREAM_BATCH_NODES).collect();
        out.push(Ok(pb::NodeBatch::from(chunk)));
    }
    Box::pin(tokio_stream::iter(out))
}

fn nodes_into(v: Vec<Node>) -> Vec<pb::Node> {
    v.into_iter().map(Into::into).collect()
}

fn usize_of(what: &str, v: u64) -> Result<usize, Status> {
    usize::try_from(v)
        .map_err(|_| ConvertError(format!("`{what}` = {v} does not fit usize")).into())
}

#[tonic::async_trait]
impl pb::store_server::Store for StoreService {
    async fn hello(
        &self,
        req: Request<pb::HelloRequest>,
    ) -> Result<Response<pb::HelloResponse>, Status> {
        let r = req.into_inner();
        if r.protocol_version != PROTOCOL_VERSION {
            return Err(WireError::Protocol(format!(
                "client speaks protocol version {}, this server speaks {PROTOCOL_VERSION}",
                r.protocol_version
            ))
            .into());
        }
        tracing::debug!(client = %r.client_version, "hello");
        let leader = self.ctx.raft.leader();
        Ok(Response::new(pb::HelloResponse {
            protocol_version: self.ctx.info.hello_protocol_version,
            server_version: SERVER_VERSION.into(),
            store_format_version: graph_store::SCHEMA_VERSION,
            extractors_hash: self.ctx.info.extractors_hash.clone(),
            node_id: self.ctx.info.node_id,
            leader_id: leader.id,
            leader_addr: leader.addr,
            cluster_id: self.ctx.info.cluster_id(),
        }))
    }

    async fn get(&self, req: Request<pb::GetRequest>) -> Result<Response<pb::GetResponse>, Status> {
        let mut r = req.into_inner();
        let key = view_free_key!(ReadRpc::Get, r);
        let (node, meta) = self.ctx.read(r.view, move |s| s.get(r.id)).await?;
        self.ctx.note_query(key, &meta);
        Ok(reply(
            pb::GetResponse {
                node: node.map(Into::into),
            },
            meta,
        ))
    }

    async fn parent(
        &self,
        req: Request<pb::ParentRequest>,
    ) -> Result<Response<pb::ParentResponse>, Status> {
        let mut r = req.into_inner();
        let key = view_free_key!(ReadRpc::Parent, r);
        let (node, meta) = self.ctx.read(r.view, move |s| s.parent(r.id)).await?;
        self.ctx.note_query(key, &meta);
        Ok(reply(
            pb::ParentResponse {
                node: node.map(Into::into),
            },
            meta,
        ))
    }

    async fn count_nodes(
        &self,
        req: Request<pb::CountNodesRequest>,
    ) -> Result<Response<pb::CountNodesResponse>, Status> {
        let mut r = req.into_inner();
        let key = view_free_key!(ReadRpc::CountNodes, r);
        let kind = Wire::<NodeKind>::try_from(r.kind)?.0;
        let (count, meta) = self.ctx.read(r.view, move |s| s.count_nodes(kind)).await?;
        self.ctx.note_query(key, &meta);
        Ok(reply(
            pb::CountNodesResponse {
                count: count as u64,
            },
            meta,
        ))
    }

    async fn roots(
        &self,
        req: Request<pb::RootsRequest>,
    ) -> Result<Response<pb::RootsResponse>, Status> {
        let mut r = req.into_inner();
        let key = view_free_key!(ReadRpc::Roots, r);
        let (nodes, meta) = self.ctx.read(r.view, |s| s.roots()).await?;
        self.ctx.note_query(key, &meta);
        Ok(reply(
            pb::RootsResponse {
                nodes: nodes_into(nodes),
            },
            meta,
        ))
    }

    async fn children(
        &self,
        req: Request<pb::ChildrenRequest>,
    ) -> Result<Response<pb::ChildrenResponse>, Status> {
        let mut r = req.into_inner();
        let key = view_free_key!(ReadRpc::Children, r);
        let (nodes, meta) = self.ctx.read(r.view, move |s| s.children(r.id)).await?;
        self.ctx.note_query(key, &meta);
        Ok(reply(
            pb::ChildrenResponse {
                nodes: nodes_into(nodes),
            },
            meta,
        ))
    }

    async fn children_page(
        &self,
        req: Request<pb::ChildrenPageRequest>,
    ) -> Result<Response<pb::NodePage>, Status> {
        let mut r = req.into_inner();
        let key = view_free_key!(ReadRpc::ChildrenPage, r);
        let (offset, limit) = (usize_of("offset", r.offset)?, usize_of("limit", r.limit)?);
        let (page, meta) = self
            .ctx
            .read(r.view, move |s| s.children_page(r.id, offset, limit))
            .await?;
        self.ctx.note_query(key, &meta);
        Ok(reply(page.into(), meta))
    }

    type DescendantsStream = NodeStream;

    async fn descendants(
        &self,
        req: Request<pb::DescendantsRequest>,
    ) -> Result<Response<Self::DescendantsStream>, Status> {
        let mut r = req.into_inner();
        let key = view_free_key!(ReadRpc::Descendants, r);
        let (nodes, meta) = self.ctx.read(r.view, move |s| s.descendants(r.id)).await?;
        self.ctx.note_query(key, &meta);
        Ok(reply(batches(nodes), meta))
    }

    async fn descendants_page(
        &self,
        req: Request<pb::DescendantsPageRequest>,
    ) -> Result<Response<pb::NodePage>, Status> {
        let mut r = req.into_inner();
        let key = view_free_key!(ReadRpc::DescendantsPage, r);
        let (offset, limit) = (usize_of("offset", r.offset)?, usize_of("limit", r.limit)?);
        let (page, meta) = self
            .ctx
            .read(r.view, move |s| s.descendants_page(r.id, offset, limit))
            .await?;
        self.ctx.note_query(key, &meta);
        Ok(reply(page.into(), meta))
    }

    async fn ancestors(
        &self,
        req: Request<pb::AncestorsRequest>,
    ) -> Result<Response<pb::AncestorsResponse>, Status> {
        let mut r = req.into_inner();
        let key = view_free_key!(ReadRpc::Ancestors, r);
        let (nodes, meta) = self.ctx.read(r.view, move |s| s.ancestors(r.id)).await?;
        self.ctx.note_query(key, &meta);
        Ok(reply(
            pb::AncestorsResponse {
                nodes: nodes_into(nodes),
            },
            meta,
        ))
    }

    type FileTokensStream = NodeStream;

    async fn file_tokens(
        &self,
        req: Request<pb::FileTokensRequest>,
    ) -> Result<Response<Self::FileTokensStream>, Status> {
        let mut r = req.into_inner();
        let key = view_free_key!(ReadRpc::FileTokens, r);
        let (toks, meta) = self
            .ctx
            .read(r.view, move |s| s.file_tokens(&r.org, &r.repo, &r.path))
            .await?;
        self.ctx.note_query(key, &meta);
        Ok(reply(
            match toks {
                Some(nodes) => batches(nodes),
                None => Box::pin(tokio_stream::iter([Ok(pb::NodeBatch {
                    nodes: vec![],
                    not_found: true,
                })])),
            },
            meta,
        ))
    }

    async fn describe(
        &self,
        req: Request<pb::DescribeRequest>,
    ) -> Result<Response<pb::DescribeResponse>, Status> {
        let mut r = req.into_inner();
        let key = view_free_key!(ReadRpc::Describe, r);
        let (repos, meta) = self
            .ctx
            .read(r.view, move |s| {
                s.describe(r.org.as_deref(), r.repo.as_deref())
            })
            .await?;
        self.ctx.note_query(key, &meta);
        Ok(reply(
            pb::DescribeResponse {
                repos: repos.into_iter().map(Into::into).collect(),
            },
            meta,
        ))
    }

    async fn describe_by_scan(
        &self,
        req: Request<pb::DescribeRequest>,
    ) -> Result<Response<pb::DescribeResponse>, Status> {
        let mut r = req.into_inner();
        let key = view_free_key!(ReadRpc::DescribeByScan, r);
        let (repos, meta) = self
            .ctx
            .read(r.view, move |s| {
                s.describe_by_scan(r.org.as_deref(), r.repo.as_deref())
            })
            .await?;
        self.ctx.note_query(key, &meta);
        Ok(reply(
            pb::DescribeResponse {
                repos: repos.into_iter().map(Into::into).collect(),
            },
            meta,
        ))
    }

    /// #165: the leader's gaps, from its registry: writes (`Index`) are
    /// forwarded to the leader, so its extractors are the ones a client's
    /// run meets. A follower forwards this call there like a write
    /// (`mg-forwarded-by` stops loops); the leader answers from its store.
    async fn extractor_gaps(
        &self,
        req: Request<pb::ExtractorGapsRequest>,
    ) -> Result<Response<pb::ExtractorGapsResponse>, Status> {
        use crate::forward::{forward_error, within, Forwarder, Route, FORWARD_UNARY_TIMEOUT};
        if let Route::Leader { addr, .. } = self.ctx.fwd.route(&self.ctx.raft, &req)? {
            let deadline = self.ctx.fwd.deadline(req.metadata(), FORWARD_UNARY_TIMEOUT);
            let mut client = self.ctx.fwd.store_client(&addr)?;
            let resp = within(
                deadline,
                client.extractor_gaps(Forwarder::request(req.into_inner(), deadline)),
            )
            .await
            .map_err(forward_error)?;
            return Ok(Response::new(resp.into_inner()));
        }
        let r = req.into_inner();
        let slot = Arc::clone(&self.ctx.slot);
        let gaps = tokio::task::spawn_blocking(move || {
            slot.with_store_read(|s| {
                graph_store::Store::extractor_gaps(s, r.org.as_deref(), r.repo.as_deref())
            })
        })
        .await
        .map_err(|e| Status::internal(format!("extractor_gaps task failed: {e}")))?
        .map_err(|e| graph_proto::store_error_to_status(&e))?;
        Ok(Response::new(pb::ExtractorGapsResponse {
            gaps: gaps.into_iter().map(Into::into).collect(),
        }))
    }

    async fn search_symbols(
        &self,
        req: Request<pb::SearchSymbolsRequest>,
    ) -> Result<Response<pb::SearchSymbolsResponse>, Status> {
        let mut r = req.into_inner();
        // The key sees the default limit applied, so an omitted limit and
        // the default one are the same query.
        let defaulted = r.query.as_ref().is_some_and(|q| q.limit.is_none());
        if let Some(q) = r.query.as_mut() {
            q.limit.get_or_insert(DEFAULT_SEARCH_LIMIT as u64);
        }
        let key = view_free_key!(ReadRpc::SearchSymbols, r);
        let q: SymbolQuery = r
            .query
            .ok_or_else(|| {
                ConvertError("required field `SearchSymbolsRequest.query` is missing".into())
            })?
            .try_into()?;
        // Only a page the default limit filled is reported as defaulted, so a
        // small answer needs no paging round trips.
        let (hits, meta) = self.ctx.read(r.view, move |s| s.search_symbols(&q)).await?;
        self.ctx.note_query(key, &meta);
        let hits_len = hits.len();
        Ok(reply(
            pb::SearchSymbolsResponse {
                hits: hits.into_iter().map(Into::into).collect(),
                applied_default_limit: defaulted && hits_len >= DEFAULT_SEARCH_LIMIT,
            },
            meta,
        ))
    }

    async fn search(
        &self,
        req: Request<pb::SearchRequest>,
    ) -> Result<Response<pb::SearchResponse>, Status> {
        let mut r = req.into_inner();
        // The key sees the default limit applied, so an omitted limit and
        // the default one are the same query.
        let defaulted = r.query.as_ref().is_some_and(|q| q.limit.is_none());
        if let Some(q) = r.query.as_mut() {
            q.limit.get_or_insert(DEFAULT_SEARCH_LIMIT as u64);
        }
        let key = view_free_key!(ReadRpc::Search, r);
        let q: Query = r
            .query
            .ok_or_else(|| ConvertError("required field `SearchRequest.query` is missing".into()))?
            .try_into()?;
        // Only a page the default limit filled is reported as defaulted, so a
        // small answer needs no paging round trips.
        let (hits, meta) = self.ctx.read(r.view, move |s| s.search(&q)).await?;
        self.ctx.note_query(key, &meta);
        let hits_len = hits.len();
        Ok(reply(
            pb::SearchResponse {
                hits: hits.into_iter().map(Into::into).collect(),
                applied_default_limit: defaulted && hits_len >= DEFAULT_SEARCH_LIMIT,
            },
            meta,
        ))
    }

    async fn open_snapshot(
        &self,
        req: Request<pb::OpenSnapshotRequest>,
    ) -> Result<Response<pb::OpenSnapshotResponse>, Status> {
        let conn = conn_id(&req);
        // A linearizable handle (D8): the barrier runs first, so the frozen
        // view holds every write acknowledged before the open.
        let linearizable = req
            .metadata()
            .get(graph_proto::READ_MODE_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("linearizable"));
        if linearizable {
            self.ctx.linearizable_barrier().await?;
        }
        // Frozen into the handle: its reads report the state it opened at.
        let mut meta = self.ctx.raft.read_meta();
        meta.stale_possible &= !linearizable;
        let slot = Arc::clone(&self.ctx.slot);
        let id = tokio::task::spawn_blocking(move || {
            slot.with_store_read(|s| Ok(slot.snapshots().open_with_meta(conn, s, Some(meta))))
                .map_err(|e| graph_proto::store_error_to_status(&e))?
        })
        .await
        .map_err(|e| Status::internal(format!("blocking task failed: {e}")))??;
        Ok(Response::new(pb::OpenSnapshotResponse { snapshot_id: id }))
    }

    async fn close_snapshot(
        &self,
        req: Request<pb::CloseSnapshotRequest>,
    ) -> Result<Response<pb::CloseSnapshotResponse>, Status> {
        self.ctx
            .slot
            .snapshots()
            .close(req.into_inner().snapshot_id);
        Ok(Response::new(pb::CloseSnapshotResponse {}))
    }

    async fn snapshot_stats(
        &self,
        _req: Request<pb::SnapshotStatsRequest>,
    ) -> Result<Response<pb::SnapshotStatsResponse>, Status> {
        let stats = self
            .ctx
            .blocking(|slot| {
                slot.with_store(|s| Ok::<_, StoreError>(graph_store::Store::snapshot_stats(s)))
            })
            .await?;
        Ok(Response::new(pb::SnapshotStatsResponse {
            stats: Some(stats.into()),
        }))
    }
}
