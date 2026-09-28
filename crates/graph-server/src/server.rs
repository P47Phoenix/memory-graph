//! Start-up and shutdown (ADR 0004 D4/D10): open the store, bind, write the
//! LOCK sidecar, start the Raft node, serve until told to stop. Shutdown
//! order: health `""` and `memory-graph.ready` go `NOT_SERVING` at once; stop
//! accepting and finish in-flight requests (bounded by the grace period);
//! shut the Raft node down (bounded by the grace period too, logged on
//! expiry); close the store; remove the sidecar last.
use crate::conn::ConnIo;
use crate::extractors::{extractors_hash, share};
use crate::lock::LockFile;
use crate::raft::RaftNode;
use crate::services::admin::AdminService;
use crate::services::store::StoreService;
use crate::services::write::WriteService;
use crate::services::{Ctx, ServerInfo};
use crate::slot::StoreSlot;
use crate::snapshots::SnapshotTable;
use crate::READY_SERVICE;
use graph_core::Extractor;
use graph_proto::pb::admin_server::AdminServer;
use graph_proto::pb::store_server::StoreServer;
use graph_proto::pb::write_server::WriteServer;
use graph_store::StoreError;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::StreamExt;
use tonic_health::ServingStatus;

/// Produces the `sysinfo --json` document for `Admin.SysInfo`, given the
/// store path. The CLI passes its own report; without one the server
/// answers a minimal document.
pub type SysInfoFn = Arc<dyn Fn(&Path) -> serde_json::Value + Send + Sync>;

/// How a server is configured (the `serve` flags).
#[derive(Clone)]
pub struct ServeConfig {
    /// The store file; `<db>.raft.redb`, `<db>.LOCK` and the snapshot files
    /// go next to it.
    pub db: PathBuf,
    /// Where to listen; port 0 picks a free one (see [`Running::addr`]).
    pub listen: SocketAddr,
    /// This node's Raft id (default 1).
    pub node_id: u64,
    /// redb cache size; `None` keeps redb's default.
    pub cache_bytes: Option<usize>,
    /// Snapshot handle (and store snapshot) max age; default 15 minutes.
    pub snapshot_max_age: Duration,
    /// How long in-flight requests get to finish on shutdown; default 30 s.
    pub shutdown_grace: Duration,
    /// The `Admin.SysInfo` provider.
    pub sysinfo: Option<SysInfoFn>,
}

impl ServeConfig {
    pub fn new(db: impl Into<PathBuf>, listen: SocketAddr) -> Self {
        Self {
            db: db.into(),
            listen,
            node_id: 1,
            cache_bytes: None,
            snapshot_max_age: Duration::from_secs(15 * 60),
            shutdown_grace: Duration::from_secs(30),
            sysinfo: None,
        }
    }
}

impl std::fmt::Debug for ServeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeConfig")
            .field("db", &self.db)
            .field("listen", &self.listen)
            .field("node_id", &self.node_id)
            .field("cache_bytes", &self.cache_bytes)
            .field("snapshot_max_age", &self.snapshot_max_age)
            .field("shutdown_grace", &self.shutdown_grace)
            .field("sysinfo", &self.sysinfo.is_some())
            .finish()
    }
}

/// Asks a running server to stop; cloneable, usable from any task.
#[derive(Clone, Default)]
pub struct ShutdownHandle {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl ShutdownHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_triggered(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub async fn wait(&self) {
        loop {
            if self.is_triggered() {
                return;
            }
            let notified = self.notify.notified();
            if self.is_triggered() {
                return;
            }
            notified.await;
        }
    }
}

/// A started server.
pub struct Running {
    /// The address actually bound (a port-0 config gets the real port).
    pub addr: SocketAddr,
    shutdown: ShutdownHandle,
    task: tokio::task::JoinHandle<Result<(), StoreError>>,
    pub slot: Arc<StoreSlot>,
    pub raft: RaftNode,
}

impl Running {
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.shutdown.clone()
    }

    /// Ask it to stop (returns at once; [`wait`](Self::wait) for the end).
    pub fn shutdown(&self) {
        self.shutdown.trigger();
    }

