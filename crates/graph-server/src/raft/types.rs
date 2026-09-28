//! The `RaftTypeConfig` and the application types it names.
use graph_proto::error::{wire_view, WireError};
use graph_proto::pb;
use graph_store::{IngestStats, StoreError, VacuumStats};
use openraft::impls::BasicNode;
use prost::Message;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub type NodeId = u64;

/// One replicated entry: a prost-encoded `pb::LogCommand` (ADR 0004 D5).
/// Kept as bytes rather than the decoded message so the log store writes
/// exactly what was proposed; the state machine decodes on apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRequest {
    pub command: Vec<u8>,
}

impl LogRequest {
    pub fn new(cmd: &pb::LogCommand) -> Self {
        Self {
            command: cmd.encode_to_vec(),
        }
    }

    pub fn decode(&self) -> Result<pb::LogCommand, StoreError> {
        pb::LogCommand::decode(self.command.as_slice())
            .map_err(|e| StoreError::Protocol(format!("undecodable log entry: {e}")))
    }
}

/// A `StoreError` in its wire form (prost `StoreErrorDetail` bytes), so a
/// response can carry the typed error through openraft's `serde` bound
/// without `StoreError` itself being serde.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrDetail(pub Vec<u8>);

impl ErrDetail {
    pub fn of(e: &StoreError) -> Self {
        ErrDetail(wire_view(e).detail().encode_to_vec())
    }

    pub fn into_error(self) -> StoreError {
        match pb::StoreErrorDetail::decode(self.0.as_slice()) {
            Ok(d) => match WireError::try_from(d) {
                Ok(w) => w.into(),
                Err(e) => StoreError::Protocol(format!("undecodable error detail: {e}")),
            },
            Err(e) => StoreError::Protocol(format!("undecodable error detail: {e}")),
        }
    }
}

/// `IngestStats` is serde; `VacuumStats` is not, so it gets a mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VacuumOutcome {
    pub terms_removed: usize,
    pub terms_kept: usize,
}

impl From<VacuumStats> for VacuumOutcome {
    fn from(s: VacuumStats) -> Self {
        Self {
            terms_removed: s.terms_removed,
            terms_kept: s.terms_kept,
        }
    }
}

impl From<VacuumOutcome> for VacuumStats {
    fn from(s: VacuumOutcome) -> Self {
        Self {
            terms_removed: s.terms_removed,
            terms_kept: s.terms_kept,
        }
    }
}

/// What applying one entry produced (one per entry, ADR 0004 D5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogResponse {
    /// The entry's index was at or below the store's marker: already
    /// applied (a restart replay), nothing done.
    Skipped,
    /// A blank, membership or `Noop` entry: only the marker moved.
    Marked,
    /// `IndexChunk`: one outcome per file, in input order.
    Index(Vec<Result<IngestStats, ErrDetail>>),
    /// `IngestExtraction`.
    Ingest(IngestStats),
    /// `Prune`: the removed paths.
    Prune(Vec<String>),
    Vacuum(VacuumOutcome),
    /// The entry's write was refused as a whole by the store (a validation
    /// error such as a NUL in a language: deterministic, so every replica
    /// refuses it the same way) and the marker did not move.
    Failed(ErrDetail),
}

/// A snapshot as a file (ADR 0004 D7): the `SnapshotData` under
/// `generic-snapshot-data`. Built by `export_snapshot`, installed by
/// `install_snapshot`; stage B streams it in chunks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub path: PathBuf,
}

openraft::declare_raft_types!(
    /// The memory-graph Raft: `u64` node ids, `BasicNode { addr }` node
    /// metadata, the tokio runtime, and the types above.
    pub TypeConfig:
        D = LogRequest,
        R = LogResponse,
        NodeId = NodeId,
        Node = BasicNode,
        SnapshotData = SnapshotFile,
);

pub type LogId = openraft::LogId<NodeId>;
pub type Entry = openraft::Entry<TypeConfig>;
pub type StoredMembership = openraft::StoredMembership<NodeId, BasicNode>;
pub type SnapshotMeta = openraft::SnapshotMeta<NodeId, BasicNode>;
pub type StorageError = openraft::StorageError<NodeId>;
pub type StorageIOError = openraft::StorageIOError<NodeId>;

/// The store's marker for a log id and back (ADR 0004 D7: `term`, `index`,
/// `node_id` of the entry's leader).
pub fn marker_of(log_id: &LogId) -> graph_store::RaftMarker {
    graph_store::RaftMarker {
        term: log_id.leader_id.term,
        index: log_id.index,
        node_id: log_id.leader_id.node_id,
    }
}

pub fn log_id_of(m: graph_store::RaftMarker) -> LogId {
    LogId::new(openraft::CommittedLeaderId::new(m.term, m.node_id), m.index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_and_log_id_round_trip() {
        let id = LogId::new(openraft::CommittedLeaderId::new(3, 7), 42);
        let m = marker_of(&id);
        assert_eq!((m.term, m.index, m.node_id), (3, 42, 7));
        assert_eq!(log_id_of(m), id);
    }

    #[test]
    fn err_detail_round_trips_the_variant() {
        let e = StoreError::InvalidSpan("bad".into());
        let back = ErrDetail::of(&e).into_error();
        assert!(
            matches!(back, StoreError::InvalidSpan(ref m) if m == "bad"),
            "{back:?}"
        );
        let e = StoreError::NotLeader {
            leader_id: Some(2),
            leader_addr: None,
        };
        assert!(matches!(
            ErrDetail::of(&e).into_error(),
            StoreError::NotLeader {
                leader_id: Some(2),
                leader_addr: None
            }
        ));
        assert!(matches!(
            ErrDetail(vec![0xff, 0xff]).into_error(),
            StoreError::Protocol(_)
        ));
    }

    #[test]
    fn log_request_round_trips_a_command() {
        let cmd = pb::LogCommand {
            cmd: Some(pb::log_command::Cmd::Vacuum(pb::log_command::Vacuum {})),
        };
        assert_eq!(LogRequest::new(&cmd).decode().unwrap(), cmd);
        assert!(LogRequest {
            command: vec![0x0a, 0xff]
        }
        .decode()
        .is_err());
    }
}
