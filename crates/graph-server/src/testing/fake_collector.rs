//! An in-process OTLP/gRPC collector for tests (ADR 0009): `TraceService`,
//! `MetricsService` and `LogsService` on 127.0.0.1:0, recording every
//! export request, with a deadline-bounded [`FakeCollector::wait_for`].
//!
//! Failure modes for the "a collector problem never hurts serving" tests:
//! [`stall`](FakeCollector::stall) (accept, never answer until
//! [`release`](FakeCollector::release)), [`refuse`](FakeCollector::refuse)
//! (answer `UNAVAILABLE`), and [`stop`](FakeCollector::stop) /
//! [`restart`](FakeCollector::restart) on the same port.
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
    /// Export calls answered `UNAVAILABLE` while refusing.
    pub refused: usize,
    /// Export calls held, unanswered, while stalled (and not yet released).
    pub stalled: usize,
}

/// Which export requests [`FakeCollector::wait_for`] counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Traces,
    Metrics,
    Logs,
    /// Calls refused ([`FakeCollector::refuse`]).
    Refused,
    /// Calls currently held ([`FakeCollector::stall`]).
    Stalled,
}

impl Received {
    pub fn count(&self, signal: Signal) -> usize {
        match signal {
            Signal::Traces => self.traces.len(),
            Signal::Metrics => self.metrics.len(),
            Signal::Logs => self.logs.len(),
            Signal::Refused => self.refused,
            Signal::Stalled => self.stalled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Accept,
    Stall,
    Refuse,
}

struct Shared {
    received: Mutex<Received>,
    changed: Condvar,
    mode: tokio::sync::watch::Sender<Mode>,
}

impl Shared {
    fn update(&self, change: impl FnOnce(&mut Received)) {
        change(&mut self.received.lock().unwrap_or_else(PoisonError::into_inner));
        self.changed.notify_all();
    }

    /// Apply the current mode to one call, then record it with `add`.
    async fn handle(&self, add: impl FnOnce(&mut Received)) -> Result<(), Status> {
        let mut mode = self.mode.subscribe();
        if *mode.borrow_and_update() == Mode::Stall {
            self.update(|r| r.stalled += 1);
            let _ = mode.wait_for(|m| *m != Mode::Stall).await;
            self.update(|r| r.stalled -= 1);
        }
        if *mode.borrow() == Mode::Refuse {
            self.update(|r| r.refused += 1);
            return Err(Status::unavailable("fake collector refuses"));
        }
        self.update(add);
        Ok(())
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
        let request = request.into_inner();
        self.0.handle(|r| r.traces.push(request)).await?;
        Ok(Response::new(ExportTraceServiceResponse::default()))
    }
}

#[tonic::async_trait]
impl MetricsService for Service {
    async fn export(
        &self,
        request: Request<ExportMetricsServiceRequest>,
    ) -> Result<Response<ExportMetricsServiceResponse>, Status> {
        let request = request.into_inner();
        self.0.handle(|r| r.metrics.push(request)).await?;
        Ok(Response::new(ExportMetricsServiceResponse::default()))
    }
}

#[tonic::async_trait]
impl LogsService for Service {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        let request = request.into_inner();
        self.0.handle(|r| r.logs.push(request)).await?;
        Ok(Response::new(ExportLogsServiceResponse::default()))
    }
}

/// A running server: its stop signal and task.
struct Serving {
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

/// The collector; it stops when dropped.
pub struct FakeCollector {
    addr: SocketAddr,
    shared: Arc<Shared>,
    serving: Option<Serving>,
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
        let shared = Arc::new(Shared {
            received: Mutex::new(Received::default()),
            changed: Condvar::new(),
            mode: tokio::sync::watch::Sender::new(Mode::Accept),
        });
        let serving = serve(&runtime, listener, &shared);
        FakeCollector {
            addr,
            shared,
            serving: Some(serving),
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

    /// Hold every export call unanswered until [`release`](Self::release).
    pub fn stall(&self) {
        self.shared.mode.send_replace(Mode::Stall);
    }

    /// Answer every export call `UNAVAILABLE` until [`accept`](Self::accept).
    pub fn refuse(&self) {
        self.shared.mode.send_replace(Mode::Refuse);
    }

    /// Back to normal: record and answer every call (held calls too).
    pub fn accept(&self) {
        self.shared.mode.send_replace(Mode::Accept);
    }

    /// End a [`stall`](Self::stall): held calls are recorded and answered.
    pub fn release(&self) {
        self.accept();
    }

    /// Close the listener and every connection; the port stays reserved
    /// for [`restart`](Self::restart).
    pub fn stop(&mut self) {
        let (Some(serving), Some(runtime)) = (self.serving.take(), self.runtime.as_ref()) else {
            return;
        };
        let _ = serving.stop.send(());
        let _ = runtime.block_on(serving.task);
    }

    /// Serve again on the same port after [`stop`](Self::stop).
    pub fn restart(&mut self) {
        self.stop();
        let runtime = self.runtime.as_ref().expect("collector runtime");
        let until = Instant::now() + Duration::from_secs(10);
        let listener = loop {
            match runtime.block_on(tokio::net::TcpListener::bind(self.addr)) {
                Ok(l) => break l,
                Err(e) if Instant::now() < until => {
                    let _ = e;
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("fake collector rebinds {}: {e}", self.addr),
            }
        };
        self.serving = Some(serve(runtime, listener, &self.shared));
    }

    /// Wait until at least `count` of `signal` arrived or `deadline`
    /// passes; whether they did.
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

fn serve(
    runtime: &tokio::runtime::Runtime,
    listener: tokio::net::TcpListener,
    shared: &Arc<Shared>,
) -> Serving {
    let service = Service(Arc::clone(shared));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = runtime.spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(TraceServiceServer::new(service.clone()))
            .add_service(MetricsServiceServer::new(service.clone()))
            .add_service(LogsServiceServer::new(service))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            )
            .await;
    });
    Serving { stop, task }
}

impl Drop for FakeCollector {
    fn drop(&mut self) {
        // Let held calls go so the graceful shutdown can finish.
        self.release();
        if let Some(serving) = self.serving.take() {
            let _ = serving.stop.send(());
        }
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::collector::trace::v1::trace_service_client::TraceServiceClient;

    const WAIT: Duration = Duration::from_secs(10);

    /// One export call from a client of our own, on a runtime of its own.
    fn export_once(endpoint: &str, timeout: Duration) -> Result<(), Status> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("client runtime");
        rt.block_on(async {
            let channel = tonic::transport::Endpoint::from_shared(endpoint.to_string())
                .expect("endpoint")
                .timeout(timeout)
                .connect()
                .await
                .map_err(|e| Status::unavailable(e.to_string()))?;
            TraceServiceClient::new(channel)
                .export(ExportTraceServiceRequest::default())
                .await
                .map(|_| ())
        })
    }

