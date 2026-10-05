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
//! | `Rejected`, `NotUtf8`, `StrictEncoding`, `Binary`, `TooLarge`, `InvalidSpan`, `Schema`, `AlreadyApplied` | `INVALID_ARGUMENT` |
//! | `Locked`, not-leader, no-leader                             | `UNAVAILABLE`         |
//! | `SchemaMismatch`, `LegacyFormat`, `SnapshotExpired`, `WrongCluster`, protocol | `FAILED_PRECONDITION` |
//! | `Corrupt`                                                   | `DATA_LOSS`           |
//! | `Storage` (disk full)                                       | `RESOURCE_EXHAUSTED`  |
//! | `Storage` (other), `OpenFailed`                             | `INTERNAL`            |
//!
//! `NotLeader`, `NoLeader` and `Protocol` are the three cluster-level errors
//! ADR 0004 adds; they are real `StoreError` variants. [`WireError`] keeps
//! them as its own variants so a server can build one without a `StoreError`
//! in hand, and `WireError <-> StoreError` maps them one to one in both
//! directions (a `WireError::Store` never wraps one of the three).
//!
//! A refusal may also be marked *transient* on the wire
//! ([`transient_rejection`], issue #225): still `Rejected` on both sides,
//! but a client retries it until its write deadline.
use crate::pb::{self, store_error_detail as d};
use graph_core::{NodeKind, SchemaError};
use graph_store::StoreError;
use prost::Message;
use tonic::{Code, Status};

/// `StoreError::Protocol(msg)`.
pub fn protocol_error(msg: String) -> StoreError {
    StoreError::Protocol(msg)
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
            // The same text as the `StoreError` variant, so a message is
            // identical whichever side rendered it.
            WireError::NotLeader {
                leader_id,
                leader_addr,
            } => write!(
                f,
                "{}",
                StoreError::NotLeader {
                    leader_id: *leader_id,
                    leader_addr: leader_addr.clone(),
                }
            ),
            WireError::NoLeader { retry_after_ms } => write!(
                f,
                "{}",
                StoreError::NoLeader {
                    retry_after_ms: *retry_after_ms
                }
            ),
            WireError::Protocol(msg) => write!(f, "{}", StoreError::Protocol(msg.clone())),
        }
    }
}

impl std::error::Error for WireError {}

impl From<StoreError> for WireError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotLeader {
                leader_id,
                leader_addr,
            } => WireError::NotLeader {
                leader_id,
                leader_addr,
            },
            StoreError::NoLeader { retry_after_ms } => WireError::NoLeader { retry_after_ms },
            StoreError::Protocol(msg) => WireError::Protocol(msg),
            e => WireError::Store(e),
        }
    }
}

