use super::*;
use std::io;
use std::sync::atomic::AtomicU32;

fn full() -> io::Error {
    io::Error::new(io::ErrorKind::StorageFull, "no space")
}

#[test]
fn a_disk_full_error_is_retried_until_it_succeeds_and_then_clears() {
    let s = DiskStall::new();
    let calls = AtomicU32::new(0);
    let recovered = AtomicU32::new(0);
    let r = s.retry(
        StallSource::Apply,
        || {
            if calls.fetch_add(1, Ordering::SeqCst) < 3 {
                Err(full())
            } else {
                Ok(7)
            }
        },
        || {
            recovered.fetch_add(1, Ordering::SeqCst);
        },
    );
    assert_eq!(r.unwrap(), 7);
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(recovered.load(Ordering::SeqCst), 3, "reopened after each");
    assert_eq!(s.current(), None, "cleared once it succeeded");
}

#[test]
fn other_errors_are_returned_at_once() {
    let s = DiskStall::new();
    let r: Result<(), io::Error> = s.retry(
        StallSource::LogAppend,
        || Err(io::Error::other("corrupt")),
        || {},
    );
    assert_eq!(r.unwrap_err().to_string(), "corrupt");
    assert_eq!(s.current(), None);
}

/// redb's `PreviousIo` after the disk-full error is retried after a
/// reopen; one a reopen never cures is returned after the cap.
#[test]
fn previous_io_is_reopened_and_capped() {
    let prev = || io::Error::other(redb::StorageError::PreviousIo.to_string());
    let s = DiskStall::new();
    let calls = AtomicU32::new(0);
    let r = s.retry(
        StallSource::Apply,
        || match calls.fetch_add(1, Ordering::SeqCst) {
            0 => Err(full()),
            1 => Err(prev()),
            _ => Ok(()),
        },
        || {},
    );
    assert!(r.is_ok());
    let s = DiskStall::new();
    let calls = AtomicU32::new(0);
    let r: Result<(), _> = s.retry(
        StallSource::Apply,
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(prev())
        },
        || {},
    );
    assert!(r.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), MAX_PREVIOUS_IO + 1);
    assert_eq!(s.current(), None, "not reported as a full disk");
}

#[test]
fn a_stall_is_visible_and_stop_ends_the_retries() {
    let s = DiskStall::new();
    let s2 = s.clone();
    let h = std::thread::spawn(move || {
        s2.retry(StallSource::LogAppend, || Err::<(), _>(full()), || {})
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let msg = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), s.stalled())
            .await
            .unwrap()
    });
    assert!(graph_proto::error::is_disk_full(&msg), "{msg}");
    assert!(msg.contains("Raft log"), "{msg}");
    assert!(s.is_stalled(StallSource::LogAppend));
    assert!(!s.is_stalled(StallSource::Apply));
    s.stop();
    assert!(h.join().unwrap().is_err());
}

#[test]
fn the_precheck_refuses_below_the_guard_floor_as_disk_full() {
    let d = tempfile::tempdir().unwrap();
    let free = Arc::new(std::sync::atomic::AtomicU64::new(10));
    let f2 = Arc::clone(&free);
    let g = DiskGuard::new(
        100,
        Arc::new(move |_| Some(f2.load(Ordering::SeqCst))),
        d.path(),
        &d.path().join("s"),
        &d.path().join("l"),
    );
    let s = DiskStall::new();
    assert!(s.precheck("apply").is_ok(), "no guard");
    s.set_guard(Some(g));
    let e = s.precheck("apply").unwrap_err();
    assert!(graph_proto::error::error_is_disk_full(&e), "{e}");
    free.store(100, Ordering::SeqCst);
    assert!(s.precheck("apply").is_ok());
}

#[test]
fn the_failpoint_error_reads_as_disk_full() {
    let s = DiskStall::new();
    assert!(s.injected(StallSource::Apply).is_none());
    s.fail_apply(true);
    let e = s.injected(StallSource::Apply).unwrap();
    assert!(graph_proto::error::is_disk_full(&e.to_string()), "{e}");
    assert!(s.injected(StallSource::LogAppend).is_none());
}
