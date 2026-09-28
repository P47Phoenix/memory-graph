//! openraft RPC types <-> the `memory_graph.v1` Raft messages (ADR 0004
//! D5). Entries travel in the log store's binary framing
//! ([`encode_entry`]/[`decode_entry`]); votes and log ids are small typed
//! fields; the membership inside a snapshot header is serde_json (a few
//! hundred bytes).
use super::log_store::{decode_entry, encode_entry};
use super::types::{LogId, NodeId, StoredMembership, TypeConfig};
use graph_proto::pb;
use openraft::raft::{AppendEntriesRequest, AppendEntriesResponse, VoteRequest, VoteResponse};
use openraft::{CommittedLeaderId, Vote};

/// A malformed Raft message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireErr(pub String);

impl std::fmt::Display for WireErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "malformed raft message: {}", self.0)
    }
}

impl std::error::Error for WireErr {}

pub fn vote_to_pb(v: &Vote<NodeId>) -> pb::RaftVote {
    pb::RaftVote {
        term: v.leader_id.term,
        node_id: v.leader_id.node_id,
        committed: v.committed,
    }
}

pub fn vote_from_pb(v: Option<pb::RaftVote>) -> Result<Vote<NodeId>, WireErr> {
    let v = v.ok_or_else(|| WireErr("missing vote".into()))?;
    Ok(if v.committed {
        Vote::new_committed(v.term, v.node_id)
    } else {
        Vote::new(v.term, v.node_id)
    })
}

pub fn log_id_to_pb(l: &LogId) -> pb::RaftLogId {
    pb::RaftLogId {
        term: l.leader_id.term,
        node_id: l.leader_id.node_id,
        index: l.index,
    }
}

pub fn log_id_from_pb(l: pb::RaftLogId) -> LogId {
    LogId::new(CommittedLeaderId::new(l.term, l.node_id), l.index)
}

pub fn append_to_pb(r: &AppendEntriesRequest<TypeConfig>) -> pb::AppendEntriesRequest {
    pb::AppendEntriesRequest {
        vote: Some(vote_to_pb(&r.vote)),
        prev_log_id: r.prev_log_id.as_ref().map(log_id_to_pb),
        leader_commit: r.leader_commit.as_ref().map(log_id_to_pb),
        entries: r.entries.iter().map(encode_entry).collect(),
    }
}

pub fn append_from_pb(
    r: pb::AppendEntriesRequest,
) -> Result<AppendEntriesRequest<TypeConfig>, WireErr> {
    Ok(AppendEntriesRequest {
        vote: vote_from_pb(r.vote)?,
        prev_log_id: r.prev_log_id.map(log_id_from_pb),
        leader_commit: r.leader_commit.map(log_id_from_pb),
        entries: r
            .entries
            .iter()
            .map(|b| decode_entry(b).map_err(|e| WireErr(e.to_string())))
            .collect::<Result<_, _>>()?,
    })
}

pub fn append_resp_to_pb(r: &AppendEntriesResponse<NodeId>) -> pb::AppendEntriesResponse {
    use pb::append_entries_response::{Partial, Result as R};
    let result = match r {
        AppendEntriesResponse::Success => R::Success(pb::RaftEmpty {}),
        AppendEntriesResponse::PartialSuccess(m) => R::PartialSuccess(Partial {
            matching: m.as_ref().map(log_id_to_pb),
        }),
        AppendEntriesResponse::Conflict => R::Conflict(pb::RaftEmpty {}),
        AppendEntriesResponse::HigherVote(v) => R::HigherVote(vote_to_pb(v)),
    };
    pb::AppendEntriesResponse {
        result: Some(result),
    }
}

pub fn append_resp_from_pb(
    r: pb::AppendEntriesResponse,
) -> Result<AppendEntriesResponse<NodeId>, WireErr> {
    use pb::append_entries_response::Result as R;
    Ok(
        match r
            .result
            .ok_or_else(|| WireErr("empty AppendEntriesResponse".into()))?
        {
            R::Success(_) => AppendEntriesResponse::Success,
            R::PartialSuccess(p) => {
                AppendEntriesResponse::PartialSuccess(p.matching.map(log_id_from_pb))
            }
            R::Conflict(_) => AppendEntriesResponse::Conflict,
            R::HigherVote(v) => AppendEntriesResponse::HigherVote(vote_from_pb(Some(v))?),
        },
    )
}

pub fn vote_req_to_pb(r: &VoteRequest<NodeId>) -> pb::VoteRequest {
    pb::VoteRequest {
        vote: Some(vote_to_pb(&r.vote)),
        last_log_id: r.last_log_id.as_ref().map(log_id_to_pb),
    }
}

