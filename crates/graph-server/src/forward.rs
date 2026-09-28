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

/// Stamps the protocol version and [`FORWARDED_BY_HEADER`] on a forwarded
/// request.
#[derive(Debug, Clone, Copy)]
pub struct ForwardHeaders {
    pub me: NodeId,
}

impl Interceptor for ForwardHeaders {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        let md = req.metadata_mut();
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
pub fn forward_error(st: Status) -> Status {
    if !st.details().is_empty() {
        return st;
    }
    if st.code() == Code::Unavailable
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
        }
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
