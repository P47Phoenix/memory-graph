//! The connection: endpoint selection, the `Hello` handshake, and the retry
//! policy every RPC goes through.
use crate::{ClientConfig, ReadMode, RetryConfig};
use graph_proto::error::is_transport_loss;
use graph_proto::error::WireError;
use graph_proto::pb::admin_client::AdminClient;
use graph_proto::pb::store_client::StoreClient;
use graph_proto::pb::write_client::WriteClient;
use graph_proto::{pb, status_to_store_error, SendVersion, View, PROTOCOL_VERSION};
use graph_store::StoreError;
use rand::Rng;
use std::future::Future;
use std::sync::RwLock;
use std::time::{Duration, Instant};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Status};

/// What the server said in `Hello`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloInfo {
    pub protocol_version: u32,
    pub server_version: String,
    pub store_format_version: u64,
    pub extractors_hash: String,
    pub node_id: u64,
    pub leader_id: Option<u64>,
    pub leader_addr: Option<String>,
    pub cluster_id: String,
    /// The endpoint that answered.
    pub endpoint: String,
}

impl From<(pb::HelloResponse, String)> for HelloInfo {
    fn from((h, endpoint): (pb::HelloResponse, String)) -> Self {
        Self {
            protocol_version: h.protocol_version,
            server_version: h.server_version,
            store_format_version: h.store_format_version,
            extractors_hash: h.extractors_hash,
            node_id: h.node_id,
            leader_id: h.leader_id,
            leader_addr: h.leader_addr,
            cluster_id: h.cluster_id,
            endpoint,
        }
    }
}

/// Drive `fut` to completion on `rt` from synchronous code. Panics when
/// called from inside any tokio runtime: the caller would block a runtime
/// worker (or deadlock a current-thread runtime) waiting on another.
#[track_caller]
pub fn block_on<F: Future>(rt: &tokio::runtime::Runtime, fut: F) -> F::Output {
    if tokio::runtime::Handle::try_current().is_ok() {
        panic!(
            "graph_client::RemoteStore was called from inside a tokio runtime; its Store methods \
             are a synchronous facade that blocks on an owned runtime. Call it from a plain thread \
             (std::thread::spawn or tokio::task::spawn_blocking), not from an async task."
        );
    }
    rt.block_on(fut)
}

/// Which deadline an RPC retries under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Read,
    Write,
}

struct Active {
    endpoint: String,
    channel: Channel,
}

pub struct Conn {
    cfg: ClientConfig,
    active: RwLock<Active>,
    hello: HelloInfo,
}

/// No message size limit on either direction: a `FileBytes` may carry a
/// file up to the store's 4 GiB cap and a `Children` answer a large file.
const NO_LIMIT: usize = usize::MAX;

