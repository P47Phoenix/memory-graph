//! `memory_graph.v1.Write`: every write becomes one or more `LogCommand`s
//! proposed through Raft (ADR 0004 D5). `Index` cuts its files into
//! `IndexChunk` entries at `RAFT_ENTRY_MAX_BYTES` (a single larger file is
//! an entry of its own); a dry-run `Prune` is node-local and never logged.
//!
//! On a node that is not the leader every write is forwarded to the leader
//! (ADR 0004 D8, [`crate::forward`]): the answer is the leader's, with
//! `forwarded_to_leader` set. `Index` is forwarded as a stream through a
//! channel of [`FORWARD_BUFFER`] messages, so the follower holds at most a
//! few files at a time; a client stream that fails mid-way cancels the
//! forwarded call (the leader sees an error, never a clean end, so it does
//! not commit a truncated batch as if it were whole).
use super::{status, Ctx};
use crate::forward::{forward_error, Forwarder, Route};
use crate::raft::{LogRequest, LogResponse};
use graph_proto::pb::log_command::Cmd;
use graph_proto::{pb, ConvertError, RAFT_ENTRY_MAX_BYTES};
use graph_store::{IngestStats, StoreError};
use std::collections::HashSet;
use std::sync::Arc;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status, Streaming};

/// Messages of a forwarded `Index` stream buffered between the client and
/// the leader.
pub const FORWARD_BUFFER: usize = 2;

pub struct WriteService {
    pub ctx: Arc<Ctx>,
}

/// Forward one unary write to the leader at `addr` and mark the answer.
macro_rules! forward_unary {
    ($self:ident, $addr:expr, $method:ident, $req:expr) => {{
        let req = $req;
        let deadline = $self
            .ctx
            .fwd
            .deadline(req.metadata(), crate::forward::FORWARD_UNARY_TIMEOUT);
        let mut client = $self.ctx.fwd.write_client(&$addr)?;
        let resp = crate::forward::within(
            deadline,
            client.$method(Forwarder::request(req.into_inner(), deadline)),
        )
        .await
        .map_err(forward_error)?;
        $self.ctx.fwd.count();
        let mut resp = resp.into_inner();
        resp.forwarded_to_leader = true;
        return Ok(Response::new(resp));
    }};
}

impl WriteService {
    /// Forward a whole `Index` stream to the leader at `addr`, message by
    /// message (bounded memory).
    async fn forward_index(
        &self,
        addr: &str,
        req: Request<Streaming<pb::IndexRequest>>,
    ) -> Result<Response<pb::IndexResponse>, Status> {
        let deadline = self
            .ctx
            .fwd
            .deadline(req.metadata(), crate::forward::FORWARD_INDEX_TIMEOUT);
        let mut incoming = req.into_inner();
        let mut client = self.ctx.fwd.write_client(addr)?;
        let (tx, rx) = tokio::sync::mpsc::channel::<pb::IndexRequest>(FORWARD_BUFFER);
        let call = crate::forward::within(
            deadline,
            client.index(Forwarder::request(
                tokio_stream::wrappers::ReceiverStream::new(rx),
                deadline,
            )),
        );
        tokio::pin!(call);
        // The pump owns the sender: when the client's stream ends the
        // sender is dropped and the forwarded stream ends too.
        let pump = async move {
            while let Some(msg) = incoming.next().await {
                if tx.send(msg?).await.is_err() {
                    // The leader answered already (an early error); the
                    // call below carries it.
                    break;
                }
            }
            Ok::<(), Status>(())
        };
        tokio::pin!(pump);
        let mut pumped = false;
        let resp = loop {
            tokio::select! {
                r = &mut call => break r.map_err(forward_error)?,
                p = &mut pump, if !pumped => match p {
                    Ok(()) => pumped = true,
                    // Returning drops the call: the forwarded stream is
                    // cancelled, not ended cleanly.
                    Err(e) => return Err(e),
                },
            }
        };
        self.ctx.fwd.count();
        let mut resp = resp.into_inner();
        resp.forwarded_to_leader = true;
        Ok(Response::new(resp))
    }

    async fn propose(&self, cmd: Cmd) -> Result<(LogResponse, u64), Status> {
        if let Some(after) = self.ctx.stall_writes_after {
            let seen = self
                .ctx
                .writes_proposed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if seen >= after {
                if seen == after {
                    use std::io::Write;
                    println!("memory-graph serve: testing: writes stalled after {after}");
                    let _ = std::io::stdout().flush();
                }
                std::future::pending::<()>().await;
            }
        }
        let req = LogRequest::new(&pb::LogCommand { cmd: Some(cmd) });
        let (resp, index) = self.ctx.raft.propose(req).await.map_err(status)?;
        match resp {
            LogResponse::Failed(d) => Err(status(d.into_error())),
            LogResponse::Skipped => Err(Status::internal(
                "the entry was skipped as already applied; this cannot happen for a fresh proposal",
            )),
            r => Ok((r, index)),
        }
    }

