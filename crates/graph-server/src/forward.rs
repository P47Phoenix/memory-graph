//! Server-side forwarding to the leader (ADR 0004 D8/D9).
//!
//! A write, a membership change or a linearizable read's barrier that
//! reaches a node which is not the leader is sent on to the leader over the
//! ordinary client services (`Write`, `Admin`), from inside the handler:
//! async, no `block_on`, one cached channel per leader address. The client
//! sees one answer from the node it called, with `forwarded_to_leader` set
//! on writes. `Index` is forwarded as a stream, message by message through
//! a small bounded channel, so a follower never holds a whole batch.
//!
//! Loops are impossible: a forwarded request carries the
//! [`FORWARDED_BY_HEADER`] header, and a node that receives one handles it
//! locally, which on a non-leader answers `NotLeader` naming the leader (the
//! client then goes there itself). With no leader known the node answers
//! `NoLeader { retry_after_ms }` and the client retries until its write
//! deadline. A forward that cannot reach the leader (transport failure, or
//! the test fault plan cutting the link) is `NoLeader` too; any answer the
//! leader gave is passed through unchanged.
use crate::raft::network::FaultPlan;
use crate::raft::node::NO_LEADER_RETRY_MS;
use crate::raft::{NodeId, RaftNode};
use graph_proto::pb::admin_client::AdminClient;
use graph_proto::pb::store_client::StoreClient;
use graph_proto::pb::write_client::WriteClient;
use graph_proto::{store_error_to_status, PROTOCOL_VERSION, PROTOCOL_VERSION_HEADER};
use graph_store::StoreError;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tonic::service::interceptor::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request, Status};

/// The metadata key a forwarded request carries: the id of the node that
/// forwarded it. A node never forwards a request that has it.
pub const FORWARDED_BY_HEADER: &str = "mg-forwarded-by";

/// TCP connect timeout of a forwarding channel.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// No message size limit: a forwarded `FileBytes` may be as large as the
/// client's.
const NO_LIMIT: usize = usize::MAX;

/// The deadline of a forwarded unary write or read barrier when the client
/// sent none (`grpc-timeout`).
pub const FORWARD_UNARY_TIMEOUT: Duration = Duration::from_secs(60);

/// ... of a forwarded `Index` stream (a whole batch).
pub const FORWARD_INDEX_TIMEOUT: Duration = Duration::from_secs(600);

/// ... of a forwarded membership change (`AddLearner --blocking` waits up
/// to a minute for catch-up, `TransferLeader` up to its own wait).
pub const FORWARD_ADMIN_TIMEOUT: Duration = Duration::from_secs(120);

/// The client's remaining deadline (`grpc-timeout`, gRPC's wire format:
/// up to 8 digits and a unit `H`/`M`/`S`/`m`/`u`/`n`), when it sent one.
pub fn incoming_timeout(md: &tonic::metadata::MetadataMap) -> Option<Duration> {
    let v = md.get("grpc-timeout")?.to_str().ok()?;
    if v.len() < 2 || v.len() > 9 {
        return None;
    }
    let (digits, unit) = v.split_at(v.len() - 1);
    let n: u64 = digits.parse().ok()?;
    Some(match unit {
        "H" => Duration::from_secs(n.checked_mul(3600)?),
        "M" => Duration::from_secs(n.checked_mul(60)?),
        "S" => Duration::from_secs(n),
        "m" => Duration::from_millis(n),
        "u" => Duration::from_micros(n),
        "n" => Duration::from_nanos(n),
        _ => return None,
    })
}