fn endpoint_for(addr: &str, connect_timeout: Duration) -> Result<Endpoint, StoreError> {
    let uri = if addr.contains("://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    };
    Endpoint::from_shared(uri)
        .map(|e| e.tcp_nodelay(true).connect_timeout(connect_timeout))
        .map_err(|e| StoreError::Rejected(format!("bad endpoint `{addr}`: {e}")))
}

/// A status without a typed detail from the transport layer (connection
/// refused, reset) is `UNAVAILABLE`; a connection lost under a call in
/// flight (the server died mid-stream) is `UNKNOWN` "transport error",
/// `CANCELLED` or `DEADLINE_EXCEEDED`. Both name the endpoint, and neither
/// is ever a protocol error.
fn map_status(endpoint: &str, s: &Status) -> StoreError {
    if s.details().is_empty() {
        if s.code() == Code::Unavailable {
            return StoreError::Storage(format!("server {endpoint} unavailable: {}", s.message()));
        }
        if is_transport_loss(s.code()) {
            return StoreError::Storage(format!(
                "server {endpoint} connection lost: {}",
                s.message()
            ));
        }
    }
    status_to_store_error(s)
}

/// Whether a failed call may be retried: `UNAVAILABLE` always (a typed
/// `Locked`, or the transport), and for a write also a detail-less
/// transport loss (every write is idempotent on a retry of the same
/// request, ADR 0004 D2/D7, so a write that did land is safe to resend).
fn retryable(kind: Kind, s: &Status) -> bool {
    s.code() == Code::Unavailable
        || (kind == Kind::Write && s.details().is_empty() && is_transport_loss(s.code()))
}

/// Consecutive `NotLeader` switches after which the client backs off
/// before the next one (two nodes naming each other as leader during an
/// election would otherwise ping-pong with no delay).
const SWITCHES_BEFORE_BACKOFF: u32 = 2;

/// The delay before following a `NotLeader` redirect: none for the first
/// switch in a row, jittered from the second on.
fn switch_delay(retry: &RetryConfig, consecutive_switches: u32, attempt: u32) -> Duration {
    if consecutive_switches >= SWITCHES_BEFORE_BACKOFF {
        jitter(retry, attempt)
    } else {
        Duration::ZERO
    }
}

fn jitter(retry: &RetryConfig, attempt: u32) -> Duration {
    let exp = retry.base.saturating_mul(1u32 << attempt.min(16));
    let cap = exp.min(retry.cap);
    let half = cap / 2;
    let extra = cap.saturating_sub(half);
    let ms = rand::thread_rng().gen_range(0..=extra.as_millis().max(1) as u64);
    half + Duration::from_millis(ms)
}

/// A channel whose every call carries the `mg-protocol-version` header.
pub type VersionedChannel = InterceptedService<Channel, SendVersion>;

pub fn store_client(ch: Channel) -> StoreClient<VersionedChannel> {
    StoreClient::with_interceptor(ch, SendVersion)
        .max_decoding_message_size(NO_LIMIT)
        .max_encoding_message_size(NO_LIMIT)
}

pub fn write_client(ch: Channel) -> WriteClient<VersionedChannel> {
    WriteClient::with_interceptor(ch, SendVersion)
        .max_decoding_message_size(NO_LIMIT)
        .max_encoding_message_size(NO_LIMIT)
}

pub fn admin_client(ch: Channel) -> AdminClient<VersionedChannel> {
    AdminClient::with_interceptor(ch, SendVersion)
        .max_decoding_message_size(NO_LIMIT)
        .max_encoding_message_size(NO_LIMIT)
}

pub fn health_client(ch: Channel) -> tonic_health::pb::health_client::HealthClient<Channel> {
    tonic_health::pb::health_client::HealthClient::new(ch)
}

impl Conn {
    /// Connect to the first endpoint that answers `Hello` with the right
    /// protocol version.
    pub async fn connect(cfg: ClientConfig) -> Result<Conn, StoreError> {
        if cfg.endpoints.is_empty() {
            return Err(StoreError::Rejected("no server endpoint given".into()));
        }
        let mut last = None;
        for ep in cfg.endpoints.clone() {
            let channel = endpoint_for(&ep, cfg.connect_timeout)?.connect_lazy();
            match Self::hello_on(&ep, &channel, &cfg).await {
                Ok(h) => {
                    let hello = HelloInfo::from((h, ep.clone()));
                    return Ok(Conn {
                        cfg,
                        active: RwLock::new(Active {
                            endpoint: ep,
                            channel,
                        }),
                        hello,
                    });
                }
                Err(e) => {
                    tracing::debug!(endpoint = %ep, error = %e, "hello failed");
                    // A protocol mismatch is definitive for that server;
                    // an unreachable endpoint means try the next.
                    if matches!(e, StoreError::Protocol(_)) {
                        return Err(e);
                    }
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| StoreError::Rejected("no server endpoint given".into())))
    }

    async fn hello_on(
        endpoint: &str,
        channel: &Channel,
        cfg: &ClientConfig,
    ) -> Result<pb::HelloResponse, StoreError> {
        let mut client = store_client(channel.clone());
        let deadline = Instant::now() + cfg.retry.budget;
        let mut attempt = 0u32;
        loop {
            let r = client
                .hello(pb::HelloRequest {
                    protocol_version: PROTOCOL_VERSION,
                    client_version: cfg.client_version.clone(),
                })
                .await;
            match r {
                Ok(resp) => {
                    let h = resp.into_inner();
                    if h.protocol_version != PROTOCOL_VERSION {
                        return Err(StoreError::Protocol(format!(
                            "server speaks protocol version {}, this client speaks {PROTOCOL_VERSION}",
                            h.protocol_version
                        )));
                    }
                    return Ok(h);
                }
                Err(st) if st.code() == Code::Unavailable && st.details().is_empty() => {
                    let delay = jitter(&cfg.retry, attempt);
                    if Instant::now() + delay > deadline {
                        return Err(map_status(endpoint, &st));
                    }
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(st) => return Err(map_status(endpoint, &st)),
            }
        }
    }

    pub fn hello(&self) -> &HelloInfo {
        &self.hello
    }

    pub fn config(&self) -> &ClientConfig {
        &self.cfg
    }

    /// The read view for this connection's configured mode.
    pub fn view(&self) -> View {
        match self.cfg.read_mode {
            ReadMode::Local => View::Local,
            ReadMode::Linearizable => View::Linearizable,
        }
    }

    fn channel(&self) -> (String, Channel) {
        let a = self
            .active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (a.endpoint.clone(), a.channel.clone())
    }

    fn switch_to(&self, addr: &str) -> Result<(), StoreError> {
        let channel = endpoint_for(addr, self.cfg.connect_timeout)?.connect_lazy();
        let mut a = self
            .active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tracing::info!(from = %a.endpoint, to = %addr, "switching to the leader");
        *a = Active {
            endpoint: addr.to_string(),
            channel,
        };
        Ok(())
    }

    /// Run `f` (which builds and awaits one RPC on the channel it is
    /// given), retrying `UNAVAILABLE` (and, for a write, a lost connection)
    /// with jittered back-off within the budget (`Kind::Read`: the retry
    /// budget; `Kind::Write`: the write deadline), switching endpoint on a
    /// `NotLeader` that names the leader (with back-off from the second
    /// switch in a row), and waiting `retry_after_ms` on `NoLeader`.
    ///
    /// A write that exhausts its deadline on a retryable error answers
    /// `NoLeader` (exit code 4 in the CLI): no leader accepted it in time,
    /// whether the server was unreachable, lost mid-call or electing.
    pub async fn call<T, F, Fut>(&self, kind: Kind, f: F) -> Result<T, StoreError>
    where
        F: Fn(Channel) -> Fut,
        Fut: Future<Output = Result<T, Status>>,
    {
        let budget = match kind {
            Kind::Read => self.cfg.retry.budget,
            Kind::Write => self.cfg.write_deadline,
        };
        let deadline = Instant::now() + budget;
        let mut attempt = 0u32;
        let mut switches = 0u32;
        loop {
            let (endpoint, channel) = self.channel();
            let st = match f(channel).await {
                Ok(v) => return Ok(v),
                Err(st) => st,
            };
            let wire = WireError::from(&st);
            let delay = match &wire {
                WireError::NotLeader {
                    leader_addr: Some(addr),
                    ..
                } => {
                    if Instant::now() >= deadline {
                        return Err(wire.into());
                    }
                    self.switch_to(addr)?;
                    switches += 1;
                    switch_delay(&self.cfg.retry, switches, attempt)
                }
                WireError::NoLeader { retry_after_ms } => {
                    switches = 0;
                    Duration::from_millis(*retry_after_ms)
                        .clamp(self.cfg.retry.base, self.cfg.retry.cap)
                }
                _ if retryable(kind, &st) => {
                    switches = 0;
                    jitter(&self.cfg.retry, attempt)
                }
                _ => return Err(map_status(&endpoint, &st)),
            };
            if Instant::now() + delay > deadline {
                return Err(deadline_error(kind, &endpoint, &st, &self.cfg.retry));
            }
            tracing::debug!(endpoint = %endpoint, error = %st, ?delay, attempt, "retrying");
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }
}

/// The error for a call whose budget ran out on a retryable status: for a
/// write, `NoLeader` (unless the server itself named a leader: then the
/// `NotLeader` it sent); for a read, the mapped status.
fn deadline_error(kind: Kind, endpoint: &str, st: &Status, retry: &RetryConfig) -> StoreError {
    let mapped = map_status(endpoint, st);
    match (kind, mapped) {
        (Kind::Write, e @ StoreError::NotLeader { .. }) => e,
        (Kind::Write, e) => {
            tracing::warn!(endpoint, error = %e, "write deadline exhausted");
            StoreError::NoLeader {
                retry_after_ms: retry.cap.as_millis() as u64,
            }
        }
        (Kind::Read, e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_loss_is_never_protocol_and_is_retryable_only_for_writes() {
        let st = Status::unknown("transport error");
        let e = map_status("h:1", &st);
        assert!(
            matches!(e, StoreError::Storage(ref m) if m == "server h:1 connection lost: transport error"),
            "{e:?}"
        );
        assert!(retryable(Kind::Write, &st));
        assert!(!retryable(Kind::Read, &st));
        assert!(retryable(Kind::Read, &Status::unavailable("refused")));
        // A typed detail is the server's own answer, not a lost connection.
        let typed: Status = graph_proto::WireError::Store(StoreError::Storage("x".into())).into();
        assert!(!retryable(Kind::Write, &typed));
    }

    #[test]
    fn a_write_deadline_is_no_leader_and_a_read_deadline_is_the_status() {
        let r = RetryConfig::default();
        let st = Status::unavailable("tcp connect error");
        assert!(matches!(
            deadline_error(Kind::Write, "h:1", &st, &r),
            StoreError::NoLeader { .. }
        ));
        assert!(matches!(
            deadline_error(Kind::Write, "h:1", &Status::unknown("transport error"), &r),
            StoreError::NoLeader { .. }
        ));
        assert!(matches!(
            deadline_error(Kind::Read, "h:1", &st, &r),
            StoreError::Storage(ref m) if m.contains("server h:1 unavailable")
        ));
    }

    #[test]
    fn leader_switches_back_off_from_the_second_in_a_row() {
        let r = RetryConfig {
            base: Duration::from_millis(50),
            cap: Duration::from_millis(400),
            budget: Duration::from_secs(5),
        };
        assert_eq!(switch_delay(&r, 1, 0), Duration::ZERO);
        assert!(switch_delay(&r, 2, 1) >= Duration::from_millis(50));
        assert!(switch_delay(&r, 5, 3) >= Duration::from_millis(200));
    }

    #[test]
    fn jitter_grows_and_caps() {
        let r = RetryConfig {
            base: Duration::from_millis(50),
            cap: Duration::from_millis(400),
            budget: Duration::from_secs(5),
        };
        for attempt in 0..10 {
            let d = jitter(&r, attempt);
            let exp = (50u64 << attempt).min(400);
            assert!(
                d.as_millis() as u64 >= exp / 2 && d.as_millis() as u64 <= exp,
                "{attempt}: {d:?}"
            );
        }
    }

    #[test]
    fn endpoints_accept_host_port_or_uri() {
        assert!(endpoint_for("127.0.0.1:7000", Duration::from_secs(1)).is_ok());
        assert!(endpoint_for("http://h:1", Duration::from_secs(1)).is_ok());
        assert!(endpoint_for("not a uri", Duration::from_secs(1)).is_err());
    }
}
