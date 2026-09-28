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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

type Result<T> = std::result::Result<T, StoreError>;

pub struct RemoteStore {
    rt: Arc<tokio::runtime::Runtime>,
    conn: Arc<Conn>,
    /// Highest `applied_index` a `Write.Index` answered (0 before any).
    applied: Arc<AtomicU64>,
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
        })
    }

    /// What the server answered in `Hello`.
    pub fn hello(&self) -> &HelloInfo {
        self.conn.hello()
    }

    pub fn config(&self) -> &ClientConfig {
        self.conn.config()
    }

    /// The highest Raft log index an `Index` RPC of this store reported as
    /// applied (0 before the first). Shared, so a progress display can read
    /// it while the store itself is boxed as `dyn Store`.
    pub fn applied_index(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.applied)
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
        let id = self.run(reads::open_snapshot(&self.conn))?;
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
                path: path.into(),
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
            path: path.into(),
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
