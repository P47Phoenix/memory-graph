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
            max_file_size: 1 << 20,
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
        },
        open,
        &mut out,
    );
    (r, String::from_utf8(out).unwrap())
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

/// A panicking extractor mid-run stops the run with an error naming the file,
/// without hanging, for any `jobs`; the same whole batches stay committed.
#[test]
fn extractor_panic_stops_the_run_for_any_jobs() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("src");
    std::fs::create_dir(&dir).unwrap();
    for i in 0..600 {
        let body = if i == 400 { "panic" } else { "fine" };
        std::fs::write(dir.join(format!("f{i:04}.zig")), body).unwrap();
    }
    let mut stored = Vec::new();
    for jobs in [1, 8] {
        let db = d.path().join(format!("g{jobs}.redb"));
        let mut out = Vec::new();
        let r = index_dir(
            DirOpts {
                db: &db,
                org: "o",
                repo: "r",
                dir: &dir,
                json: false,
                max_file_size: 1 << 20,
                prune: false,
                force: false,
                reindex: false,
                jobs,
                progress: Some(false),
                memory: None,
                deterministic: true,
                stats: false,
                trace: None,
                disk_probe: None,
                min_free_disk: graph_cli::diskinfo::MinFree::Default,
                disk_check: true,
                chunk_bytes: graph_cli::DEFAULT_CHUNK_BYTES,
            },
            open,
            &mut out,
        );
        let err = format!("{:#}", r.unwrap_err());
        assert!(
            err.contains("f0400.zig") && err.contains("panicked"),
            "{err}"
        );
        let s = V2Store::open(&db).unwrap();
        stored.push(s.count_nodes(NodeKind::File).unwrap());
    }
    assert_eq!(stored, [256, 256], "whole batches before the panic");
}
