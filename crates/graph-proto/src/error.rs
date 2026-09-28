//! `StoreError` <-> `tonic::Status` (ADR 0004 D1).
//!
//! Outbound, the gRPC code comes from the table below and the typed variant
//! rides prost-encoded (`pb::StoreErrorDetail`) in `Status::details`, with
//! the error's `Display` text as the status message. Inbound, the detail is
//! decoded when present (so the exact variant and fields come back); a
//! status without one (a proxy, a tonic transport error, a foreign server)
//! is mapped by code with its message.
//!
//! | `StoreError`                                                | gRPC code             |
//! |-------------------------------------------------------------|-----------------------|
//! | `Rejected`, `NotUtf8`, `TooLarge`, `InvalidSpan`, `Schema`  | `INVALID_ARGUMENT`    |
//! | `Locked`, not-leader, no-leader                             | `UNAVAILABLE`         |
//! | `SchemaMismatch`, `LegacyFormat`, `SnapshotExpired`, protocol | `FAILED_PRECONDITION` |
//! | `Corrupt`                                                   | `DATA_LOSS`           |
//! | `Storage` (disk full)                                       | `RESOURCE_EXHAUSTED`  |
//! | `Storage` (other), `OpenFailed`                             | `INTERNAL`            |
//!
//! `NotLeader`, `NoLeader` and `Protocol` are the three cluster-level errors
//! ADR 0004 adds. Until stage A package 1 (which adds them to `StoreError`)
//! merges, they live on [`WireError`] here, and folding a `WireError` into a
//! `StoreError` tags them onto `StoreError::Storage` (see
//! [`protocol_error`]).
// TODO(stage-a-merge): once `StoreError::{NotLeader, NoLeader, Protocol}`
// exist, make `WireError` an alias for (or thin wrapper over) `StoreError`:
// the three `WireError` variants become the `StoreError` ones, and
// `From<WireError> for StoreError` / `protocol_error` stop tagging.
use crate::pb::{self, store_error_detail as d};
use graph_core::{NodeKind, SchemaError};
use graph_store::StoreError;
use prost::Message;
use tonic::{Code, Status};

/// Prefix of the message a folded cluster-level error carries on
/// `StoreError::Storage` until the merge (see the module doc).
pub const FOLDED_PREFIX: &str = "[wire] ";

/// A `StoreError::Protocol(msg)` stand-in (see the module doc).
pub fn protocol_error(msg: String) -> StoreError {
    // TODO(stage-a-merge): `StoreError::Protocol(msg)`.
    StoreError::Storage(format!("{FOLDED_PREFIX}protocol: {msg}"))
}

