//! #172: `index` prepares every file against one fingerprint snapshot taken
//! before any commit, so its parse threads never open a read transaction
//! (`graph_store::precheck_reads` counts the ones `prepare` opens). Its own
//! test binary, so no other test in the process moves the counter.
use graph_cli::{index_dir, DirOpts};
use graph_store::{open_store, Store};
use std::path::Path;

fn open(db: &Path) -> anyhow::Result<Box<dyn Store>> {
    Ok(open_store(db, vec![])?)
}

fn run(db: &Path, dir: &Path, reindex: bool) -> String {
    let mut out = Vec::new();
    index_dir(
        DirOpts {
            db,
            org: "o",
            repo: "r",
            dir,
            json: true,
            max_file_size: Some(1 << 20),
            prune: false,
            force: false,
            reindex,
            jobs: 4,
            progress: Some(false),
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
    )
    .unwrap();
    String::from_utf8(out).unwrap()
}

#[test]
fn index_parse_threads_open_no_read_transaction() {
    let d = tempfile::tempdir().unwrap();
    let (db, dir) = (d.path().join("g.redb"), d.path().join("src"));
    let deep = dir.join("a").join("b");
    std::fs::create_dir_all(&deep).unwrap();
    for i in 0..20 {
        std::fs::write(dir.join(format!("f{i}.txt")), format!("top {i}")).unwrap();
        std::fs::write(deep.join(format!("c{i}.rs")), format!("fn c{i}() {{}}")).unwrap();
    }
    let before = graph_store::precheck_reads();
    run(&db, &dir, false);
    // Unchanged files, in subdirectories too, are skipped from the snapshot.
    let again = run(&db, &dir, false);
    std::fs::write(deep.join("c0.rs"), "fn changed() {}").unwrap();
    run(&db, &dir, false);
    run(&db, &dir, true);
    assert_eq!(
        graph_store::precheck_reads(),
        before,
        "a parse thread opened a pre-check read transaction"
    );
    let v: serde_json::Value = serde_json::from_str(again.lines().last().unwrap()).unwrap();
    assert_eq!(v["unchanged"], 40, "{again}");
}