    async fn index_chunk(
        &self,
        org: &str,
        repo: &str,
        reindex: bool,
        files: Vec<pb::FileBytes>,
    ) -> Result<(Vec<pb::FileResult>, u64), Status> {
        let n = files.len();
        let (resp, index) = self
            .propose(Cmd::IndexChunk(pb::log_command::IndexChunk {
                org: org.into(),
                repo: repo.into(),
                reindex,
                files,
            }))
            .await?;
        match resp {
            LogResponse::Index(results) if results.len() == n => Ok((
                results
                    .into_iter()
                    .map(|r| {
                        let r: Result<IngestStats, StoreError> = r.map_err(|d| d.into_error());
                        pb::FileResult::from(r)
                    })
                    .collect(),
                index,
            )),
            other => Err(Status::internal(format!("IndexChunk applied as {other:?}"))),
        }
    }
}

/// Cut `files` into chunks of at most `RAFT_ENTRY_MAX_BYTES` of source
/// bytes (a larger single file alone): the same soft cap as `--chunk-bytes`.
pub fn cut_chunks(files: Vec<pb::FileBytes>) -> Vec<Vec<pb::FileBytes>> {
    let mut out: Vec<Vec<pb::FileBytes>> = Vec::new();
    let mut cur: Vec<pb::FileBytes> = Vec::new();
    let mut cur_bytes = 0usize;
    for f in files {
        if !cur.is_empty() && cur_bytes + f.bytes.len() > RAFT_ENTRY_MAX_BYTES {
            out.push(std::mem::take(&mut cur));
            cur_bytes = 0;
        }
        cur_bytes += f.bytes.len();
        cur.push(f);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[tonic::async_trait]
impl pb::write_server::Write for WriteService {
    async fn index(
        &self,
        req: Request<Streaming<pb::IndexRequest>>,
    ) -> Result<Response<pb::IndexResponse>, Status> {
        if let Route::Leader { addr, .. } = self.ctx.fwd.route(&self.ctx.raft, &req)? {
            return self.forward_index(&addr, req).await;
        }
        let mut stream = req.into_inner();
        let header = match stream.next().await {
            Some(Ok(pb::IndexRequest {
                msg: Some(pb::index_request::Msg::Header(h)),
            })) => h,
            Some(Ok(_)) => {
                return Err(
                    ConvertError("Index stream must start with an IndexHeader".into()).into(),
                )
            }
            Some(Err(e)) => return Err(e),
            None => return Err(ConvertError("empty Index stream".into()).into()),
        };
        let reindex = header.options.map(|o| o.reindex).unwrap_or(false);
        let mut results = Vec::new();
        let mut applied_index = 0;
        let mut pending: Vec<pb::FileBytes> = Vec::new();
        let mut pending_bytes = 0usize;
        while let Some(msg) = stream.next().await {
            let file = match msg?.msg {
                Some(pb::index_request::Msg::File(f)) => f,
                Some(pb::index_request::Msg::Header(_)) => {
                    return Err(ConvertError("a second IndexHeader in the stream".into()).into())
                }
                None => return Err(ConvertError("empty IndexRequest".into()).into()),
            };
            if !pending.is_empty() && pending_bytes + file.bytes.len() > RAFT_ENTRY_MAX_BYTES {
                let (r, idx) = self
                    .index_chunk(
                        &header.org,
                        &header.repo,
                        reindex,
                        std::mem::take(&mut pending),
                    )
                    .await?;
                results.extend(r);
                applied_index = idx;
                pending_bytes = 0;
            }
            pending_bytes += file.bytes.len();
            pending.push(file);
        }
        if !pending.is_empty() {
            let (r, idx) = self
                .index_chunk(&header.org, &header.repo, reindex, pending)
                .await?;
            results.extend(r);
            applied_index = idx;
        }
        Ok(Response::new(pb::IndexResponse {
            results,
            forwarded_to_leader: false,
            applied_index,
        }))
    }

    async fn index_file(
        &self,
        req: Request<pb::IndexFileRequest>,
    ) -> Result<Response<pb::IndexFileResponse>, Status> {
        if let Route::Leader { addr, .. } = self.ctx.fwd.route(&self.ctx.raft, &req)? {
            forward_unary!(self, addr, index_file, req);
        }
        let r = req.into_inner();
        let file = r.file.ok_or_else(|| {
            ConvertError("required field `IndexFileRequest.file` is missing".into())
        })?;
        let reindex = r.options.map(|o| o.reindex).unwrap_or(false);
        let (mut results, applied_index) = self
            .index_chunk(&r.org, &r.repo, reindex, vec![file])
            .await?;
        let outcome: Result<IngestStats, StoreError> = results.remove(0).try_into()?;
        let stats = outcome.map_err(status)?;
        Ok(Response::new(pb::IndexFileResponse {
            stats: Some(stats.into()),
            forwarded_to_leader: false,
            applied_index,
        }))
    }

    async fn ingest_extraction(
        &self,
        req: Request<pb::IngestExtractionRequest>,
    ) -> Result<Response<pb::IngestExtractionResponse>, Status> {
        if let Route::Leader { addr, .. } = self.ctx.fwd.route(&self.ctx.raft, &req)? {
            forward_unary!(self, addr, ingest_extraction, req);
        }
        let r = req.into_inner();
        let (resp, applied_index) = self
            .propose(Cmd::IngestExtraction(pb::log_command::IngestExtraction {
                org: r.org,
                repo: r.repo,
                path: r.path,
                language: r.language,
                extraction: r.extraction,
                origin: r.origin,
            }))
            .await?;
        match resp {
            LogResponse::Ingest(stats) => Ok(Response::new(pb::IngestExtractionResponse {
                stats: Some(stats.into()),
                forwarded_to_leader: false,
                applied_index,
            })),
            other => Err(Status::internal(format!(
                "IngestExtraction applied as {other:?}"
            ))),
        }
    }

    async fn prune(
        &self,
        req: Request<pb::PruneRequest>,
    ) -> Result<Response<pb::PruneResponse>, Status> {
        if !req.get_ref().dry_run {
            if let Route::Leader { addr, .. } = self.ctx.fwd.route(&self.ctx.raft, &req)? {
                forward_unary!(self, addr, prune, req);
            }
        }
        let r = req.into_inner();
        if r.dry_run {
            let keep: HashSet<String> = r.keep.into_iter().collect();
            let removed = self
                .ctx
                .blocking(move |slot| {
                    slot.with_store(|s| {
                        graph_store::Store::prune_files(s, &r.org, &r.repo, &keep, true)
                    })
                })
                .await?;
            return Ok(Response::new(pb::PruneResponse {
                removed,
                forwarded_to_leader: false,
                applied_index: 0,
            }));
        }
        let (resp, applied_index) = self
            .propose(Cmd::Prune(pb::log_command::Prune {
                org: r.org,
                repo: r.repo,
                keep: r.keep,
            }))
            .await?;
        match resp {
            LogResponse::Prune(removed) => Ok(Response::new(pb::PruneResponse {
                removed,
                forwarded_to_leader: false,
                applied_index,
            })),
            other => Err(Status::internal(format!("Prune applied as {other:?}"))),
        }
    }

    async fn vacuum(
        &self,
        req: Request<pb::VacuumRequest>,
    ) -> Result<Response<pb::VacuumResponse>, Status> {
        if let Route::Leader { addr, .. } = self.ctx.fwd.route(&self.ctx.raft, &req)? {
            forward_unary!(self, addr, vacuum, req);
        }
        let (resp, applied_index) = self
            .propose(Cmd::Vacuum(pb::log_command::Vacuum {}))
            .await?;
        match resp {
            LogResponse::Vacuum(v) => Ok(Response::new(pb::VacuumResponse {
                stats: Some(graph_store::VacuumStats::from(v).into()),
                forwarded_to_leader: false,
                applied_index,
            })),
            other => Err(Status::internal(format!("Vacuum applied as {other:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(n: usize) -> pb::FileBytes {
        pb::FileBytes {
            path: format!("{n}"),
            bytes: vec![0; n],
            language: None,
            origin: None,
        }
    }

    #[test]
    fn chunks_are_cut_at_the_entry_cap_and_a_big_file_stands_alone() {
        let m = RAFT_ENTRY_MAX_BYTES;
        let chunks = cut_chunks(vec![f(m / 2), f(m / 2), f(1), f(m + 1), f(1), f(1)]);
        let sizes: Vec<Vec<usize>> = chunks
            .iter()
            .map(|c| c.iter().map(|x| x.bytes.len()).collect())
            .collect();
        assert_eq!(
            sizes,
            vec![vec![m / 2, m / 2], vec![1], vec![m + 1], vec![1, 1]]
        );
        assert!(cut_chunks(vec![]).is_empty());
        assert_eq!(
            cut_chunks(vec![f(0), f(0)]).len(),
            1,
            "empty files share a chunk"
        );
    }
}
