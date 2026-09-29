//! [`RemoteStore`]: `graph_store::Store` over the connection.
use crate::conn::{
    admin_client, block_on, health_client, store_client, write_client, Conn, HelloInfo, Kind,
};
use crate::snapshot::RemoteSnapshot;
use crate::{reads, ClientConfig};
use graph_core::{Extraction, Node, NodeId, NodeKind};
use graph_proto::{pb, ConvertError};
use graph_store::{
    BatchFile, CompactStats, Hit, IndexOptions, IngestStats, Page, PreparedFile, Query, RepoInfo,
    SnapshotStats, Store, StoreError, StoreRead, SymbolHit, SymbolQuery, VacuumStats,
};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

type Result<T> = std::result::Result<T, StoreError>;

/// A request carrying `msg` with `deadline` as its `grpc-timeout` (the
/// channel enforces it, and the server stops working on it).
fn timed<T>(msg: T, deadline: Duration) -> tonic::Request<T> {
    let mut r = tonic::Request::new(msg);
    r.set_timeout(deadline);
    r
}

/// Whether a failed attempt may still have taken effect: a detail-less
/// transport loss or deadline (a typed answer, such as `NotLeader`, or
/// `UNAVAILABLE` means the request was not processed).
fn outcome_unknown(st: &tonic::Status) -> bool {
    st.details().is_empty() && graph_proto::error::is_transport_loss(st.code(), st.message())
}

/// What [`RemoteStore::admin_remove`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoveOutcome {
    /// The membership entry's log index (with `not_a_member`, the index of
    /// the membership in effect).
    pub log_index: u64,
    /// The node was not a member: nothing changed (a mistyped id, or an
    /// earlier attempt already removed it; see `retried`).
    pub not_a_member: bool,
    /// An earlier attempt of this call ended with its outcome unknown (the
    /// connection was lost or timed out after sending), so `not_a_member`
    /// may mean that attempt removed the node. A clean `NotLeader`
    /// redirect or an `UNAVAILABLE` retry does not set it.
    pub retried: bool,
}

pub struct RemoteStore {
    rt: Arc<tokio::runtime::Runtime>,
    conn: Arc<Conn>,
    /// Highest `applied_index` a `Write.Index` answered (0 before any).
    applied: Arc<AtomicU64>,
    /// Whether any `Write.Index` answer came from a node that forwarded it
    /// to the leader (`forwarded_to_leader`).
    forwarded: Arc<AtomicBool>,
}

