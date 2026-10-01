//! CLI failure path: a file whose extraction has an invalid span fails alone.
//! A fake extractor is injected through `graph_cli::index_dir`'s opener, so
//! production code needs no test hook.
use graph_cli::{index_dir, DirOpts};
use graph_core::NodeKind;
use graph_core::{Extraction, Extractor, Span, SymbolDecl, SymbolKind};
use graph_store::{open_store, Store, StoreRead, V2Store};
use std::path::Path;

struct Fake;
impl Extractor for Fake {
    fn language(&self) -> &str {
        "zig"
    }
    fn extract(&self, source: &str) -> Extraction {
        assert!(!source.starts_with("panic"), "extractor panics");
        let bad = source.starts_with("bad");
        let span = Span {
            start: if bad { 3 } else { 0 },
            end: if bad { 1 } else { source.len() as u32 },
            start_line: 1,
            start_col: 1,
            end_line: 1,
            end_col: 1,
        };
        Extraction {
            symbols: vec![SymbolDecl {
                owner: None,
                name: "s".into(),
                kind: SymbolKind::Function,
                lang_kind: None,
                span,
            }],
            tokens: vec![],
            has_errors: false,
        }
    }
}

fn open(db: &Path) -> anyhow::Result<Box<dyn Store>> {
    Ok(open_store(db, vec![Box::new(Fake)])?)
}

fn run(db: &Path, dir: &Path, json: bool, prune: bool) -> (anyhow::Result<()>, String) {
    let mut out = Vec::new();
    let r = index_dir(
        DirOpts {
            db,
            org: "o",
            repo: "r",
            dir,
            json,
            max_file_size: Some(1 << 20),
            prune,
            force: false,
            reindex: false,
            jobs: 2,
            progress: None,
            memory: None,
            deterministic: false,
            stats: false,
            trace: None,
            disk_probe: None,
            min_free_disk: graph_cli::diskinfo::MinFree::Default,
            disk_check: true,
            chunk_bytes: graph_cli::DEFAULT_CHUNK_BYTES,
            remote: None,
            encoding: None,
            strict_encoding: false,
            compact: false,
        },
        open,
        &mut out,
    );
    (r.map(|_| ()), String::from_utf8(out).unwrap())
}

#[test]
fn invalid_span_fails_one_file_and_exits_nonzero() {
    let d = tempfile::tempdir().unwrap();
    let (db, dir) = (d.path().join("g.redb"), d.path().join("src"));
    std::fs::create_dir(&dir).unwrap();
    let w = |n: &str, c: &str| std::fs::write(dir.join(n), c).unwrap();
    w("old.zig", "fine old");
    w("gone.zig", "fine gone");
    let (r, _) = run(&db, &dir, false, false);
    r.unwrap();

    w("old.zig", "bad now");
    w("new.zig", "bad new");
    w("ok.zig", "fine new");
    std::fs::remove_file(dir.join("gone.zig")).unwrap();

    // Text mode, with --prune.
    let (r, text) = run(&db, &dir, false, true);
    let err = format!("{:#}", r.unwrap_err());
    assert!(err.contains("2 file(s) failed"), "{err}");
    assert!(text.contains("failed=2"), "{text}");
    assert!(text.contains("  failed: new.zig: invalid span: "), "{text}");
    assert!(text.contains("  failed: old.zig: invalid span: "), "{text}");
    assert!(
        !text.contains('`'),
        "path is not repeated in the reason: {text}"
    );

    let s = V2Store::open(&db).unwrap();
    assert!(s.file_tokens("o", "r", "ok.zig").unwrap().is_some());
    assert!(s.file_tokens("o", "r", "new.zig").unwrap().is_none());
    // Old version preserved, and --prune was skipped so gone.zig remains too.
    assert!(s.file_tokens("o", "r", "old.zig").unwrap().is_some());
    assert!(s.file_tokens("o", "r", "gone.zig").unwrap().is_some());
    assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 3);
    assert_eq!(s.count_nodes(NodeKind::Symbol).unwrap(), 3);
    drop(s);

    // JSON mode.
    let (r, json) = run(&db, &dir, true, false);
    assert!(r.is_err());
    let v: serde_json::Value = serde_json::from_str(json.trim()).unwrap();
    assert_eq!(v["failed"], 2);
    let files = v["failed_files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["path"], "new.zig");
    assert!(files[0]["reason"]
        .as_str()
        .unwrap()
        .starts_with("invalid span"));
    assert_eq!(v["files"], 1);
}

/// A panicking extractor mid-run fails that one file (#82), without hanging,
/// for any `jobs` and batching: every other file is stored, the run exits
/// nonzero naming the file, and `--prune` is skipped.
#[test]
fn extractor_panic_fails_one_file_for_any_jobs() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("src");
    std::fs::create_dir(&dir).unwrap();
    for i in 0..600 {
        let body = if i == 400 { "panic" } else { "fine" };
        std::fs::write(dir.join(format!("f{i:04}.zig")), body).unwrap();
    }
    for (jobs, deterministic) in [(1, true), (8, true), (1, false), (8, false)] {
        let db = d.path().join(format!("g{jobs}{deterministic}.redb"));
        let mut out = Vec::new();
        let r = index_dir(
            DirOpts {
                db: &db,
                org: "o",
                repo: "r",
                dir: &dir,
                json: false,
                max_file_size: Some(1 << 20),
                prune: false,
                force: false,
                reindex: false,
                jobs,
                progress: Some(false),
                memory: None,
                deterministic,
                stats: false,
                trace: None,
                disk_probe: None,
                min_free_disk: graph_cli::diskinfo::MinFree::Default,
                disk_check: true,
                chunk_bytes: graph_cli::DEFAULT_CHUNK_BYTES,
                remote: None,
                encoding: None,
                strict_encoding: false,
                compact: false,
            },
            open,
            &mut out,
        );
        let err = format!("{:#}", r.unwrap_err());
        assert!(err.contains("1 file(s) failed"), "{err}");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("failed=1"), "{text}");
        assert!(
            text.contains("  failed: f0400.zig: extractor panicked: extractor panics"),
            "{text}"
        );
        let s = V2Store::open(&db).unwrap();
        assert_eq!(s.count_nodes(NodeKind::File).unwrap(), 599, "jobs {jobs}");
        assert!(s.file_tokens("o", "r", "f0400.zig").unwrap().is_none());
    }
}

