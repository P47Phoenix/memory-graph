//! Scratch memory an extractor needs beyond its input and output, counted
//! against the caller's memory budget (#254).
//!
//! Some extractors need a large amount of working memory for one file, such
//! as the Rust extractor's dedicated parse stack (up to 2 GiB reserved, a
//! third of it touched in the worst case). A caller that bounds its memory,
//! such as the CLI's `--memory` ingest budget, installs a [`ScratchBudget`]
//! on the thread that calls [`crate::Extractor::extract`]; the extractor
//! asks for its scratch with [`reserve_scratch`] before it allocates and the
//! reservation is given back when the returned guard drops. With no budget
//! installed (any library caller that does not opt in) a reservation is a
//! no-op, so extractors call it unconditionally.
//!
//! Language-agnostic: the budget only sees byte counts.
use std::cell::RefCell;
use std::sync::Arc;

/// A memory budget that extractors reserve scratch memory from.
pub trait ScratchBudget: Send + Sync {
    /// Reserve `bytes` before using them. May block until they fit; must
    /// always return eventually (a budget that cannot fit them should let
    /// them through rather than deadlock).
    fn reserve(&self, bytes: u64);
    /// Give back `bytes` reserved earlier.
    fn release(&self, bytes: u64);
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<dyn ScratchBudget>>> = const { RefCell::new(None) };
}

/// Run `f` with `budget` installed for this thread's scratch reservations,
/// restoring the previous budget (if any) afterwards, even on a panic.
pub fn with_scratch_budget<R>(budget: Arc<dyn ScratchBudget>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Arc<dyn ScratchBudget>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0.take();
            CURRENT.with(|c| *c.borrow_mut() = prev);
        }
    }
    let prev = CURRENT.with(|c| c.borrow_mut().replace(budget));
    let _restore = Restore(prev);
    f()
}

/// A scratch reservation, given back when dropped.
#[must_use = "the reservation is released when the guard drops"]
pub struct ScratchReservation {
    held: Option<(Arc<dyn ScratchBudget>, u64)>,
}

impl ScratchReservation {
    /// Bytes this guard holds against a budget (0 when none is installed).
    pub fn bytes(&self) -> u64 {
        self.held.as_ref().map_or(0, |h| h.1)
    }
}

impl Drop for ScratchReservation {
    fn drop(&mut self) {
        if let Some((b, n)) = self.held.take() {
            b.release(n);
        }
    }
}

/// Reserve `bytes` of scratch memory from the budget installed on this
/// thread (see [`with_scratch_budget`]); a no-op without one, or for 0.
pub fn reserve_scratch(bytes: u64) -> ScratchReservation {
    let budget = CURRENT.with(|c| c.borrow().clone());
    let held = match budget {
        Some(b) if bytes > 0 => {
            b.reserve(bytes);
            Some((b, bytes))
        }
        _ => None,
    };
    ScratchReservation { held }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    #[derive(Default)]
    struct Count(AtomicU64, AtomicU64);
    impl ScratchBudget for Count {
        fn reserve(&self, n: u64) {
            self.0.fetch_add(n, Relaxed);
            self.1.fetch_max(self.0.load(Relaxed), Relaxed);
        }
        fn release(&self, n: u64) {
            self.0.fetch_sub(n, Relaxed);
        }
    }

    #[test]
    fn reservations_go_to_the_installed_budget_and_come_back() {
        assert_eq!(reserve_scratch(5).bytes(), 0, "no budget: no-op");
        let c = Arc::new(Count::default());
        with_scratch_budget(c.clone(), || {
            let a = reserve_scratch(10);
            let b = reserve_scratch(7);
            assert_eq!((a.bytes(), b.bytes()), (10, 7));
            assert_eq!(c.0.load(Relaxed), 17);
            drop(a);
            assert_eq!(c.0.load(Relaxed), 7);
        });
        assert_eq!((c.0.load(Relaxed), c.1.load(Relaxed)), (0, 17));
        assert_eq!(reserve_scratch(5).bytes(), 0, "budget uninstalled after");
    }

    #[test]
    fn the_budget_is_uninstalled_after_a_panic() {
        let c = Arc::new(Count::default());
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_scratch_budget(c.clone(), || panic!("boom"))
        }));
        assert!(r.is_err());
        assert_eq!(reserve_scratch(5).bytes(), 0);
    }
}