impl RemoteStore {
    /// Build the runtime, connect to the first endpoint that answers
    /// `Hello` (refusing another protocol version with `Protocol`).
    pub fn connect(cfg: ClientConfig) -> Result<RemoteStore> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("graph-client")
            .enable_all()
            .build()
            .map_err(|e| StoreError::Storage(format!("tokio runtime: {e}")))?;
        let rt = Arc::new(rt);
        let conn = block_on(&rt, Conn::connect(cfg))?;
        Ok(RemoteStore {
            rt,
            conn: Arc::new(conn),
            applied: Arc::new(AtomicU64::new(0)),
            forwarded: Arc::new(AtomicBool::new(false)),
        })
    }

    /// What the server answered in `Hello`.
    pub fn hello(&self) -> &HelloInfo {
        self.conn.hello()
    }

    pub fn config(&self) -> &ClientConfig {
        self.conn.config()
    }

    /// The endpoint calls go to now: the one that answered the last
    /// successful call (the connection moves on only after a failure).
    pub fn endpoint(&self) -> String {
        self.conn.active_endpoint()
    }

    /// The highest Raft log index an `Index` RPC of this store reported as
    /// applied (0 before the first). Shared, so a progress display can read
    /// it while the store itself is boxed as `dyn Store`.
    pub fn applied_index(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.applied)
    }

    /// Whether an `Index` RPC of this store was answered through a node
    /// that forwarded it to the leader (the connected node is a follower).
    /// Shared like [`applied_index`](Self::applied_index).
    pub fn forwarded_to_leader(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.forwarded)
    }

    /// The [`ReadMeta`](graph_proto::ReadMeta)s of this store's reads (and
    /// its snapshots'): the last, and whether any was `stale_possible`.
    /// Shared like [`applied_index`](Self::applied_index).
    pub fn read_log(&self) -> Arc<crate::ReadLog> {
        self.conn.read_log()
    }

    fn run<F: std::future::Future>(&self, f: F) -> F::Output {
        block_on(&self.rt, f)
    }

    fn view(&self) -> graph_proto::View {
        self.conn.view()
    }

    /// `Admin.Status`.
    pub fn admin_status(&self) -> Result<pb::StatusResponse> {
        self.run(self.conn.call(Kind::Read, |ch| async move {
            admin_client(ch).status(pb::StatusRequest {}).await
        }))
        .map(|r| r.into_inner())
    }

    /// `Admin.Metrics`: the node's metrics in Prometheus text format (what
    /// `serve --metrics-listen` serves at `/metrics`).
    pub fn admin_metrics(&self) -> Result<String> {
        self.run(self.conn.call(Kind::Read, |ch| async move {
            admin_client(ch).metrics(pb::MetricsRequest {}).await
        }))
        .map(|r| r.into_inner().text)
    }

    /// `Admin.SysInfo`: the server machine's `sysinfo --json` document.
    pub fn admin_sysinfo(&self) -> Result<String> {
        self.run(self.conn.call(Kind::Read, |ch| async move {
            admin_client(ch).sys_info(pb::SysInfoRequest {}).await
        }))
        .map(|r| r.into_inner().json)
    }

    /// `Admin.Compact` (`vacuum --compact` on the server's file).
    pub fn admin_compact(&self) -> Result<CompactStats> {
        let r = self.run(self.conn.call(Kind::Read, |ch| async move {
            admin_client(ch).compact(pb::CompactRequest {}).await
        }))?;
        Ok(r.into_inner().stats.unwrap_or_default().into())
    }

    /// `Admin.Shutdown`: ask the server to stop gracefully.
    pub fn admin_shutdown(&self, grace: Duration) -> Result<()> {
        let grace_ms = grace.as_millis() as u64;
        self.run(self.conn.call(Kind::Read, |ch| async move {
            admin_client(ch)
                .shutdown(pb::ShutdownRequest { grace_ms })
                .await
        }))?;
        Ok(())
    }

    /// `Admin.Members`: every member with its role and address, and the
    /// leader this node knows.
    pub fn admin_members(&self) -> Result<pb::MembersResponse> {
        self.run(self.conn.call(Kind::Read, |ch| async move {
            admin_client(ch).members(pb::MembersRequest {}).await
        }))
        .map(|r| r.into_inner())
    }

    /// `Admin.Leader`: the leader this node knows (id and address).
    pub fn admin_leader(&self) -> Result<pb::LeaderResponse> {
        self.run(self.conn.call(Kind::Read, |ch| async move {
            admin_client(ch).leader(pb::LeaderRequest {}).await
        }))
        .map(|r| r.into_inner())
    }

    /// `Admin.AddLearner`: add `node_id` at `addr` as a learner (on the
    /// leader: a `NotLeader` answer moves the connection there, like a
    /// write); with `blocking`, return once it caught up. Returns the
    /// membership entry's log index.
    pub fn admin_add_learner(&self, node_id: u64, addr: &str, blocking: bool) -> Result<u64> {
        let addr = addr.to_string();
        let d = self.config().admin_deadline;
        self.run(self.conn.call(Kind::Write, |ch| {
            let addr = addr.clone();
            async move {
                admin_client(ch)
                    .add_learner(timed(
                        pb::AddLearnerRequest {
                            node_id,
                            addr,
                            blocking,
                        },
                        d,
                    ))
                    .await
            }
        }))
        .map(|r| r.into_inner().log_index)
    }

    /// `Admin.Promote`: make learner `node_id` a voter (on the leader).
    /// Returns the membership entry's log index.
    pub fn admin_promote(&self, node_id: u64) -> Result<u64> {
        let d = self.config().admin_deadline;
        self.run(self.conn.call(Kind::Write, |ch| async move {
            admin_client(ch)
                .promote(timed(pb::PromoteRequest { node_id }, d))
                .await
        }))
        .map(|r| r.into_inner().log_index)
    }

    /// `Admin.Remove`: remove `node_id` from the membership (any node
    /// forwards it to the leader, which enforces the guards: not the
    /// leader, not below quorum, 3 voters to 2 only with `force`). A node
    /// that is not a member is no error (a retry of a committed remove must
    /// succeed) but is reported in [`RemoveOutcome::not_a_member`].
    pub fn admin_remove(&self, node_id: u64, force: bool) -> Result<RemoveOutcome> {
        let d = self.config().admin_deadline;
        // Set when an attempt ended with its outcome unknown: a detail-less
        // transport loss or deadline after the request may have been sent.
        // A typed answer (such as a `NotLeader` redirect) or `UNAVAILABLE`
        // (never processed) leaves it clear.
        let uncertain = Arc::new(AtomicBool::new(false));
        let r = self.run(self.conn.call(Kind::Write, |ch| {
            let uncertain = Arc::clone(&uncertain);
            async move {
                let r = admin_client(ch)
                    .remove(timed(pb::RemoveRequest { node_id, force }, d))
                    .await;
                if r.as_ref().is_err_and(outcome_unknown) {
                    uncertain.store(true, Ordering::Relaxed);
                }
                r
            }
        }))?;
        let r = r.into_inner();
        Ok(RemoveOutcome {
            log_index: r.log_index,
            not_a_member: r.not_a_member,
            retried: uncertain.load(Ordering::Relaxed),
        })
    }

    /// `Admin.TransferLeader`: make voter `node_id` the leader (forwarded
    /// to the leader by any node). Returns the new leader's id.
    pub fn admin_transfer_leader(&self, node_id: u64) -> Result<u64> {
        let d = self.config().admin_deadline;
        self.run(self.conn.call(Kind::Write, |ch| async move {
            admin_client(ch)
                .transfer_leader(timed(
                    pb::TransferLeaderRequest {
                        to_node_id: node_id,
                    },
                    d,
                ))
                .await
        }))
        .map(|r| r.into_inner().leader_id)
    }

    /// `Admin.TriggerSnapshot`: build a snapshot on the connected node and,
    /// with `out`, download it there (`cluster snapshot --out`): written to
    /// `<out>.part`, checked against the size and SHA-256 the server
    /// announced, then renamed into place. Returns what the server built.
    pub fn admin_trigger_snapshot(&self, out: Option<&Path>) -> Result<pb::SnapshotInfo> {
        let out = out.map(Path::to_path_buf);
        self.run(self.conn.call(Kind::Read, |ch| {
            let out = out.clone();
            async move { download_snapshot(ch, out).await }
        }))
    }

    /// `grpc.health.v1.Health/Check` for `service` (`""` for the server,
    /// `memory-graph.ready` for "a leader is known"): `Ok(true)` when
    /// SERVING.
    pub fn health(&self, service: &str) -> Result<bool> {
        let service = service.to_string();
        let r = self.run(self.conn.call(Kind::Read, |ch| {
            let service = service.clone();
            async move {
                health_client(ch)
                    .check(tonic_health::pb::HealthCheckRequest { service })
                    .await
            }
        }))?;
        Ok(r.into_inner().status
            == tonic_health::pb::health_check_response::ServingStatus::Serving as i32)
    }

    /// The `Index` RPC over remote-prepared files (the body of
    /// `index_prepared`).
    fn index_remote(
        &self,
        org: &str,
        repo: &str,
        files: &[PreparedFile],
        opts: IndexOptions,
    ) -> Result<Vec<Result<IngestStats>>> {
        let mut msgs = Vec::with_capacity(files.len() + 1);
        msgs.push(pb::IndexRequest {
            msg: Some(pb::index_request::Msg::Header(pb::IndexHeader {
                org: org.into(),
                repo: repo.into(),
                options: Some(opts.into()),
            })),
        });
        for f in files {
            let parts = f.remote_parts().ok_or_else(|| {
                StoreError::Rejected(format!(
                    "`{}` was prepared by an embedded store, not by this remote store",
                    f.path()
                ))
            })?;
            if parts.org != org || parts.repo != repo {
                return Err(StoreError::Rejected(format!(
                    "`{}` was prepared for {}/{}, not {org}/{repo}",
                    parts.path, parts.org, parts.repo
                )));
            }
            msgs.push(pb::IndexRequest {
                msg: Some(pb::index_request::Msg::File(pb::FileBytes {
                    path: parts.path.to_string(),
                    bytes: parts.bytes.to_vec(),
                    language: parts.language.map(str::to_string),
                    origin: parts.origin.map(str::to_string),
                })),
            });
        }
        let msgs = Arc::new(msgs);
        let resp = self.run(self.conn.call(Kind::Write, |ch| {
            let msgs = Arc::clone(&msgs);
            async move {
                let stream = tokio_stream::iter((*msgs).clone());
                write_client(ch).index(stream).await
            }
        }))?;
        let resp = resp.into_inner();
        self.applied
            .fetch_max(resp.applied_index, Ordering::Relaxed);
        if resp.forwarded_to_leader {
            self.forwarded.store(true, Ordering::Relaxed);
        }
        if resp.results.len() != files.len() {
            return Err(StoreError::Protocol(format!(
                "Index answered {} results for {} files",
                resp.results.len(),
                files.len()
            )));
        }
        resp.results
            .into_iter()
            .map(|r| {
                let r: std::result::Result<Result<IngestStats>, ConvertError> = r.try_into();
                r.map_err(StoreError::from)
            })
            .collect()
    }
}

