//! The disk guard, with a scripted volume: plenty of space until the
//! database passes a size, then almost none. The run must stop cleanly
//! (what is ready is committed, the database stays consistent), and a rerun
//! must resume, skipping what was stored.
use graph_cli::diskinfo::{DiskProbe, DiskSample, MinFree};
use graph_cli::{index_dir, DirOpts};
use graph_store::{open_store, Store};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;

const GIB: u64 = 1 << 30;

fn open(db: &Path) -> anyhow::Result<Box<dyn Store>> {
    Ok(open_store(db, graph_cli::shipped_extractors())?)
}

fn run(
    db: &Path,
    dir: &Path,
    probe: DiskProbe,
    min_free: MinFree,
) -> (anyhow::Result<()>, serde_json::Value) {
    let mut out = Vec::new();
    let r = index_dir(
        DirOpts {
            db,
            org: "o",
            repo: "r",
            dir,
            json: true,
            max_file_size: 1 << 20,
            prune: false,
            force: false,
            reindex: false,
            jobs: 4,
            memory: None,
            deterministic: false,
            stats: true,
            trace: None,
            progress: Some(false),
            disk_probe: Some(probe),
            min_free_disk: min_free,
            disk_check: true,
            chunk_bytes: 64 << 10,
        },
        open,
        &mut out,
    );
    let v = serde_json::from_slice(&out).unwrap_or(serde_json::Value::Null);
    (r, v)
}

fn describe(db: &Path) -> serde_json::Value {
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_memory-graph"))
        .args(["--db", db.to_str().unwrap(), "describe", "--json"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}

#[test]
fn stops_cleanly_when_the_disk_runs_low_and_resumes() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("src");
    std::fs::create_dir(&dir).unwrap();
    for i in 0..3000 {
        let body = format!("fn f{i}() {{ {} }}\n", "let x = 1; ".repeat(300));
        std::fs::write(dir.join(format!("f{i:03}.rs")), body).unwrap();
    }
    let db = d.path().join("g.redb");

    // Free space "drops" once the database has grown 512 KB past the size
    // it was created with (redb preallocates).
    let db_path = db.clone();
    let seen = Arc::new(AtomicU64::new(0));
    let base = Arc::new(AtomicU64::new(0));
    let (seen2, base2) = (seen.clone(), base.clone());
    let shrinking: DiskProbe = Arc::new(move |_p: &Path| {
        let len = std::fs::metadata(&db_path).map_or(0, |m| m.len());
        if len > 0 && base2.load(Relaxed) == 0 {
            base2.store(len, Relaxed);
        }
        seen2.fetch_max(len, Relaxed);
        let grown = len.saturating_sub(base2.load(Relaxed));
        Some(DiskSample {
            total: 1000 * GIB,
            available: if grown > 512 << 10 { GIB } else { 100 * GIB },
        })
    });
    let (r, _) = run(&db, &dir, shrinking, MinFree::Bytes(2 * GIB));
    let err = format!("{:#}", r.unwrap_err());
    assert!(err.contains("stopped before the disk filled"), "{err}");
    assert!(err.contains("free space and rerun to resume"), "{err}");
    assert!(
        seen.load(Relaxed) > base.load(Relaxed) + (512 << 10),
        "the probe saw the database grow"
    );

    // Consistent, partial store.
    let desc = describe(&db);
    let repo = &desc["repos"][0];
    assert_eq!(repo["open_batch"], false, "{desc}");
    let stored = repo["files"].as_u64().unwrap();
    assert!(stored > 0 && stored < 3000, "stored {stored}: {desc}");

    // Plenty of space again: the rerun skips what was stored and finishes.
    let generous: DiskProbe = Arc::new(|_p: &Path| {
        Some(DiskSample {
            total: 1000 * GIB,
            available: 500 * GIB,
        })
    });
    let (r, sum) = run(&db, &dir, generous, MinFree::Bytes(2 * GIB));
    r.unwrap();
    assert_eq!(sum["files"], 3000, "{sum}");
    assert_eq!(sum["unchanged"].as_u64().unwrap(), stored, "{sum}");
    assert_eq!(sum["stats"]["disk"]["stopped"], serde_json::Value::Null);
    assert!(sum["stats"]["disk"]["projected_bytes"].as_u64().unwrap() > 0);
}

#[test]
fn refuses_up_front_below_the_reserve() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("src");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
    let db = d.path().join("g.redb");
    let low: DiskProbe = Arc::new(|_p: &Path| {
        Some(DiskSample {
            total: 100 * GIB,
            available: GIB,
        })
    });
    let (r, _) = run(&db, &dir, low, MinFree::Default);
    let err = format!("{:#}", r.unwrap_err());
    assert!(
        err.contains("refusing to index") && err.contains("--no-disk-check"),
        "{err}"
    );
    assert!(!db.exists(), "nothing was created");
}
