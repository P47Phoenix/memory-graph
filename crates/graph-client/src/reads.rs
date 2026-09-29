//! The `StoreRead` RPCs, shared by [`RemoteStore`](crate::RemoteStore)
//! (view = the configured read mode) and [`RemoteSnapshot`](crate::RemoteSnapshot)
//! (view = its handle). Every function is one RPC, except the unbounded
//! searches, which page to completion under a snapshot handle when the
//! server applied its default limit.
use crate::conn::{store_client, Conn, Kind};
use graph_core::{Node, NodeId, NodeKind};
use graph_proto::convert::enum_i32;
use graph_proto::{pb, ConvertError, ReadMeta, View};
use graph_store::{Hit, Page, Query, RepoInfo, StoreError, SymbolHit, SymbolQuery};
use tokio_stream::StreamExt;

type Result<T> = std::result::Result<T, StoreError>;

fn view_of(v: View) -> Option<pb::View> {
    Some(v.into())
}

fn conv<T, M: TryInto<T, Error = ConvertError>>(m: M) -> Result<T> {
    m.try_into().map_err(StoreError::from)
}

fn nodes(v: Vec<pb::Node>) -> Result<Vec<Node>> {
    v.into_iter().map(conv).collect()
}

pub async fn get(c: &Conn, view: View, id: NodeId) -> Result<Option<Node>> {
    let r = c
        .call(Kind::Read, |ch| async move {
            store_client(ch)
                .get(pb::GetRequest {
                    view: view_of(view),
                    id,
                })
                .await
        })
        .await?;
    c.answer(r).node.map(conv).transpose()
}

pub async fn parent(c: &Conn, view: View, id: NodeId) -> Result<Option<Node>> {
    let r = c
        .call(Kind::Read, |ch| async move {
            store_client(ch)
                .parent(pb::ParentRequest {
                    view: view_of(view),
                    id,
                })
                .await
        })
        .await?;
    c.answer(r).node.map(conv).transpose()
}

pub async fn count_nodes(c: &Conn, view: View, kind: NodeKind) -> Result<usize> {
    let r = c
        .call(Kind::Read, |ch| async move {
            store_client(ch)
                .count_nodes(pb::CountNodesRequest {
                    view: view_of(view),
                    kind: enum_i32::<_, pb::NodeKind>(kind),
                })
                .await
        })
        .await?;
    usize::try_from(c.answer(r).count)
        .map_err(|_| StoreError::Protocol("count does not fit usize".into()))
}

pub async fn roots(c: &Conn, view: View) -> Result<Vec<Node>> {
    let r = c
        .call(Kind::Read, |ch| async move {
            store_client(ch)
                .roots(pb::RootsRequest {
                    view: view_of(view),
                })
                .await
        })
        .await?;
    nodes(c.answer(r).nodes)
}

pub async fn children(c: &Conn, view: View, id: NodeId) -> Result<Vec<Node>> {
    let r = c
        .call(Kind::Read, |ch| async move {
            store_client(ch)
                .children(pb::ChildrenRequest {
                    view: view_of(view),
                    id,
                })
                .await
        })
        .await?;
    nodes(c.answer(r).nodes)
}

pub async fn children_page(
    c: &Conn,
    view: View,
    id: NodeId,
    offset: usize,
    limit: usize,
) -> Result<Page<Node>> {
    let r = c
        .call(Kind::Read, |ch| async move {
            store_client(ch)
                .children_page(pb::ChildrenPageRequest {
                    view: view_of(view),
                    id,
                    offset: offset as u64,
                    limit: limit as u64,
                })
                .await
        })
        .await?;
    conv(c.answer(r))
}

pub async fn descendants_page(
    c: &Conn,
    view: View,
    id: NodeId,
    offset: usize,
    limit: usize,
) -> Result<Page<Node>> {
    let r = c
        .call(Kind::Read, |ch| async move {
            store_client(ch)
                .descendants_page(pb::DescendantsPageRequest {
                    view: view_of(view),
                    id,
                    offset: offset as u64,
                    limit: limit as u64,
                })
                .await
        })
        .await?;
    conv(c.answer(r))
}