/// Run a forwarded call under `deadline`: the call itself carries it as
/// `grpc-timeout` (see [`Forwarder::request`]) so the leader stops working
/// on it, and this node stops waiting at the same moment (a leader that
/// hangs cannot hold the handler forever). An expired deadline answers
/// `DEADLINE_EXCEEDED`.
///
/// The call runs in a `forward` span (ADR 0009 D5), a child of the
/// request's `rpc` span, with `memory_graph.forwarded_by` = `me`;
/// [`ForwardHeaders`] sends its W3C context, so the leader's `rpc` span is
/// its child.
pub async fn within<T>(
    me: NodeId,
    deadline: Duration,
    call: impl std::future::Future<Output = Result<T, Status>>,
) -> Result<T, Status> {
    use tracing::Instrument;
    let span = tracing::info_span!(
        "forward",
        memory_graph.forwarded_by = me,
        outcome = tracing::field::Empty,
    );
    let r = match tokio::time::timeout(deadline, call)
        .instrument(span.clone())
        .await
    {
        Ok(r) => r,
        Err(_) => Err(Status::deadline_exceeded(format!(
            "the forwarded request got no answer from the leader within {deadline:?}"
        ))),
    };
    if let Err(st) = &r {
        span.record("outcome", format!("{:?}", st.code()).as_str());
    } else {
        span.record("outcome", "ok");
    }
    r
}

/// Stamps the protocol version and [`FORWARDED_BY_HEADER`] on a forwarded
/// request.
#[derive(Debug, Clone, Copy)]
pub struct ForwardHeaders {
    pub me: NodeId,
}

impl Interceptor for ForwardHeaders {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        let md = req.metadata_mut();
        graph_proto::trace_context::inject_current(md);
        md.insert(
            PROTOCOL_VERSION_HEADER,
            PROTOCOL_VERSION
                .to_string()
                .parse()
                .expect("a number is valid ASCII metadata"),
        );
        md.insert(
            FORWARDED_BY_HEADER,
            self.me
                .to_string()
                .parse()
                .expect("a number is valid ASCII metadata"),
        );
        Ok(req)
    }
}

/// A channel to the leader with the forwarding headers.
pub type ForwardChannel = InterceptedService<Channel, ForwardHeaders>;

/// Where a request is handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Here: this node leads, or the request was already forwarded once.
    Local,
    /// Forward to the leader at this address.
    Leader { id: NodeId, addr: String },
}

/// This node's forwarding state: channels by leader address, the fault
/// plan (tests), and the `writes_forwarded_total` counter.
pub struct Forwarder {
    me: NodeId,
    faults: Option<FaultPlan>,
    channels: Mutex<HashMap<String, Channel>>,
    forwarded: AtomicU64,
    default_timeout: Option<Duration>,
}

/// Whether `req` was forwarded by another node.
pub fn was_forwarded<T>(req: &Request<T>) -> bool {
    req.metadata().get(FORWARDED_BY_HEADER).is_some()
}

fn no_leader(why: &str) -> Status {
    tracing::debug!(why, "forwarding: no leader reachable");
    store_error_to_status(&StoreError::NoLeader {
        retry_after_ms: NO_LEADER_RETRY_MS,
    })
}

/// What a failed forwarded call answers the client: the leader's own answer
/// when it gave one (a typed detail, or any status that is not a transport
/// failure), else `NoLeader` (the leader could not be reached; the client
/// retries until its deadline). Every write is idempotent on a retry
/// (ADR 0004 D2/D7), so a forward lost after it landed is safe to resend.
///
/// Note that `Cancelled` / `Unknown` transport losses (and a forward that
/// hit its deadline) are ambiguous: the leader may have committed the
/// write before the link failed, and the client's retry then applies it a
/// second time. That is safe only because every write is idempotent:
/// `Index`/`IndexFile` answer `unchanged`, `Prune` with the same keep set
/// removes nothing more, `Vacuum` finds nothing left to reclaim, and a
/// membership change is refused or a no-op (`tests/membership.rs`,
/// `prune_and_vacuum_sent_twice_are_idempotent`).
pub fn forward_error(st: Status) -> Status {
    if !st.details().is_empty() {
        return st;
    }
    if st.code() == Code::Unavailable
        || st.code() == Code::DeadlineExceeded
        || graph_proto::error::is_transport_loss(st.code(), st.message())
    {
        tracing::debug!(error = %st, "forward to the leader failed");
        return no_leader("the forward to the leader failed");
    }
    st
}

impl Forwarder {
    pub fn new(me: NodeId, faults: Option<FaultPlan>) -> Self {
        Self {
            me,
            faults,
            channels: Mutex::new(HashMap::new()),
            forwarded: AtomicU64::new(0),
            default_timeout: None,
        }
    }