fn run_mem(
    db: &Path,
    dir: &Path,
    memory: &str,
    jobs: usize,
) -> (anyhow::Result<()>, serde_json::Value) {
    let mut out = Vec::new();
    let r = index_dir(
        DirOpts {
            db,
            org: "o",
            repo: "r",
            dir,
            json: true,
            max_file_size: Some(1 << 20),
            prune: false,
            force: false,
            reindex: false,
            jobs,
            progress: Some(false),
            memory: Some(graph_cli::sysinfo::parse_memory_spec(memory).unwrap()),
            deterministic: false,
            stats: true,
            trace: None,
            disk_probe: None,
            min_free_disk: graph_cli::diskinfo::MinFree::Default,
            disk_check: true,
            chunk_bytes: graph_cli::DEFAULT_CHUNK_BYTES,
            remote: None,
            encoding: None,
            strict_encoding: false,
            compact: false,
        },
        open,
        &mut out,
    );
    (r.map(|_| ()), serde_json::from_slice(&out).unwrap())
}

/// Span failures are recorded at flush and panics on arrival, so the
/// failure list is sorted: the same under any budget or thread count.
#[test]
fn failed_files_order_does_not_depend_on_batching() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("src");
    std::fs::create_dir(&dir).unwrap();
    for i in 0..40 {
        let body = match i % 4 {
            0 => "bad span",
            1 => "panic now",
            _ => "fine",
        };
        std::fs::write(dir.join(format!("f{i:02}.zig")), body).unwrap();
    }
    let mut seen = Vec::new();
    for (k, (mem, jobs)) in [("1K", 1), ("64M", 8)].into_iter().enumerate() {
        let (r, v) = run_mem(&d.path().join(format!("g{k}.redb")), &dir, mem, jobs);
        assert!(r.is_err());
        assert_eq!(v["failed"], 20, "{v}");
        assert_eq!(v["files"], 20, "{v}");
        seen.push(v["failed_files"].clone());
    }
    assert_eq!(seen[0], seen[1]);
    let paths: Vec<_> = seen[0]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap().to_string())
        .collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
}

/// A panicked file gives its budget back: many panicking files, each larger
/// than a tiny fixed budget, still finish (a leak would stall admission).
#[test]
fn panicked_files_release_their_budget() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("src");
    std::fs::create_dir(&dir).unwrap();
    let big = format!("panic{}", "x".repeat(8 << 10));
    for i in 0..30 {
        std::fs::write(dir.join(format!("p{i:02}.zig")), &big).unwrap();
    }
    std::fs::write(dir.join("z.zig"), "fine").unwrap();
    let db = d.path().join("g.redb");
    let t = std::thread::spawn(move || run_mem(&db, &dir, "4K", 2).1);
    let t0 = std::time::Instant::now();
    while !t.is_finished() {
        assert!(t0.elapsed().as_secs() < 60, "index stalled: budget leaked");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let v = t.join().unwrap();
    assert_eq!(v["failed"], 30, "{v}");
    assert_eq!(v["files"], 1, "{v}");
}

/// Make reading `path` fail for this process: no permissions on unix, an
/// exclusive (no sharing) handle on Windows. `None` when the platform lets
/// the read through anyway (running as root).
fn make_unreadable(path: &Path) -> Option<Box<dyn std::any::Any>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(path).is_ok() {
            return None;
        }
        Some(Box::new(()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let h = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .unwrap();
        assert!(
            std::fs::read(path).is_err(),
            "exclusive handle blocks reads"
        );
        Some(Box::new(h))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

/// A file that cannot be read is skipped as unreadable and blocks `--prune`
/// (#82: the writer's `walk_errors |= unreadable` had no test), so a
/// transient read failure never deletes stored files.
#[test]
fn unreadable_file_blocks_prune() {
    let d = tempfile::tempdir().unwrap();
    let (db, dir) = (d.path().join("g.redb"), d.path().join("src"));
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("gone.txt"), "gone\n").unwrap();
    std::fs::write(dir.join("locked.txt"), "locked\n").unwrap();
    let (r, _) = run(&db, &dir, false, false);
    r.unwrap();
    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    let locked = dir.join("locked.txt");
    let Some(guard) = make_unreadable(&locked) else {
        eprintln!("skipped: this process can read any file");
        return;
    };
    let (r, text) = run(&db, &dir, false, true);
    drop(guard);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644));
    }
    r.unwrap();
    assert!(text.contains("skipped (unreadable): 1"), "{text}");
    assert!(text.contains("pruned=0"), "{text}");
    let s = V2Store::open(&db).unwrap();
    assert!(
        s.file_tokens("o", "r", "gone.txt").unwrap().is_some(),
        "--prune was skipped"
    );
}