pub async fn ancestors(c: &Conn, view: View, id: NodeId) -> Result<Vec<Node>> {
    let r = c
        .call(Kind::Read, |ch| async move {
            store_client(ch)
                .ancestors(pb::AncestorsRequest {
                    view: view_of(view),
                    id,
                })
                .await
        })
        .await?;
    nodes(c.answer(r).nodes)
}

/// Consume a `NodeBatch` stream into one list; `Ok(None)` when the first
/// batch says `not_found`.
async fn drain(
    stream: tonic::Streaming<pb::NodeBatch>,
) -> std::result::Result<Option<Vec<Node>>, tonic::Status> {
    let mut stream = stream;
    let mut out = Vec::new();
    let mut first = true;
    while let Some(b) = stream.next().await {
        let b = b?;
        if first && b.not_found {
            return Ok(None);
        }
        first = false;
        for n in b.nodes {
            out.push(Node::try_from(n)?);
        }
    }
    Ok(Some(out))
}

pub async fn descendants(c: &Conn, view: View, id: NodeId) -> Result<Vec<Node>> {
    let r = c
        .call(Kind::Read, |ch| async move {
            let r = store_client(ch)
                .descendants(pb::DescendantsRequest {
                    view: view_of(view),
                    id,
                })
                .await?;
            let meta = ReadMeta::from_metadata(r.metadata());
            drain(r.into_inner()).await.map(|v| (v, meta))
        })
        .await?;
    c.note(r.1);
    Ok(r.0.unwrap_or_default())
}

pub async fn file_tokens(
    c: &Conn,
    view: View,
    org: &str,
    repo: &str,
    path: &str,
) -> Result<Option<Vec<Node>>> {
    let req = pb::FileTokensRequest {
        view: view_of(view),
        org: org.into(),
        repo: repo.into(),
        path: path.into(),
    };
    let (toks, meta) = c
        .call(Kind::Read, |ch| {
            let req = req.clone();
            async move {
                let r = store_client(ch).file_tokens(req).await?;
                let meta = ReadMeta::from_metadata(r.metadata());
                drain(r.into_inner()).await.map(|v| (v, meta))
            }
        })
        .await?;
    c.note(meta);
    Ok(toks)
}

pub async fn describe(
    c: &Conn,
    view: View,
    org: Option<&str>,
    repo: Option<&str>,
    by_scan: bool,
) -> Result<Vec<RepoInfo>> {
    let req = pb::DescribeRequest {
        view: view_of(view),
        org: org.map(str::to_string),
        repo: repo.map(str::to_string),
    };
    let r = c
        .call(Kind::Read, |ch| {
            let req = req.clone();
            async move {
                let mut cl = store_client(ch);
                if by_scan {
                    cl.describe_by_scan(req).await
                } else {
                    cl.describe(req).await
                }
            }
        })
        .await?;
    c.answer(r).repos.into_iter().map(conv).collect()
}

/// One `Search` page: hits and whether the server applied its default.
async fn search_page(c: &Conn, view: View, q: &Query) -> Result<(Vec<Hit>, bool)> {
    let req = pb::SearchRequest {
        view: view_of(view),
        query: Some(q.clone().into()),
    };
    let r = c
        .call(Kind::Read, |ch| {
            let req = req.clone();
            async move { store_client(ch).search(req).await }
        })
        .await
        .map(|r| c.answer(r))?;
    let hits = r.hits.into_iter().map(conv).collect::<Result<Vec<_>>>()?;
    Ok((hits, r.applied_default_limit))
}

