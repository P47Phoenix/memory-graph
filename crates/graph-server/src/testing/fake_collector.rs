//! An in-process OTLP/gRPC collector for tests (ADR 0009): `TraceService`,
//! `MetricsService` and `LogsService` on 127.0.0.1:0, recording every
//! export request, with a deadline-bounded [`FakeCollector::wait_for`].
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::{
    LogsService, LogsServiceServer,
};
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_server::{
    MetricsService, MetricsServiceServer,
};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::{
    TraceService, TraceServiceServer,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use std::net::SocketAddr;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tonic::{Request, Response, Status};

/// What the collector has received so far.
#[derive(Debug, Clone, Default)]
pub struct Received {
    pub traces: Vec<ExportTraceServiceRequest>,
    pub metrics: Vec<ExportMetricsServiceRequest>,
    pub logs: Vec<ExportLogsServiceRequest>,
}

/// Which export requests [`FakeCollector::wait_for`] counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Traces,
    Metrics,
    Logs,
}

impl Received {
    pub fn count(&self, signal: Signal) -> usize {
        match signal {
            Signal::Traces => self.traces.len(),
            Signal::Metrics => self.metrics.len(),
            Signal::Logs => self.logs.len(),
        }
    }
}

#[derive(Default)]
struct Shared {
    received: Mutex<Received>,
    changed: Condvar,
}

impl Shared {
    fn record(&self, add: impl FnOnce(&mut Received)) {
        add(&mut self.received.lock().unwrap_or_else(PoisonError::into_inner));
        self.changed.notify_all();
    }
}

#[derive(Clone)]
struct Service(Arc<Shared>);

#[tonic::async_trait]
impl TraceService for Service {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        self.0.record(|r| r.traces.push(request.into_inner()));
        Ok(Response::new(ExportTraceServiceResponse::default()))
    }
}

#[tonic::async_trait]
impl MetricsService for Service {
    async fn export(
        &self,
        request: Request<ExportMetricsServiceRequest>,
    ) -> Result<Response<ExportMetricsServiceResponse>, Status> {
        self.0.record(|r| r.metrics.push(request.into_inner()));
        Ok(Response::new(ExportMetricsServiceResponse::default()))
    }
}

#[tonic::async_trait]
impl LogsService for Service {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        self.0.record(|r| r.logs.push(request.into_inner()));
        Ok(Response::new(ExportLogsServiceResponse::default()))
    }
}

/// The collector; it stops when dropped.
pub struct FakeCollector {
    addr: SocketAddr,
    shared: Arc<Shared>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl FakeCollector {
    /// Start on a free loopback port; panics on failure (tests only).
    pub fn start() -> FakeCollector {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("fake-otlp")
            .enable_all()
            .build()
            .expect("fake collector runtime");
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("fake collector binds");
        let addr = listener.local_addr().expect("bound address");
        let shared = Arc::new(Shared::default());
        let service = Service(Arc::clone(&shared));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        runtime.spawn(
            tonic::transport::Server::builder()
                .add_service(TraceServiceServer::new(service.clone()))
                .add_service(MetricsServiceServer::new(service.clone()))
                .add_service(LogsServiceServer::new(service))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    async {
                        let _ = stopped.await;
                    },
                ),
        );
        FakeCollector {
            addr,
            shared,
            stop: Some(stop),
            runtime: Some(runtime),
        }
    }

    /// `http://127.0.0.1:<port>`, for `--otlp-endpoint`.
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// A copy of everything received so far.
    pub fn received(&self) -> Received {
        self.shared
            .received
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Wait until at least `count` export requests of `signal` arrived or
    /// `deadline` passes; whether they did.
    pub fn wait_for(&self, signal: Signal, count: usize, deadline: Duration) -> bool {
        let until = Instant::now() + deadline;
        let mut received = self
            .shared
            .received
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while received.count(signal) < count {
            let Some(left) = until.checked_duration_since(Instant::now()) else {
                return false;
            };
            received = self
                .shared
                .changed
                .wait_timeout(received, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }
}

impl Drop for FakeCollector {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}
