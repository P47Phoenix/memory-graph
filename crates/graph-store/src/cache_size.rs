//! The default redb page-cache size (read cache phase 1, ADR 0008, epic
//! story 46). Without an explicit size, redb keeps its own 1 GiB default,
//! which ADR 0003 had documented as 256 MiB; a front end now derives the
//! size from the memory the machine has available instead.

/// The smallest derived cache: enough for the hot dictionary and posting
/// pages of a small store on a constrained machine.
pub const MIN_DERIVED_CACHE_BYTES: u64 = 64 << 20;

/// The largest derived cache: past this the OS page cache serves the rest
/// as well, and a long-lived server should not claim more by default.
pub const MAX_DERIVED_CACHE_BYTES: u64 = 4 << 30;

/// The size to use when the platform cannot report available memory: the
/// 256 MiB ADR 0003 documented.
pub const FALLBACK_CACHE_BYTES: u64 = 256 << 20;

/// The page-cache size for a machine with `avail_bytes` of available
/// memory: a quarter of it, clamped to
/// [[`MIN_DERIVED_CACHE_BYTES`], [`MAX_DERIVED_CACHE_BYTES`]]. Pure, so the
/// policy is tested with injected values; an explicit `--cache-bytes`
/// always wins over it.
pub fn derive_cache_bytes(avail_bytes: u64) -> u64 {
    (avail_bytes / 4).clamp(MIN_DERIVED_CACHE_BYTES, MAX_DERIVED_CACHE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    #[test]
    fn zero_available_clamps_to_the_floor() {
        assert_eq!(derive_cache_bytes(0), 64 * MIB);
    }

    #[test]
    fn a_small_machine_clamps_to_the_floor() {
        assert_eq!(derive_cache_bytes(128 * MIB), 64 * MIB);
    }

    #[test]
    fn a_mid_machine_gets_a_quarter() {
        assert_eq!(derive_cache_bytes(GIB), 256 * MIB);
        assert_eq!(derive_cache_bytes(8 * GIB), 2 * GIB);
    }

    #[test]
    fn a_large_machine_clamps_to_the_ceiling() {
        assert_eq!(derive_cache_bytes(64 * GIB), 4 * GIB);
        assert_eq!(derive_cache_bytes(u64::MAX), 4 * GIB);
    }
}