    #[test]
    fn accepts_and_records() {
        let c = FakeCollector::start();
        export_once(&c.endpoint(), WAIT).expect("accepted");
        assert!(c.wait_for(Signal::Traces, 1, WAIT));
    }

    #[test]
    fn refuse_answers_unavailable_and_counts() {
        let c = FakeCollector::start();
        c.refuse();
        let e = export_once(&c.endpoint(), WAIT).unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unavailable);
        assert!(c.wait_for(Signal::Refused, 1, WAIT));
        assert_eq!(c.received().traces.len(), 0);
        c.accept();
        export_once(&c.endpoint(), WAIT).expect("accepted again");
        assert!(c.wait_for(Signal::Traces, 1, WAIT));
    }

    #[test]
    fn stall_holds_calls_until_released() {
        let c = FakeCollector::start();
        c.stall();
        let endpoint = c.endpoint();
        let call = std::thread::spawn(move || export_once(&endpoint, WAIT));
        assert!(c.wait_for(Signal::Stalled, 1, WAIT), "the call is held");
        assert_eq!(c.received().traces.len(), 0);
        c.release();
        call.join()
            .expect("client thread")
            .expect("answered after release");
        assert!(c.wait_for(Signal::Traces, 1, WAIT));
        assert_eq!(c.received().stalled, 0);
    }

    #[test]
    fn stop_and_restart_on_the_same_port() {
        let mut c = FakeCollector::start();
        let endpoint = c.endpoint();
        c.stop();
        assert!(
            export_once(&endpoint, Duration::from_secs(2)).is_err(),
            "nothing listens"
        );
        c.restart();
        assert_eq!(c.endpoint(), endpoint);
        export_once(&endpoint, WAIT).expect("served again");
        assert!(c.wait_for(Signal::Traces, 1, WAIT));
    }
}
