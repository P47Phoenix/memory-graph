//! [`StubProxy`]: a gRPC endpoint that answers every call with one fixed,
//! detail-less status, the way an authenticating proxy in front of a
//! server (Envoy, a mesh sidecar) refuses a call before it reaches
//! `memory-graph serve` (ADR 0012 D8: a detail-less `UNAUTHENTICATED` or
//! `PERMISSION_DENIED` maps to exit 8 or 9 all the same).
use std::future::Ready;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use tonic::codegen::http;

/// The running stub; stopped when dropped.
pub struct StubProxy {
    addr: SocketAddr,
    calls: Arc<AtomicU64>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StubProxy {
    /// Listen on `127.0.0.1:0` and answer every call with `code` and
    /// `message`, no details (no `StoreErrorDetail`).
    pub fn start(code: tonic::Code, message: &str) -> StubProxy {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("stub proxy bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("stub proxy addr");
        let calls = Arc::new(AtomicU64::new(0));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let refuse = Refuse {
            code,
            message: Arc::from(message),
            calls: Arc::clone(&calls),
        };
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("stub proxy runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
                let (_, health) = tonic_health::server::health_reporter();
                let _ = tonic::transport::Server::builder()
                    .layer(RefuseLayer(refuse))
                    .add_service(health)
                    .serve_with_incoming_shutdown(
                        tokio_stream::wrappers::TcpListenerStream::new(listener),
                        async {
                            let _ = rx.await;
                        },
                    )
                    .await;
            });
        });
        StubProxy {
            addr,
            calls,
            stop: Some(tx),
            thread: Some(thread),
        }
    }

    /// `host:port`.
    pub fn endpoint(&self) -> String {
        self.addr.to_string()
    }

    /// Calls answered so far (every one refused).
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
}

impl Drop for StubProxy {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Clone)]
struct Refuse {
    code: tonic::Code,
    message: Arc<str>,
    calls: Arc<AtomicU64>,
}

#[derive(Clone)]
struct RefuseLayer(Refuse);

impl<S> tower_layer::Layer<S> for RefuseLayer {
    type Service = RefuseService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        RefuseService {
            _inner: inner,
            refuse: self.0.clone(),
        }
    }
}

#[derive(Clone)]
struct RefuseService<S> {
    _inner: S,
    refuse: Refuse,
}

impl<S, B, RB> tower_service::Service<http::Request<B>> for RefuseService<S>
where
    S: tower_service::Service<http::Request<B>, Response = http::Response<RB>>,
    RB: Default,
{
    type Response = http::Response<RB>;
    type Error = S::Error;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: http::Request<B>) -> Self::Future {
        self.refuse.calls.fetch_add(1, Ordering::Relaxed);
        let st = tonic::Status::new(self.refuse.code, self.refuse.message.to_string());
        std::future::ready(Ok(st.into_http::<RB>()))
    }
}