/// One `TriggerSnapshot` call: the info message, then (with `out`) the
/// chunks into `<out>.part`, verified and renamed to `out`.
async fn download_snapshot(
    ch: tonic::transport::Channel,
    out: Option<std::path::PathBuf>,
) -> std::result::Result<pb::SnapshotInfo, tonic::Status> {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    use tokio_stream::StreamExt;
    let local = |what: &str, e: std::io::Error| tonic::Status::internal(format!("{what}: {e}"));
    let mut stream = admin_client(ch)
        .trigger_snapshot(pb::TriggerSnapshotRequest {
            download: out.is_some(),
        })
        .await?
        .into_inner();
    let info = match stream.next().await {
        Some(Ok(pb::TriggerSnapshotResponse {
            msg: Some(pb::trigger_snapshot_response::Msg::Info(i)),
        })) => i,
        Some(Ok(_)) => {
            return Err(tonic::Status::internal(
                "TriggerSnapshot did not start with a SnapshotInfo",
            ))
        }
        Some(Err(e)) => return Err(e),
        None => return Err(tonic::Status::internal("empty TriggerSnapshot stream")),
    };
    let Some(out) = out else {
        return Ok(info);
    };
    let mut part = out.as_os_str().to_owned();
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    let mut file = std::fs::File::create(&part).map_err(|e| local("creating the download", e))?;
    let mut h = Sha256::new();
    let mut n = 0u64;
    let r = async {
        while let Some(msg) = stream.next().await {
            match msg?.msg {
                Some(pb::trigger_snapshot_response::Msg::Chunk(c)) => {
                    n += c.len() as u64;
                    h.update(&c);
                    file.write_all(&c)
                        .map_err(|e| local("writing the download", e))?;
                }
                _ => {
                    return Err(tonic::Status::internal(
                        "a second SnapshotInfo in the TriggerSnapshot stream",
                    ))
                }
            }
        }
        file.sync_all()
            .map_err(|e| local("syncing the download", e))?;
        let sha: String = h
            .finalize_reset()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if n != info.size || sha != info.sha256 {
            return Err(tonic::Status::data_loss(format!(
                "snapshot download is {n} bytes with sha256 {sha}, the server announced {} \
                 bytes with {}",
                info.size, info.sha256
            )));
        }
        Ok(())
    }
    .await;
    drop(file);
    if let Err(e) = r {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, &out).map_err(|e| local("placing the download", e))?;
    Ok(info)
}