    /// Wait until the server has stopped and everything is closed.
    pub async fn wait(self) -> Result<(), StoreError> {
        self.task
            .await
            .map_err(|e| StoreError::Storage(format!("server task: {e}")))?
    }
}

fn io_err(what: &str, e: std::io::Error) -> StoreError {
    StoreError::Storage(format!("{what}: {e}"))
}

/// Start a server on the current tokio runtime.
pub async fn start(
    cfg: ServeConfig,
    extractors: Vec<Arc<dyn Extractor>>,
) -> Result<Running, StoreError> {
    let hash = extractors_hash(&extractors);
    // redb's exclusive lock is the ownership check (`Locked` for a second
    // server on the same file); everything else follows.
    let slot = StoreSlot::open(&cfg.db, extractors, cfg.cache_bytes, cfg.snapshot_max_age)?;
    let listener = TcpListener::bind(cfg.listen)
        .await
        .map_err(|e| io_err(&format!("cannot listen on {}", cfg.listen), e))?;
    let addr = listener
        .local_addr()
        .map_err(|e| io_err("local address", e))?;
    let lock =
        LockFile::create(&cfg.db, &addr.to_string()).map_err(|e| io_err("LOCK sidecar", e))?;
    let raft = RaftNode::start(cfg.node_id, addr.to_string(), Arc::clone(&slot)).await?;
    let shutdown = ShutdownHandle::new();
    let ctx = Arc::new(Ctx {
        slot: Arc::clone(&slot),
        raft: raft.clone(),
        info: ServerInfo {
            node_id: cfg.node_id,
            cluster_id: "standalone".into(),
            extractors_hash: hash,
            db_path: cfg.db.display().to_string(),
            listen_addr: addr.to_string(),
            started: Instant::now(),
        },
        shutdown: shutdown.clone(),
        sysinfo: cfg.sysinfo.clone(),
    });

    // Health (D10): "" is SERVING once the store is open (now);
    // `memory-graph.ready` follows the known leader.
    let (reporter, health) = tonic_health::server::health_reporter();
    reporter
        .set_service_status("", ServingStatus::Serving)
        .await;
    {
        // Shutdown step 1: the moment shutdown starts, both health names
        // go NOT_SERVING (load balancers stop routing here during the
        // drain), before anything else is torn down.
        let reporter = reporter.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            shutdown.wait().await;
            mark_not_serving(&reporter).await;
        });
    }
    {
        let reporter = reporter.clone();
        let shutdown = shutdown.clone();
        let mut rx = raft.raft.metrics();
        tokio::spawn(async move {
            loop {
                if shutdown.is_triggered() {
                    return;
                }
                let ready = rx.borrow().current_leader.is_some();
                reporter
                    .set_service_status(
                        READY_SERVICE,
                        if ready {
                            ServingStatus::Serving
                        } else {
                            ServingStatus::NotServing
                        },
                    )
                    .await;
                if shutdown.is_triggered() {
                    // Raced the shutdown watcher: never leave SERVING behind.
                    mark_not_serving(&reporter).await;
                    return;
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
        });
    }
    tokio::spawn(SnapshotTable::reaper(Arc::downgrade(&slot)));

    let no_limit = usize::MAX;
    let router = tonic::transport::Server::builder()
        .add_service(health)
        .add_service(
            StoreServer::new(StoreService {
                ctx: Arc::clone(&ctx),
            })
            .max_decoding_message_size(no_limit)
            .max_encoding_message_size(no_limit),
        )
        .add_service(
            WriteServer::new(WriteService {
                ctx: Arc::clone(&ctx),
            })
            .max_decoding_message_size(no_limit)
            .max_encoding_message_size(no_limit),
        )
        .add_service(
            AdminServer::new(AdminService {
                ctx: Arc::clone(&ctx),
            })
            .max_decoding_message_size(no_limit)
            .max_encoding_message_size(no_limit),
        );
    let incoming = {
        let weak = Arc::downgrade(&slot);
        TcpListenerStream::new(listener).map(move |r| r.map(|t| ConnIo::for_slot(t, weak.clone())))
    };
    let signal = {
        let s = shutdown.clone();
        async move { s.wait().await }
    };
    let mut serve = tokio::spawn(router.serve_with_incoming_shutdown(incoming, signal));
    tracing::info!(%addr, db = %cfg.db.display(), node_id = cfg.node_id, "serving");

    let task = {
        let shutdown = shutdown.clone();
        let slot = Arc::clone(&slot);
        let raft = raft.clone();
        let reporter = reporter.clone();
        let grace = cfg.shutdown_grace;
        tokio::spawn(async move {
            let result = tokio::select! {
                r = &mut serve => match r {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(StoreError::Storage(format!("server: {e}"))),
                    Err(e) => Err(StoreError::Storage(format!("server task: {e}"))),
                },
                _ = async { shutdown.wait().await; tokio::time::sleep(grace).await } => {
                    tracing::warn!(?grace, "in-flight requests did not finish within the grace period; aborting");
                    serve.abort();
                    Ok(())
                }
            };
            // Health already went NOT_SERVING when shutdown began (above);
            // re-assert it in case serving ended on its own.
            mark_not_serving(&reporter).await;
            if tokio::time::timeout(grace, raft.shutdown()).await.is_err() {
                tracing::warn!(
                    ?grace,
                    "raft node did not shut down within the grace period; continuing"
                );
            }
            slot.close();
            // Last: the sidecar names this process as the holder until the
            // store is closed.
            drop(lock);
            tracing::info!("stopped");
            result
        })
    };
    Ok(Running {
        addr,
        shutdown,
        task,
        slot,
        raft,
    })
}

/// Shutdown step 1: report `""` and [`READY_SERVICE`] as `NOT_SERVING`.
pub(crate) async fn mark_not_serving(reporter: &tonic_health::server::HealthReporter) {
    reporter
        .set_service_status("", ServingStatus::NotServing)
        .await;
    reporter
        .set_service_status(READY_SERVICE, ServingStatus::NotServing)
        .await;
}

/// The blocking entry point (`memory-graph serve`): builds a multi-thread
/// runtime, serves until Ctrl-C / SIGTERM (or `Admin.Shutdown`), then
/// shuts down gracefully.
pub fn run_blocking(
    cfg: ServeConfig,
    extractors: Vec<Box<dyn Extractor>>,
) -> Result<(), StoreError> {
    run_blocking_with(cfg, extractors, |_| {})
}

/// [`run_blocking`], calling `on_ready` with the bound address once the
/// store is open, the LOCK sidecar written and the listener accepting (the
/// CLI prints it, so `--listen 127.0.0.1:0` is usable by scripts and tests).
pub fn run_blocking_with(
    cfg: ServeConfig,
    extractors: Vec<Box<dyn Extractor>>,
    on_ready: impl FnOnce(SocketAddr) + Send + 'static,
) -> Result<(), StoreError> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| io_err("tokio runtime", e))?;
    rt.block_on(async move {
        let running = start(cfg, share(extractors)).await?;
        on_ready(running.addr);
        let handle = running.shutdown_handle();
        tokio::spawn(async move {
            wait_for_signal().await;
            tracing::info!("signal received; shutting down");
            handle.trigger();
        });
        running.wait().await
    })
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic_health::pb::health_server::Health;
    use tonic_health::pb::{health_check_response::ServingStatus as Pb, HealthCheckRequest};
    use tonic_health::server::{HealthReporter, HealthService};

    async fn status(svc: &HealthService, name: &str) -> i32 {
        svc.check(tonic::Request::new(HealthCheckRequest {
            service: name.into(),
        }))
        .await
        .unwrap()
        .into_inner()
        .status
    }

    /// Shutdown step 1 reports both health names NOT_SERVING.
    #[tokio::test]
    async fn mark_not_serving_flips_both_health_names() {
        let reporter = HealthReporter::new();
        let svc = HealthService::from_health_reporter(reporter.clone());
        reporter
            .set_service_status(READY_SERVICE, ServingStatus::Serving)
            .await;
        assert_eq!(status(&svc, "").await, Pb::Serving as i32);
        assert_eq!(status(&svc, READY_SERVICE).await, Pb::Serving as i32);
        mark_not_serving(&reporter).await;
        assert_eq!(status(&svc, "").await, Pb::NotServing as i32);
        assert_eq!(status(&svc, READY_SERVICE).await, Pb::NotServing as i32);
    }
}