    /// Test hook: every forward's default deadline is this instead.
    pub fn with_default_timeout(mut self, d: Option<Duration>) -> Self {
        self.default_timeout = d;
        self
    }

    /// The deadline of a forward of a request with metadata `md`: the
    /// client's own (`grpc-timeout`) when it sent one, else `default`.
    pub fn deadline(&self, md: &tonic::metadata::MetadataMap, default: Duration) -> Duration {
        incoming_timeout(md).unwrap_or(self.default_timeout.unwrap_or(default))
    }

    /// A forwarded request carrying `msg` with `deadline` as its
    /// `grpc-timeout`.
    pub fn request<T>(msg: T, deadline: Duration) -> Request<T> {
        let mut r = Request::new(msg);
        r.set_timeout(deadline);
        r
    }

    /// This node's id (`memory_graph.forwarded_by` on a `forward` span).
    pub fn me(&self) -> NodeId {
        self.me
    }

    /// Requests this node forwarded to a leader so far.
    pub fn forwarded_total(&self) -> u64 {
        self.forwarded.load(Ordering::Relaxed)
    }

    /// Count one forwarded request.
    pub fn count(&self) {
        self.forwarded.fetch_add(1, Ordering::Relaxed);
    }

    /// Where `req` goes: [`Route::Local`] when this node leads (or `req`
    /// was forwarded to it), the leader otherwise; `NoLeader` when none is
    /// known or (tests) the fault plan cuts the link to it.
    pub fn route<T>(&self, raft: &RaftNode, req: &Request<T>) -> Result<Route, Status> {
        if was_forwarded(req) {
            return Ok(Route::Local);
        }
        let leader = raft.leader();
        match (leader.id, leader.addr) {
            (Some(id), _) if id == self.me => Ok(Route::Local),
            (Some(id), Some(addr)) => {
                if let Some(plan) = &self.faults {
                    if !plan.connected(self.me, id) {
                        return Err(no_leader("testing: the fault plan cuts the leader off"));
                    }
                }
                Ok(Route::Leader { id, addr })
            }
            _ => {
                // A node that does not know the leader but leads (a single
                // voter right after start) is caught by `id == me` above;
                // everything else waits for an election.
                Err(no_leader("no leader is known"))
            }
        }
    }

    fn channel(&self, addr: &str) -> Result<Channel, Status> {
        let mut map = self
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(ch) = map.get(addr) {
            return Ok(ch.clone());
        }
        // Only the node forwarded to last (the leader, or a transfer
        // target) is kept: a cluster that changes leaders or addresses over
        // months does not accumulate channels.
        map.clear();
        let uri = if addr.contains("://") {
            addr.to_string()
        } else {
            format!("http://{addr}")
        };
        let ch = Endpoint::from_shared(uri)
            .map_err(|e| Status::internal(format!("bad leader address `{addr}`: {e}")))?
            .tcp_nodelay(true)
            .connect_timeout(CONNECT_TIMEOUT)
            .connect_lazy();
        map.insert(addr.to_string(), ch.clone());
        Ok(ch)
    }

    fn intercepted(&self, addr: &str) -> Result<ForwardChannel, Status> {
        Ok(InterceptedService::new(
            self.channel(addr)?,
            ForwardHeaders { me: self.me },
        ))
    }

    /// A `Write` client of the leader at `addr`.
    pub fn write_client(&self, addr: &str) -> Result<WriteClient<ForwardChannel>, Status> {
        Ok(WriteClient::new(self.intercepted(addr)?)
            .max_decoding_message_size(NO_LIMIT)
            .max_encoding_message_size(NO_LIMIT))
    }

    /// A `Store` client of the leader at `addr` (`ExtractorGaps`, #165).
    pub fn store_client(&self, addr: &str) -> Result<StoreClient<ForwardChannel>, Status> {
        Ok(StoreClient::new(self.intercepted(addr)?)
            .max_decoding_message_size(NO_LIMIT)
            .max_encoding_message_size(NO_LIMIT))
    }

