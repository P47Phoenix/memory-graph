//! The connection: endpoint selection, the `Hello` handshake, and the retry
//! policy every RPC goes through.
use crate::{ClientConfig, ReadMode, RetryConfig};
use graph_proto::error::WireError;
use graph_proto::pb::admin_client::AdminClient;
use graph_proto::pb::store_client::StoreClient;
use graph_proto::pb::write_client::WriteClient;
use graph_proto::{pb, status_to_store_error, View, PROTOCOL_VERSION};
use graph_store::StoreError;
use rand::Rng;
use std::future::Future;
use std::sync::RwLock;
use std::time::{Duration, Instant};
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

fn endpoint_for(addr: &str) -> Result<Endpoint, StoreError> {
    let uri = if addr.contains("://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    };
    Endpoint::from_shared(uri)
        .map(|e| e.tcp_nodelay(true).connect_timeout(Duration::from_secs(5)))
        .map_err(|e| StoreError::Rejected(format!("bad endpoint `{addr}`: {e}")))
}

/// A status without a typed detail from the transport layer (connection
/// refused, reset) is `UNAVAILABLE` too; name the endpoint in that case.
fn map_status(endpoint: &str, s: &Status) -> StoreError {
    if s.details().is_empty() && s.code() == Code::Unavailable {
        return StoreError::Storage(format!("server {endpoint} unavailable: {}", s.message()));
    }
    status_to_store_error(s)
}

fn jitter(retry: &RetryConfig, attempt: u32) -> Duration {
    let exp = retry.base.saturating_mul(1u32 << attempt.min(16));
    let cap = exp.min(retry.cap);
    let half = cap / 2;
    let extra = cap.saturating_sub(half);
    let ms = rand::thread_rng().gen_range(0..=extra.as_millis().max(1) as u64);
    half + Duration::from_millis(ms)
}

pub fn store_client(ch: Channel) -> StoreClient<Channel> {
    StoreClient::new(ch)
        .max_decoding_message_size(NO_LIMIT)
        .max_encoding_message_size(NO_LIMIT)
}

pub fn write_client(ch: Channel) -> WriteClient<Channel> {
    WriteClient::new(ch)
        .max_decoding_message_size(NO_LIMIT)
        .max_encoding_message_size(NO_LIMIT)
}

pub fn admin_client(ch: Channel) -> AdminClient<Channel> {
    AdminClient::new(ch)
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
            let channel = endpoint_for(&ep)?.connect_lazy();
            match Self::hello_on(&channel, &cfg).await {
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
                        return Err(map_status("", &st));
                    }
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(st) => return Err(map_status("", &st)),
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
        let channel = endpoint_for(addr)?.connect_lazy();
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
    /// given), retrying `UNAVAILABLE` with jittered back-off within the
    /// budget (`Kind::Read`: the retry budget; `Kind::Write`: the write
    /// deadline), switching endpoint on a `NotLeader` that names the
    /// leader, and waiting `retry_after_ms` on `NoLeader`.
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
                    Duration::ZERO
                }
                WireError::NoLeader { retry_after_ms } => Duration::from_millis(*retry_after_ms)
                    .clamp(self.cfg.retry.base, self.cfg.retry.cap),
                _ if st.code() == Code::Unavailable => jitter(&self.cfg.retry, attempt),
                _ => return Err(map_status(&endpoint, &st)),
            };
            if Instant::now() + delay > deadline {
                return Err(map_status(&endpoint, &st));
            }
            tracing::debug!(endpoint = %endpoint, error = %st, ?delay, attempt, "retrying");
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(endpoint_for("127.0.0.1:7000").is_ok());
        assert!(endpoint_for("http://h:1").is_ok());
        assert!(endpoint_for("not a uri").is_err());
    }
}