async fn search_symbols_page(
    c: &Conn,
    view: View,
    q: &SymbolQuery,
) -> Result<(Vec<SymbolHit>, bool)> {
    let req = pb::SearchSymbolsRequest {
        view: view_of(view),
        query: Some(q.clone().into()),
    };
    let r = c
        .call(Kind::Read, |ch| {
            let req = req.clone();
            async move { store_client(ch).search_symbols(req).await }
        })
        .await
        .map(|r| c.answer(r))?;
    let hits = r.hits.into_iter().map(conv).collect::<Result<Vec<_>>>()?;
    Ok((hits, r.applied_default_limit))
}

/// `OpenSnapshot`; `linearizable`: the server runs the read barrier first.
pub async fn open_snapshot(c: &Conn, linearizable: bool) -> Result<u64> {
    let r = c
        .call(Kind::Read, |ch| async move {
            let mut req = tonic::Request::new(pb::OpenSnapshotRequest {});
            if linearizable {
                req.metadata_mut().insert(
                    graph_proto::READ_MODE_HEADER,
                    tonic::metadata::MetadataValue::from_static("linearizable"),
                );
            }
            store_client(ch).open_snapshot(req).await
        })
        .await?;
    Ok(r.into_inner().snapshot_id)
}

pub async fn close_snapshot(c: &Conn, id: u64) -> Result<()> {
    c.call(Kind::Read, |ch| async move {
        store_client(ch)
            .close_snapshot(pb::CloseSnapshotRequest { snapshot_id: id })
            .await
    })
    .await?;
    Ok(())
}

/// A frozen view to page under: the caller's own snapshot handle, or a
/// fresh one (closed afterwards) when the view is live.
async fn paging_view(c: &Conn, view: View) -> Result<(View, Option<u64>)> {
    match view {
        View::Snapshot(_) => Ok((view, None)),
        View::Local | View::Linearizable => {
            let id = open_snapshot(c, view == View::Linearizable).await?;
            Ok((View::Snapshot(id), Some(id)))
        }
    }
}

/// `search` with the semantics of the embedded store: a query without a
/// limit returns every row. When the server applied its default limit, the
/// first answer is discarded and the query re-run page by page (page size =
/// the server's default) under one snapshot handle, so no page straddles a
/// write.
pub async fn search(c: &Conn, view: View, q: &Query) -> Result<Vec<Hit>> {
    let (hits, applied) = search_page(c, view, q).await?;
    if !applied {
        return Ok(hits);
    }
    let page_size = hits.len().max(1);
    let (pview, opened) = paging_view(c, view).await?;
    let result = page_all(page_size, q.offset.unwrap_or(0), |offset| {
        let mut pq = q.clone();
        pq.limit = Some(page_size);
        pq.offset = Some(offset);
        async move { search_page(c, pview, &pq).await.map(|(h, _)| h) }
    })
    .await;
    if let Some(id) = opened {
        let _ = close_snapshot(c, id).await;
    }
    result
}

pub async fn search_symbols(c: &Conn, view: View, q: &SymbolQuery) -> Result<Vec<SymbolHit>> {
    let (hits, applied) = search_symbols_page(c, view, q).await?;
    if !applied {
        return Ok(hits);
    }
    let page_size = hits.len().max(1);
    let (pview, opened) = paging_view(c, view).await?;
    let result = page_all(page_size, q.offset.unwrap_or(0), |offset| {
        let mut pq = q.clone();
        pq.limit = Some(page_size);
        pq.offset = Some(offset);
        async move { search_symbols_page(c, pview, &pq).await.map(|(h, _)| h) }
    })
    .await;
    if let Some(id) = opened {
        let _ = close_snapshot(c, id).await;
    }
    result
}

/// Fetch pages of `page_size` from `start` until a short page.
async fn page_all<T, F, Fut>(page_size: usize, start: usize, fetch: F) -> Result<Vec<T>>
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<T>>>,
{
    let mut out = Vec::new();
    let mut offset = start;
    loop {
        let page = fetch(offset).await?;
        let n = page.len();
        out.extend(page);
        if n < page_size {
            return Ok(out);
        }
        offset += n;
    }
}
