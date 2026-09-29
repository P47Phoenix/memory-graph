//! [`ReadMeta`]: how fresh a read's answer is (ADR 0004 D8), carried on
//! every `Store` read response as the ASCII response header
//! [`READ_META_HEADER`] (`mg-read-meta`), so no read message changes shape.
//! [`READ_MODE_HEADER`] (`mg-read`) on `OpenSnapshot` asks for a
//! linearizable snapshot (the read barrier runs before the handle opens).
//!
//! Header value: `applied_index=<u64>;leader_committed_index=<u64|->;stale_possible=<true|false>`.
//! Unknown keys are ignored, so a later field is not a breaking change.

/// The response header that carries a [`ReadMeta`].
pub const READ_META_HEADER: &str = "mg-read-meta";

/// The request header on `OpenSnapshot` naming the read mode the handle is
/// opened under (`linearizable`; absent or anything else: local).
pub const READ_MODE_HEADER: &str = "mg-read";

/// The freshness of one read's answer, as the node that served it saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReadMeta {
    /// The Raft log index this node had applied when it answered.
    pub applied_index: u64,
    /// The leader's committed index as this node last saw it (on the
    /// leader: its own applied index; `None`: never heard from a leader).
    pub leader_committed_index: Option<u64>,
    /// The answer may miss acknowledged writes: no leader is known, the
    /// leader has not been heard from within an election timeout, or the
    /// applied index lags the leader's committed index as last seen.
    pub stale_possible: bool,
}

impl ReadMeta {
    /// The header value.
    pub fn to_header(&self) -> String {
        let lc = self
            .leader_committed_index
            .map_or_else(|| "-".to_string(), |i| i.to_string());
        format!(
            "applied_index={};leader_committed_index={lc};stale_possible={}",
            self.applied_index, self.stale_possible
        )
    }

    /// Parse a header value; `None` when a known key is malformed or
    /// `applied_index` / `stale_possible` is missing.
    pub fn from_header(v: &str) -> Option<ReadMeta> {
        let (mut applied, mut lc, mut stale) = (None, None, None);
        for part in v.split(';') {
            let (k, val) = part.split_once('=')?;
            match k.trim() {
                "applied_index" => applied = Some(val.trim().parse().ok()?),
                "leader_committed_index" => {
                    lc = match val.trim() {
                        "-" => Some(None),
                        n => Some(Some(n.parse().ok()?)),
                    }
                }
                "stale_possible" => stale = Some(val.trim().parse().ok()?),
                _ => {}
            }
        }
        Some(ReadMeta {
            applied_index: applied?,
            leader_committed_index: lc.flatten(),
            stale_possible: stale?,
        })
    }

    /// Read it from a response's metadata (`None` when absent or bad: a
    /// server of an earlier build).
    pub fn from_metadata(md: &tonic::metadata::MetadataMap) -> Option<ReadMeta> {
        md.get(READ_META_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(Self::from_header)
    }

    /// Put it on a response's metadata.
    pub fn insert_into(&self, md: &mut tonic::metadata::MetadataMap) {
        if let Ok(v) = self.to_header().parse() {
            md.insert(READ_META_HEADER, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for m in [
            ReadMeta::default(),
            ReadMeta {
                applied_index: 42,
                leader_committed_index: Some(44),
                stale_possible: true,
            },
            ReadMeta {
                applied_index: u64::MAX,
                leader_committed_index: None,
                stale_possible: false,
            },
        ] {
            assert_eq!(ReadMeta::from_header(&m.to_header()), Some(m));
            let mut md = tonic::metadata::MetadataMap::new();
            m.insert_into(&mut md);
            assert_eq!(ReadMeta::from_metadata(&md), Some(m));
        }
    }

    #[test]
    fn unknown_keys_are_ignored_and_bad_values_refused() {
        assert_eq!(
            ReadMeta::from_header("applied_index=3;future=x;stale_possible=false"),
            Some(ReadMeta {
                applied_index: 3,
                leader_committed_index: None,
                stale_possible: false
            })
        );
        assert_eq!(
            ReadMeta::from_header("applied_index=x;stale_possible=true"),
            None
        );
        assert_eq!(ReadMeta::from_header("stale_possible=true"), None);
        assert_eq!(ReadMeta::from_header(""), None);
        assert!(ReadMeta::from_metadata(&tonic::metadata::MetadataMap::new()).is_none());
    }
}