pub fn vote_req_from_pb(r: pb::VoteRequest) -> Result<VoteRequest<NodeId>, WireErr> {
    Ok(VoteRequest {
        vote: vote_from_pb(r.vote)?,
        last_log_id: r.last_log_id.map(log_id_from_pb),
    })
}

pub fn vote_resp_to_pb(r: &VoteResponse<NodeId>) -> pb::VoteResponse {
    pb::VoteResponse {
        vote: Some(vote_to_pb(&r.vote)),
        vote_granted: r.vote_granted,
        last_log_id: r.last_log_id.as_ref().map(log_id_to_pb),
    }
}

pub fn vote_resp_from_pb(r: pb::VoteResponse) -> Result<VoteResponse<NodeId>, WireErr> {
    Ok(VoteResponse {
        vote: vote_from_pb(r.vote)?,
        vote_granted: r.vote_granted,
        last_log_id: r.last_log_id.map(log_id_from_pb),
    })
}

pub fn membership_to_json(m: &StoredMembership) -> Vec<u8> {
    serde_json::to_vec(m).expect("membership serializes")
}

pub fn membership_from_json(b: &[u8]) -> Result<StoredMembership, WireErr> {
    serde_json::from_slice(b).map_err(|e| WireErr(format!("snapshot membership: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::types::{Entry, LogRequest};
    use openraft::EntryPayload;

    fn lid(t: u64, n: u64, i: u64) -> LogId {
        LogId::new(CommittedLeaderId::new(t, n), i)
    }

    #[test]
    fn append_entries_round_trips_with_binary_entries() {
        let big = vec![7u8; 1 << 20];
        let req = AppendEntriesRequest::<TypeConfig> {
            vote: Vote::new_committed(3, 1),
            prev_log_id: Some(lid(2, 1, 9)),
            leader_commit: Some(lid(3, 1, 10)),
            entries: vec![
                Entry {
                    log_id: lid(3, 1, 10),
                    payload: EntryPayload::Normal(LogRequest {
                        command: big.clone(),
                    }),
                },
                Entry {
                    log_id: lid(3, 1, 11),
                    payload: EntryPayload::Blank,
                },
            ],
        };
        let p = append_to_pb(&req);
        // The 1 MiB payload travels as bytes plus the 25-byte framing.
        assert_eq!(p.entries[0].len(), big.len() + 25);
        let back = append_from_pb(p).unwrap();
        assert_eq!(back.vote, req.vote);
        assert_eq!(back.prev_log_id, req.prev_log_id);
        assert_eq!(back.leader_commit, req.leader_commit);
        assert_eq!(back.entries, req.entries);
        let empty = AppendEntriesRequest::<TypeConfig> {
            vote: Vote::new(1, 2),
            prev_log_id: None,
            leader_commit: None,
            entries: vec![],
        };
        let back = append_from_pb(append_to_pb(&empty)).unwrap();
        assert_eq!(back.vote, empty.vote);
        assert!(back.prev_log_id.is_none() && back.leader_commit.is_none());
    }

    #[test]
    fn responses_and_votes_round_trip() {
        for r in [
            AppendEntriesResponse::Success,
            AppendEntriesResponse::PartialSuccess(None),
            AppendEntriesResponse::PartialSuccess(Some(lid(1, 2, 3))),
            AppendEntriesResponse::Conflict,
            AppendEntriesResponse::HigherVote(Vote::new(5, 3)),
        ] {
            assert_eq!(append_resp_from_pb(append_resp_to_pb(&r)).unwrap(), r);
        }
        let v = VoteRequest {
            vote: Vote::new(4, 2),
            last_log_id: Some(lid(3, 1, 7)),
        };
        assert_eq!(vote_req_from_pb(vote_req_to_pb(&v)).unwrap(), v);
        let r = VoteResponse {
            vote: Vote::new_committed(4, 2),
            vote_granted: true,
            last_log_id: None,
        };
        assert_eq!(vote_resp_from_pb(vote_resp_to_pb(&r)).unwrap(), r);
        assert!(vote_from_pb(None).is_err());
        assert!(append_resp_from_pb(pb::AppendEntriesResponse { result: None }).is_err());
        let bad = pb::AppendEntriesRequest {
            vote: Some(vote_to_pb(&Vote::new(1, 1))),
            prev_log_id: None,
            leader_commit: None,
            entries: vec![vec![1, 2, 3]],
        };
        assert!(append_from_pb(bad).is_err());
    }
}