impl StoreRead for RemoteStore {
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

impl Store for RemoteStore {
    fn snapshot(&self) -> Result<Box<dyn StoreRead + Send + '_>> {
        let id = self.run(reads::open_snapshot(
            &self.conn,
            self.view() == graph_proto::View::Linearizable,
        ))?;
        Ok(Box::new(RemoteSnapshot::new(
            Arc::clone(&self.conn),
            Arc::clone(&self.rt),
            id,
        )))
    }

    fn snapshot_stats(&self) -> SnapshotStats {
        let r = self.run(self.conn.call(Kind::Read, |ch| async move {
            store_client(ch)
                .snapshot_stats(pb::SnapshotStatsRequest {})
                .await
        }));
        match r {
            Ok(r) => r
                .into_inner()
                .stats
                .and_then(|s| SnapshotStats::try_from(s).ok())
                .unwrap_or_default(),
            Err(e) => {
                tracing::debug!(error = %e, "snapshot_stats");
                SnapshotStats::default()
            }
        }
    }

    fn index_bytes_opts(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        bytes: &[u8],
        language: Option<&str>,
        origin: Option<&str>,
        opts: IndexOptions,
    ) -> Result<IngestStats> {
        let req = pb::IndexFileRequest {
            org: org.into(),
            repo: repo.into(),
            file: Some(pb::FileBytes {
                // `/`-separated on the wire whatever the client OS (#120); the
                // server's store normalizes again on apply.
                path: graph_core::normalize_path(path),
                bytes: bytes.to_vec(),
                language: language.map(str::to_string),
                origin: origin.map(str::to_string),
            }),
            options: Some(opts.into()),
        };
        let r = self.run(self.conn.call(Kind::Write, |ch| {
            let req = req.clone();
            async move { write_client(ch).index_file(req).await }
        }))?;
        let stats = r
            .into_inner()
            .stats
            .ok_or_else(|| StoreError::Protocol("IndexFileResponse.stats is missing".into()))?;
        IngestStats::try_from(stats).map_err(StoreError::from)
    }

