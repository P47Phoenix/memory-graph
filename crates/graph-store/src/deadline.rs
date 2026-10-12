//! A per-thread query deadline (ADR 0011 D3): the server runs each read on
//! one blocking thread and sets the client's `grpc-timeout` here, so a long
//! scan (an infix symbol walk) stops with [`StoreError::DeadlineExceeded`]
//! and never returns a partial result. With no deadline set (the embedded
//! default) nothing is checked.

use crate::StoreError;
use std::cell::Cell;
use std::time::Instant;

thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Run `f` with `deadline` as this thread's query deadline, restoring the
/// previous one afterwards (also on a panic).
pub fn with_query_deadline<T>(deadline: Option<Instant>, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Instant>);
    impl Drop for Restore {
        fn drop(&mut self) {
            DEADLINE.with(|d| d.set(self.0));
        }
    }
    let _restore = Restore(DEADLINE.with(|d| d.replace(deadline)));
    f()
}

/// `Err(DeadlineExceeded)` once this thread's query deadline has passed.
pub(crate) fn check_query_deadline() -> Result<(), StoreError> {
    match DEADLINE.with(Cell::get) {
        Some(d) if Instant::now() >= d => Err(StoreError::DeadlineExceeded(
            "the query passed its deadline before it finished; no rows were returned".into(),
        )),
        _ => Ok(()),
    }
}
