//! `memory_graph.v1.Store`: `Hello`, every `StoreRead` method under a
//! `View`, and the snapshot handles (ADR 0004 D1/D8).
use super::Ctx;
use crate::conn::conn_id;
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
            cluster_id: self.ctx.info.cluster_id.clone(),
        }))
    }

    async fn get(&self, req: Request<pb::GetRequest>) -> Result<Response<pb::GetResponse>, Status> {
        let r = req.into_inner();
        let node = self.ctx.read(r.view, move |s| s.get(r.id)).await?;
        Ok(Response::new(pb::GetResponse {
            node: node.map(Into::into),
        }))
    }

    async fn parent(
        &self,
        req: Request<pb::ParentRequest>,
    ) -> Result<Response<pb::ParentResponse>, Status> {
        let r = req.into_inner();
        let node = self.ctx.read(r.view, move |s| s.parent(r.id)).await?;
        Ok(Response::new(pb::ParentResponse {
            node: node.map(Into::into),
        }))
    }

    async fn count_nodes(
        &self,
        req: Request<pb::CountNodesRequest>,
    ) -> Result<Response<pb::CountNodesResponse>, Status> {
        let r = req.into_inner();
        let kind = Wire::<NodeKind>::try_from(r.kind)?.0;
        let count = self.ctx.read(r.view, move |s| s.count_nodes(kind)).await?;
        Ok(Response::new(pb::CountNodesResponse {
            count: count as u64,
        }))
    }

    async fn roots(
        &self,
        req: Request<pb::RootsRequest>,
    ) -> Result<Response<pb::RootsResponse>, Status> {
        let r = req.into_inner();
        let nodes = self.ctx.read(r.view, |s| s.roots()).await?;
        Ok(Response::new(pb::RootsResponse {
            nodes: nodes_into(nodes),
        }))
    }

    async fn children(
        &self,
        req: Request<pb::ChildrenRequest>,
    ) -> Result<Response<pb::ChildrenResponse>, Status> {
        let r = req.into_inner();
        let nodes = self.ctx.read(r.view, move |s| s.children(r.id)).await?;
        Ok(Response::new(pb::ChildrenResponse {
            nodes: nodes_into(nodes),
        }))
    }

    async fn children_page(
        &self,
        req: Request<pb::ChildrenPageRequest>,
    ) -> Result<Response<pb::NodePage>, Status> {
        let r = req.into_inner();
        let (offset, limit) = (usize_of("offset", r.offset)?, usize_of("limit", r.limit)?);
        let page = self
            .ctx
            .read(r.view, move |s| s.children_page(r.id, offset, limit))
            .await?;
        Ok(Response::new(page.into()))
    }

    type DescendantsStream = NodeStream;

    async fn descendants(
        &self,
        req: Request<pb::DescendantsRequest>,
    ) -> Result<Response<Self::DescendantsStream>, Status> {
        let r = req.into_inner();
        let nodes = self.ctx.read(r.view, move |s| s.descendants(r.id)).await?;
        Ok(Response::new(batches(nodes)))
    }

    async fn descendants_page(
        &self,
        req: Request<pb::DescendantsPageRequest>,
    ) -> Result<Response<pb::NodePage>, Status> {
        let r = req.into_inner();
        let (offset, limit) = (usize_of("offset", r.offset)?, usize_of("limit", r.limit)?);
        let page = self
            .ctx
            .read(r.view, move |s| s.descendants_page(r.id, offset, limit))
            .await?;
        Ok(Response::new(page.into()))
    }

    async fn ancestors(
        &self,
        req: Request<pb::AncestorsRequest>,
    ) -> Result<Response<pb::AncestorsResponse>, Status> {
        let r = req.into_inner();
        let nodes = self.ctx.read(r.view, move |s| s.ancestors(r.id)).await?;
        Ok(Response::new(pb::AncestorsResponse {
            nodes: nodes_into(nodes),
        }))
    }

    type FileTokensStream = NodeStream;

    async fn file_tokens(
        &self,
        req: Request<pb::FileTokensRequest>,
    ) -> Result<Response<Self::FileTokensStream>, Status> {
        let r = req.into_inner();
        let toks = self
            .ctx
            .read(r.view, move |s| s.file_tokens(&r.org, &r.repo, &r.path))
            .await?;
        Ok(Response::new(match toks {
            Some(nodes) => batches(nodes),
            None => Box::pin(tokio_stream::iter([Ok(pb::NodeBatch {
                nodes: vec![],
                not_found: true,
            })])),
        }))
    }

    async fn describe(
        &self,
        req: Request<pb::DescribeRequest>,
    ) -> Result<Response<pb::DescribeResponse>, Status> {
        let r = req.into_inner();
        let repos = self
            .ctx
            .read(r.view, move |s| {
                s.describe(r.org.as_deref(), r.repo.as_deref())
            })
            .await?;
        Ok(Response::new(pb::DescribeResponse {
            repos: repos.into_iter().map(Into::into).collect(),
        }))
    }

    async fn describe_by_scan(
        &self,
        req: Request<pb::DescribeRequest>,
    ) -> Result<Response<pb::DescribeResponse>, Status> {
        let r = req.into_inner();
        let repos = self
            .ctx
            .read(r.view, move |s| {
                s.describe_by_scan(r.org.as_deref(), r.repo.as_deref())
            })
            .await?;
        Ok(Response::new(pb::DescribeResponse {
            repos: repos.into_iter().map(Into::into).collect(),
        }))
    }

    async fn search_symbols(
        &self,
        req: Request<pb::SearchSymbolsRequest>,
    ) -> Result<Response<pb::SearchSymbolsResponse>, Status> {
        let r = req.into_inner();
        let mut q: SymbolQuery = r
            .query
            .ok_or_else(|| {
                ConvertError("required field `SearchSymbolsRequest.query` is missing".into())
            })?
            .try_into()?;
        // Only a page the default limit filled is reported, so a small answer
        // needs no paging round trips (a full one may have more behind it).
        let defaulted = q.limit.is_none();
        if defaulted {
            q.limit = Some(DEFAULT_SEARCH_LIMIT);
        }
        let hits = self.ctx.read(r.view, move |s| s.search_symbols(&q)).await?;
        let hits_len = hits.len();
        Ok(Response::new(pb::SearchSymbolsResponse {
            hits: hits.into_iter().map(Into::into).collect(),
            applied_default_limit: defaulted && hits_len >= DEFAULT_SEARCH_LIMIT,
        }))
    }

    async fn search(
        &self,
        req: Request<pb::SearchRequest>,
    ) -> Result<Response<pb::SearchResponse>, Status> {
        let r = req.into_inner();
        let mut q: Query = r
            .query
            .ok_or_else(|| ConvertError("required field `SearchRequest.query` is missing".into()))?
            .try_into()?;
        // Only a page the default limit filled is reported, so a small answer
        // needs no paging round trips (a full one may have more behind it).
        let defaulted = q.limit.is_none();
        if defaulted {
            q.limit = Some(DEFAULT_SEARCH_LIMIT);
        }
        let hits = self.ctx.read(r.view, move |s| s.search(&q)).await?;
        let hits_len = hits.len();
        Ok(Response::new(pb::SearchResponse {
            hits: hits.into_iter().map(Into::into).collect(),
            applied_default_limit: defaulted && hits_len >= DEFAULT_SEARCH_LIMIT,
        }))
    }

    async fn open_snapshot(
        &self,
        req: Request<pb::OpenSnapshotRequest>,
    ) -> Result<Response<pb::OpenSnapshotResponse>, Status> {
        let conn = conn_id(&req);
        let slot = Arc::clone(&self.ctx.slot);
        let id = tokio::task::spawn_blocking(move || {
            slot.with_store(|s| Ok(slot.snapshots().open(conn, s)))
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