    fn ingest_file_with_origin(
        &self,
        org: &str,
        repo: &str,
        path: &str,
        language: &str,
        ex: &Extraction,
        origin: Option<&str>,
    ) -> Result<IngestStats> {
        let req = pb::IngestExtractionRequest {
            org: org.into(),
            repo: repo.into(),
            path: graph_core::normalize_path(path),
            language: language.into(),
            extraction: Some(ex.clone().into()),
            origin: origin.map(str::to_string),
        };
        let req = Arc::new(req);
        let r = self.run(self.conn.call(Kind::Write, |ch| {
            let req = Arc::clone(&req);
            async move { write_client(ch).ingest_extraction((*req).clone()).await }
        }))?;
        let stats = r.into_inner().stats.ok_or_else(|| {
            StoreError::Protocol("IngestExtractionResponse.stats is missing".into())
        })?;
        IngestStats::try_from(stats).map_err(StoreError::from)
    }

    fn index_batch(
        &self,
        org: &str,
        repo: &str,
        files: &[BatchFile<'_>],
        opts: IndexOptions,
    ) -> Result<Vec<Result<IngestStats>>> {
        let prepared = files
            .iter()
            .map(|f| self.prepare(org, repo, f, opts))
            .collect::<Result<Vec<_>>>()?;
        self.index_prepared(org, repo, prepared, opts)
    }

    fn prepare(
        &self,
        org: &str,
        repo: &str,
        file: &BatchFile<'_>,
        _opts: IndexOptions,
    ) -> Result<PreparedFile> {
        Ok(PreparedFile::remote(
            org,
            repo,
            file.path,
            file.bytes.to_vec(),
            file.language.map(str::to_string),
            file.origin.map(str::to_string),
        ))
    }

    fn index_prepared(
        &self,
        org: &str,
        repo: &str,
        files: Vec<PreparedFile>,
        opts: IndexOptions,
    ) -> Result<Vec<Result<IngestStats>>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        self.index_remote(org, repo, &files, opts)
    }

    fn prune_files(
        &self,
        org: &str,
        repo: &str,
        keep: &HashSet<String>,
        dry_run: bool,
    ) -> Result<Vec<String>> {
        let req = pb::PruneRequest {
            org: org.into(),
            repo: repo.into(),
            keep: keep.iter().cloned().collect(),
            dry_run,
        };
        let kind = if dry_run { Kind::Read } else { Kind::Write };
        let r = self.run(self.conn.call(kind, |ch| {
            let req = req.clone();
            async move { write_client(ch).prune(req).await }
        }))?;
        Ok(r.into_inner().removed)
    }

    fn vacuum(&self) -> Result<VacuumStats> {
        let r = self.run(self.conn.call(Kind::Write, |ch| async move {
            write_client(ch).vacuum(pb::VacuumRequest {}).await
        }))?;
        let stats = r
            .into_inner()
            .stats
            .ok_or_else(|| StoreError::Protocol("VacuumResponse.stats is missing".into()))?;
        VacuumStats::try_from(stats).map_err(StoreError::from)
    }
}

#[cfg(test)]
mod remove_outcome_tests {
    use super::outcome_unknown;
    use graph_store::StoreError;
    use tonic::Status;

    #[test]
    fn only_a_lost_answer_makes_a_retried_remove_uncertain() {
        assert!(outcome_unknown(&Status::unknown("transport error")));
        assert!(outcome_unknown(&Status::deadline_exceeded("timeout")));
        assert!(!outcome_unknown(&Status::unavailable("tcp connect error")));
        let redirect: Status = graph_proto::WireError::Store(StoreError::NotLeader {
            leader_id: Some(1),
            leader_addr: Some("h:1".into()),
        })
        .into();
        assert!(!outcome_unknown(&redirect));
    }
}