    /// An `Admin` client of the node at `addr` (the leader, or a transfer
    /// target).
    pub fn admin_client(&self, addr: &str) -> Result<AdminClient<ForwardChannel>, Status> {
        Ok(AdminClient::new(self.intercepted(addr)?)
            .max_decoding_message_size(NO_LIMIT)
            .max_encoding_message_size(NO_LIMIT))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_leader_answer_passes_through_and_a_lost_link_is_no_leader() {
        let typed = store_error_to_status(&StoreError::Rejected("no".into()));
        let back = forward_error(typed.clone());
        assert_eq!(back.code(), typed.code());
        assert_eq!(back.details(), typed.details());
        for st in [
            Status::unavailable("tcp connect error"),
            Status::unknown("transport error"),
            Status::cancelled("gone"),
            Status::deadline_exceeded("slow leader"),
        ] {
            let e = graph_proto::status_to_store_error(&forward_error(st));
            assert!(matches!(e, StoreError::NoLeader { .. }), "{e:?}");
        }
        // A bare status from the leader that is not a transport failure.
        assert_eq!(
            forward_error(Status::invalid_argument("x")).code(),
            Code::InvalidArgument
        );
    }

    #[test]
    fn the_clients_deadline_is_propagated_else_a_bounded_default() {
        let md = |v: &str| {
            let mut m = tonic::metadata::MetadataMap::new();
            m.insert("grpc-timeout", v.parse().unwrap());
            m
        };
        assert_eq!(
            incoming_timeout(&md("1500m")),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(incoming_timeout(&md("3S")), Some(Duration::from_secs(3)));
        assert_eq!(incoming_timeout(&md("2M")), Some(Duration::from_secs(120)));
        assert_eq!(incoming_timeout(&md("1H")), Some(Duration::from_secs(3600)));
        assert_eq!(incoming_timeout(&md("7u")), Some(Duration::from_micros(7)));
        assert_eq!(incoming_timeout(&md("9n")), Some(Duration::from_nanos(9)));
        for bad in ["", "S", "12", "1x", "123456789S", "-1S"] {
            let m = if bad.is_empty() {
                tonic::metadata::MetadataMap::new()
            } else {
                md(bad)
            };
            assert_eq!(incoming_timeout(&m), None, "{bad:?}");
        }
        let f = Forwarder::new(1, None);
        let none = tonic::metadata::MetadataMap::new();
        assert_eq!(
            f.deadline(&none, FORWARD_UNARY_TIMEOUT),
            FORWARD_UNARY_TIMEOUT
        );
        assert_eq!(
            f.deadline(&md("250m"), FORWARD_UNARY_TIMEOUT),
            Duration::from_millis(250)
        );
        let f = f.with_default_timeout(Some(Duration::from_millis(5)));
        assert_eq!(
            f.deadline(&none, FORWARD_UNARY_TIMEOUT),
            Duration::from_millis(5)
        );
        let r = Forwarder::request((), Duration::from_millis(1500));
        assert_eq!(r.metadata().get("grpc-timeout").unwrap(), "1500000u");
    }

    #[test]
    fn the_channel_cache_keeps_only_the_last_target() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _g = rt.enter();
        let f = Forwarder::new(1, None);
        f.channel("127.0.0.1:1").unwrap();
        f.channel("127.0.0.1:2").unwrap();
        f.channel("127.0.0.1:2").unwrap();
        let keys: Vec<String> = f.channels.lock().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["127.0.0.1:2"]);
    }

    #[tokio::test]
    async fn a_forward_past_its_deadline_is_deadline_exceeded() {
        let st = within(
            1,
            Duration::from_millis(10),
            std::future::pending::<Result<(), Status>>(),
        )
        .await
        .unwrap_err();
        assert_eq!(st.code(), Code::DeadlineExceeded);
        assert!(
            within(1, Duration::from_secs(1), async { Ok::<_, Status>(1) })
                .await
                .is_ok()
        );
    }

    #[test]
    fn forwarded_requests_are_marked() {
        let req = ForwardHeaders { me: 3 }.call(Request::new(())).unwrap();
        assert!(was_forwarded(&req));
        assert_eq!(
            req.metadata().get(FORWARDED_BY_HEADER).unwrap(),
            "3",
            "names the forwarding node"
        );
        assert!(req.metadata().get(PROTOCOL_VERSION_HEADER).is_some());
        assert!(!was_forwarded(&Request::new(())));
    }
}