/// Every error a memory-graph server can answer with: a `StoreError`, or
/// one of the cluster-level errors of ADR 0004.
#[derive(Debug)]
pub enum WireError {
    Store(StoreError),
    /// This node is not the leader; the known leader, when there is one.
    NotLeader {
        leader_id: Option<u64>,
        leader_addr: Option<String>,
    },
    /// No leader is known; retry after this long.
    NoLeader {
        retry_after_ms: u64,
    },
    /// Protocol or version mismatch, or a malformed message.
    Protocol(String),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Store(e) => write!(f, "{e}"),
            WireError::NotLeader {
                leader_id,
                leader_addr,
            } => match (leader_id, leader_addr) {
                (Some(id), Some(addr)) => {
                    write!(f, "not the leader: leader is node {id} at {addr}")
                }
                (Some(id), None) => write!(f, "not the leader: leader is node {id}"),
                (None, Some(addr)) => write!(f, "not the leader: leader is at {addr}"),
                (None, None) => write!(f, "not the leader"),
            },
            WireError::NoLeader { retry_after_ms } => {
                write!(f, "no leader known; retry after {retry_after_ms} ms")
            }
            WireError::Protocol(msg) => write!(f, "protocol error: {msg}"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<StoreError> for WireError {
    fn from(e: StoreError) -> Self {
        WireError::Store(e)
    }
}

impl From<WireError> for StoreError {
    fn from(e: WireError) -> Self {
        match e {
            WireError::Store(e) => e,
            // TODO(stage-a-merge): the three real variants.
            WireError::NotLeader { .. } | WireError::NoLeader { .. } => {
                StoreError::Storage(format!("{FOLDED_PREFIX}{e}"))
            }
            WireError::Protocol(msg) => protocol_error(msg),
        }
    }
}

/// Whether a storage error message describes a full disk (Linux/macOS
/// ENOSPC 28, Windows ERROR_DISK_FULL 112, or the words themselves).
pub fn is_disk_full(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("no space left")
        || m.contains("disk full")
        || m.contains("os error 28)")
        || m.contains("os error 112)")
        || m.ends_with("os error 28")
        || m.ends_with("os error 112")
}

impl WireError {
    /// The gRPC status code for this error (the table in the module doc).
    pub fn code(&self) -> Code {
        match self {
            WireError::Store(e) => match e {
                StoreError::Rejected(_)
                | StoreError::NotUtf8(_)
                | StoreError::TooLarge(_)
                | StoreError::InvalidSpan(_)
                | StoreError::Schema(_) => Code::InvalidArgument,
                StoreError::Locked(_) => Code::Unavailable,
                StoreError::SchemaMismatch { .. }
                | StoreError::LegacyFormat { .. }
                | StoreError::SnapshotExpired { .. } => Code::FailedPrecondition,
                StoreError::Corrupt(_) => Code::DataLoss,
                StoreError::Storage(msg) => {
                    if is_disk_full(msg) {
                        Code::ResourceExhausted
                    } else {
                        Code::Internal
                    }
                }
                StoreError::OpenFailed { .. } => Code::Internal,
            },
            WireError::NotLeader { .. } | WireError::NoLeader { .. } => Code::Unavailable,
            WireError::Protocol(_) => Code::FailedPrecondition,
        }
    }

    /// The typed detail carried in `Status::details`.
    pub fn detail(&self) -> pb::StoreErrorDetail {
        use d::Kind as K;
        let kind = match self {
            WireError::Store(e) => match e {
                StoreError::Locked(msg) => K::Locked(d::Locked { msg: msg.clone() }),
                StoreError::SchemaMismatch { found } => {
                    K::SchemaMismatch(d::SchemaMismatch { found: *found })
                }
                StoreError::LegacyFormat { path, version } => K::LegacyFormat(d::LegacyFormat {
                    path: path.clone(),
                    version: *version,
                }),
                StoreError::OpenFailed { path, reason } => K::OpenFailed(d::OpenFailed {
                    path: path.clone(),
                    reason: reason.clone(),
                }),
                StoreError::Rejected(msg) => K::Rejected(d::Rejected { msg: msg.clone() }),
                StoreError::NotUtf8(path) => K::NotUtf8(d::NotUtf8 { path: path.clone() }),
                StoreError::TooLarge(path) => K::TooLarge(d::TooLarge { path: path.clone() }),
                StoreError::InvalidSpan(msg) => K::InvalidSpan(d::InvalidSpan { msg: msg.clone() }),
                StoreError::Corrupt(msg) => K::Corrupt(d::Corrupt { msg: msg.clone() }),
                StoreError::Schema(SchemaError::InvalidContainment(parent, child)) => {
                    K::Schema(d::Schema {
                        msg: e.to_string(),
                        parent: pb::NodeKind::from(*parent) as i32,
                        child: pb::NodeKind::from(*child) as i32,
                    })
                }
                StoreError::Storage(msg) => K::Storage(d::Storage { msg: msg.clone() }),
                StoreError::SnapshotExpired {
                    age_secs,
                    max_age_secs,
                } => K::SnapshotExpired(d::SnapshotExpired {
                    age_secs: *age_secs,
                    max_age_secs: *max_age_secs,
                }),
            },
            WireError::NotLeader {
                leader_id,
                leader_addr,
            } => K::NotLeader(d::NotLeader {
                leader_id: *leader_id,
                leader_addr: leader_addr.clone(),
            }),
            WireError::NoLeader { retry_after_ms } => K::NoLeader(d::NoLeader {
                retry_after_ms: *retry_after_ms,
            }),
            WireError::Protocol(msg) => K::Protocol(d::Protocol { msg: msg.clone() }),
        };
        pb::StoreErrorDetail { kind: Some(kind) }
    }

    /// Map a status without a usable detail by its code alone.
    pub fn from_code(code: Code, message: &str) -> Self {
        let msg = message.to_string();
        match code {
            Code::InvalidArgument => WireError::Store(StoreError::Rejected(msg)),
            Code::Unavailable => WireError::Store(StoreError::Locked(msg)),
            Code::FailedPrecondition => WireError::Protocol(msg),
            Code::DataLoss => WireError::Store(StoreError::Corrupt(msg)),
            Code::ResourceExhausted | Code::Internal => WireError::Store(StoreError::Storage(msg)),
            other => WireError::Protocol(format!("{other:?}: {msg}")),
        }
    }
}

impl TryFrom<pb::StoreErrorDetail> for WireError {
    type Error = crate::ConvertError;
    fn try_from(detail: pb::StoreErrorDetail) -> Result<Self, crate::ConvertError> {
        use d::Kind as K;
        let kind = detail
            .kind
            .ok_or_else(|| crate::ConvertError("StoreErrorDetail.kind is missing".into()))?;
        Ok(match kind {
            K::Locked(x) => StoreError::Locked(x.msg).into(),
            K::SchemaMismatch(x) => StoreError::SchemaMismatch { found: x.found }.into(),
            K::LegacyFormat(x) => StoreError::LegacyFormat {
                path: x.path,
                version: x.version,
            }
            .into(),
            K::OpenFailed(x) => StoreError::OpenFailed {
                path: x.path,
                reason: x.reason,
            }
            .into(),
            K::Rejected(x) => StoreError::Rejected(x.msg).into(),
            K::NotUtf8(x) => StoreError::NotUtf8(x.path).into(),
            K::TooLarge(x) => StoreError::TooLarge(x.path).into(),
            K::InvalidSpan(x) => StoreError::InvalidSpan(x.msg).into(),
            K::Corrupt(x) => StoreError::Corrupt(x.msg).into(),
            K::Schema(x) => {
                let parent = crate::convert::Wire::<NodeKind>::try_from(x.parent);
                let child = crate::convert::Wire::<NodeKind>::try_from(x.child);
                match (parent, child) {
                    (Ok(p), Ok(c)) => {
                        StoreError::Schema(SchemaError::InvalidContainment(p.0, c.0)).into()
                    }
                    // A peer that knows a containment rule this build does
                    // not: keep the text, in the same code class.
                    _ => StoreError::Rejected(x.msg).into(),
                }
            }
            K::Storage(x) => StoreError::Storage(x.msg).into(),
            K::SnapshotExpired(x) => StoreError::SnapshotExpired {
                age_secs: x.age_secs,
                max_age_secs: x.max_age_secs,
            }
            .into(),
            K::NotLeader(x) => WireError::NotLeader {
                leader_id: x.leader_id,
                leader_addr: x.leader_addr,
            },
            K::NoLeader(x) => WireError::NoLeader {
                retry_after_ms: x.retry_after_ms,
            },
            K::Protocol(x) => WireError::Protocol(x.msg),
        })
    }
}

impl From<WireError> for pb::StoreErrorDetail {
    fn from(e: WireError) -> Self {
        e.detail()
    }
}

impl From<&WireError> for Status {
    fn from(e: &WireError) -> Self {
        Status::with_details(e.code(), e.to_string(), e.detail().encode_to_vec().into())
    }
}

impl From<WireError> for Status {
    fn from(e: WireError) -> Self {
        Status::from(&e)
    }
}

impl From<Status> for WireError {
    fn from(s: Status) -> Self {
        WireError::from(&s)
    }
}

impl From<&Status> for WireError {
    fn from(s: &Status) -> Self {
        let details = s.details();
        if !details.is_empty() {
            if let Ok(w) = pb::StoreErrorDetail::decode(details)
                .map_err(|_| ())
                .and_then(|d| WireError::try_from(d).map_err(|_| ()))
            {
                return w;
            }
        }
        WireError::from_code(s.code(), s.message())
    }
}

/// `StoreError -> Status`: the code table and the typed detail. (A `From`
/// impl is impossible: both types are foreign to this crate.)
pub fn store_error_to_status(e: &StoreError) -> Status {
    Status::from(&wire_view(e))
}

/// `Status -> StoreError`: the typed detail when present, else by code.
/// Cluster-level errors fold onto `Storage` until the merge (module doc).
pub fn status_to_store_error(s: &Status) -> StoreError {
    WireError::from(s).into()
}

/// A `WireError::Store` over a borrowed `StoreError` (which is not
/// `Clone`): re-creates the variant field by field.
pub fn wire_view(e: &StoreError) -> WireError {
    WireError::Store(match e {
        StoreError::Locked(m) => StoreError::Locked(m.clone()),
        StoreError::SchemaMismatch { found } => StoreError::SchemaMismatch { found: *found },
        StoreError::LegacyFormat { path, version } => StoreError::LegacyFormat {
            path: path.clone(),
            version: *version,
        },
        StoreError::OpenFailed { path, reason } => StoreError::OpenFailed {
            path: path.clone(),
            reason: reason.clone(),
        },
        StoreError::Rejected(m) => StoreError::Rejected(m.clone()),
        StoreError::NotUtf8(p) => StoreError::NotUtf8(p.clone()),
        StoreError::TooLarge(p) => StoreError::TooLarge(p.clone()),
        StoreError::InvalidSpan(m) => StoreError::InvalidSpan(m.clone()),
        StoreError::Corrupt(m) => StoreError::Corrupt(m.clone()),
        StoreError::Schema(SchemaError::InvalidContainment(p, c)) => {
            StoreError::Schema(SchemaError::InvalidContainment(*p, *c))
        }
        StoreError::Storage(m) => StoreError::Storage(m.clone()),
        StoreError::SnapshotExpired {
            age_secs,
            max_age_secs,
        } => StoreError::SnapshotExpired {
            age_secs: *age_secs,
            max_age_secs: *max_age_secs,
        },
    })
}