impl From<WireError> for StoreError {
    fn from(e: WireError) -> Self {
        match e {
            WireError::Store(e) => e,
            WireError::NotLeader {
                leader_id,
                leader_addr,
            } => StoreError::NotLeader {
                leader_id,
                leader_addr,
            },
            WireError::NoLeader { retry_after_ms } => StoreError::NoLeader { retry_after_ms },
            WireError::Protocol(msg) => StoreError::Protocol(msg),
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

/// Whether a status without a typed detail means the connection under a
/// call was lost (the server died, a reset, a client-side timeout): the
/// call may or may not have reached the server.
///
/// `CANCELLED`, `DEADLINE_EXCEEDED` and `ABORTED` always count. `UNKNOWN`
/// counts only when its message looks like a transport failure ("transport
/// error", "connection", "broken pipe", "reset", "closed", any case): tonic
/// also answers `UNKNOWN` for a server handler that panicked, and that must
/// surface as a storage error, not be retried as a lost connection.
pub fn is_transport_loss(code: Code, message: &str) -> bool {
    match code {
        Code::Cancelled | Code::DeadlineExceeded | Code::Aborted => true,
        Code::Unknown => {
            let m = message.to_ascii_lowercase();
            [
                "transport error",
                "connection",
                "broken pipe",
                "reset",
                "closed",
            ]
            .iter()
            .any(|w| m.contains(w))
        }
        _ => false,
    }
}

impl WireError {
    /// The gRPC status code for this error (the table in the module doc).
    pub fn code(&self) -> Code {
        match self {
            WireError::Store(e) => match e {
                StoreError::Rejected(_)
                | StoreError::NotUtf8(_)
                | StoreError::StrictEncoding { .. }
                | StoreError::Binary(_)
                | StoreError::TooLarge(_)
                | StoreError::InvalidSpan(_)
                | StoreError::Schema(_)
                | StoreError::AlreadyApplied { .. } => Code::InvalidArgument,
                StoreError::Locked(_) => Code::Unavailable,
                StoreError::SchemaMismatch { .. }
                | StoreError::LegacyFormat { .. }
                | StoreError::SnapshotExpired { .. }
                | StoreError::WrongCluster { .. } => Code::FailedPrecondition,
                StoreError::Corrupt(_) => Code::DataLoss,
                StoreError::Storage(msg) => {
                    if is_disk_full(msg) {
                        Code::ResourceExhausted
                    } else {
                        Code::Internal
                    }
                }
                StoreError::OpenFailed { .. } => Code::Internal,
                StoreError::NotLeader { .. } | StoreError::NoLeader { .. } => Code::Unavailable,
                StoreError::Protocol(_) => Code::FailedPrecondition,
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
                StoreError::Rejected(msg) => K::Rejected(d::Rejected {
                    msg: msg.clone(),
                    transient: false,
                }),
                // Server-internal (the state machine consumes it); should one
                // ever leak it travels as the refusal it is, in its class.
                StoreError::AlreadyApplied { .. } => K::Rejected(d::Rejected {
                    msg: e.to_string(),
                    transient: false,
                }),
                StoreError::NotUtf8(path) => K::NotUtf8(d::NotUtf8 { path: path.clone() }),
                StoreError::StrictEncoding { path, encoding } => {
                    K::StrictEncoding(d::StrictEncoding {
                        path: path.clone(),
                        encoding: encoding.clone(),
                    })
                }
                StoreError::Binary(path) => K::Binary(d::Binary { path: path.clone() }),
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
                StoreError::NotLeader {
                    leader_id,
                    leader_addr,
                } => K::NotLeader(d::NotLeader {
                    leader_id: *leader_id,
                    leader_addr: leader_addr.clone(),
                }),
                StoreError::NoLeader { retry_after_ms } => K::NoLeader(d::NoLeader {
                    retry_after_ms: *retry_after_ms,
                }),
                StoreError::Protocol(msg) => K::Protocol(d::Protocol { msg: msg.clone() }),
                StoreError::WrongCluster { expected, found } => K::WrongCluster(d::WrongCluster {
                    expected: expected.clone(),
                    found: found.clone(),
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
    ///
    /// `Protocol` is reserved for what really is a protocol mismatch (the
    /// typed detail, `Unimplemented` from a server without the method, and
    /// the client's own `Hello` version check): a bare `FAILED_PRECONDITION`
    /// (a proxy, a foreign server) is a refusal (`Rejected`), and a
    /// transport-level loss (`UNKNOWN` "transport error" and the like, `CANCELLED`,
    /// `DEADLINE_EXCEEDED`, `ABORTED`) is a `Storage` error naming the lost
    /// connection, never a protocol error.
    pub fn from_code(code: Code, message: &str) -> Self {
        let msg = message.to_string();
        match code {
            Code::InvalidArgument | Code::FailedPrecondition => {
                WireError::Store(StoreError::Rejected(msg))
            }
            Code::Unavailable => WireError::Store(StoreError::Locked(msg)),
            Code::DataLoss => WireError::Store(StoreError::Corrupt(msg)),
            Code::ResourceExhausted | Code::Internal => WireError::Store(StoreError::Storage(msg)),
            Code::Unimplemented => WireError::Protocol(format!("{code:?}: {msg}")),
            other if is_transport_loss(other, &msg) => WireError::Store(StoreError::Storage(
                format!("connection lost ({other:?}): {msg}"),
            )),
            other => WireError::Store(StoreError::Storage(format!("{other:?}: {msg}"))),
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
            K::StrictEncoding(x) => StoreError::StrictEncoding {
                path: x.path,
                encoding: x.encoding,
            }
            .into(),
            K::Binary(x) => StoreError::Binary(x.path).into(),
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
            K::WrongCluster(x) => StoreError::WrongCluster {
                expected: x.expected,
                found: x.found,
            }
            .into(),
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
pub fn status_to_store_error(s: &Status) -> StoreError {
    WireError::from(s).into()
}

/// A *transient* refusal (`StoreErrorDetail.Rejected.transient`, issue
/// #225): the same code (`INVALID_ARGUMENT`), message and `StoreError`
/// (`Rejected(msg)`) as any refusal, but marked as expected to clear on its
/// own shortly, so a client retries the request until its write deadline
/// ([`is_transient_rejection`]) and then reports the refusal unchanged. An
/// older client ignores the flag and fails at once, as before.
pub fn transient_rejection(msg: String) -> Status {
    let detail = pb::StoreErrorDetail {
        kind: Some(d::Kind::Rejected(d::Rejected {
            msg: msg.clone(),
            transient: true,
        })),
    };
    let display = StoreError::Rejected(msg).to_string();
    Status::with_details(
        Code::InvalidArgument,
        display,
        detail.encode_to_vec().into(),
    )
}

/// Whether `s` is a [`transient_rejection`] (its typed detail says so).
pub fn is_transient_rejection(s: &Status) -> bool {
    let details = s.details();
    !details.is_empty()
        && matches!(
            pb::StoreErrorDetail::decode(details),
            Ok(pb::StoreErrorDetail {
                kind: Some(d::Kind::Rejected(d::Rejected {
                    transient: true,
                    ..
                })),
            })
        )
}

/// A `WireError` over a borrowed `StoreError` (which is not `Clone`):
/// re-creates the variant field by field; the three cluster-level variants
/// become their `WireError` counterparts.
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
        StoreError::AlreadyApplied { index } => StoreError::AlreadyApplied { index: *index },
        StoreError::NotUtf8(p) => StoreError::NotUtf8(p.clone()),
        StoreError::StrictEncoding { path, encoding } => StoreError::StrictEncoding {
            path: path.clone(),
            encoding: encoding.clone(),
        },
        StoreError::Binary(p) => StoreError::Binary(p.clone()),
        StoreError::TooLarge(p) => StoreError::TooLarge(p.clone()),
        StoreError::InvalidSpan(m) => StoreError::InvalidSpan(m.clone()),
        StoreError::Corrupt(m) => StoreError::Corrupt(m.clone()),
        StoreError::Schema(SchemaError::InvalidContainment(p, c)) => {
            StoreError::Schema(SchemaError::InvalidContainment(*p, *c))
        }
        StoreError::Storage(m) => StoreError::Storage(m.clone()),
        StoreError::WrongCluster { expected, found } => StoreError::WrongCluster {
            expected: expected.clone(),
            found: found.clone(),
        },
        StoreError::SnapshotExpired {
            age_secs,
            max_age_secs,
        } => StoreError::SnapshotExpired {
            age_secs: *age_secs,
            max_age_secs: *max_age_secs,
        },
        StoreError::NotLeader {
            leader_id,
            leader_addr,
        } => {
            return WireError::NotLeader {
                leader_id: *leader_id,
                leader_addr: leader_addr.clone(),
            }
        }
        StoreError::NoLeader { retry_after_ms } => {
            return WireError::NoLeader {
                retry_after_ms: *retry_after_ms,
            }
        }
        StoreError::Protocol(m) => return WireError::Protocol(m.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_a_transport_loss_only_when_it_reads_like_one() {
        for m in [
            "transport error",
            "error trying to connect: Connection refused",
            "Broken pipe (os error 32)",
            "connection RESET by peer",
            "stream closed because of a broken pipe",
            "h2 protocol error: channel closed",
        ] {
            assert!(is_transport_loss(Code::Unknown, m), "{m}");
            let e: StoreError = WireError::from_code(Code::Unknown, m).into();
            assert!(
                matches!(&e, StoreError::Storage(s) if s.starts_with("connection lost")),
                "{e:?}"
            );
        }
        for c in [Code::Cancelled, Code::DeadlineExceeded, Code::Aborted] {
            assert!(is_transport_loss(c, ""));
        }
        assert!(!is_transport_loss(Code::Internal, "connection reset"));
    }

    /// Issue #225: a transient refusal is an ordinary refusal on the wire
    /// (code, message, `StoreError`), plus the flag; nothing else carries it.
    #[test]
    fn a_transient_rejection_is_a_flagged_refusal() {
        let st = transient_rejection("would drop below quorum".into());
        let plain = store_error_to_status(&StoreError::Rejected("would drop below quorum".into()));
        assert_eq!(st.code(), plain.code());
        assert_eq!(st.message(), plain.message());
        assert!(is_transient_rejection(&st));
        assert!(!is_transient_rejection(&plain));
        assert!(matches!(
            status_to_store_error(&st),
            StoreError::Rejected(ref m) if m == "would drop below quorum"
        ));
        // Other kinds, and statuses without a (decodable) detail, are not.
        for other in [
            store_error_to_status(&StoreError::NoLeader { retry_after_ms: 1 }),
            store_error_to_status(&StoreError::Storage("x".into())),
            Status::invalid_argument("would drop below quorum"),
            Status::with_details(Code::InvalidArgument, "x", vec![0xff, 0xff].into()),
        ] {
            assert!(!is_transient_rejection(&other), "{other:?}");
        }
    }

    #[test]
    fn a_handler_panic_is_a_storage_error_not_a_lost_connection() {
        let m = "panicked at crates/x.rs:1:1: index out of bounds";
        assert!(!is_transport_loss(Code::Unknown, m));
        assert!(!is_transport_loss(Code::Unknown, ""));
        let e: StoreError = WireError::from_code(Code::Unknown, m).into();
        match e {
            StoreError::Storage(s) => {
                assert!(!s.contains("connection lost"), "{s}");
                assert!(s.contains("panicked"), "{s}");
            }
            other => panic!("{other:?}"),
        }
    }
}
