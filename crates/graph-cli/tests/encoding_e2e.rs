//! `--encoding`, `--strict-encoding`, `MEMORY_GRAPH_ENCODING` and the
//! `.memory-graph.toml` `[encoding]` globs, end to end over the real binary
//! (epic story 42, ADR 0007 C3, C4, C8), embedded and through `--server`.
//!
//! The probe is the token `café`: its windows-1252 bytes (`63 61 66 E9`)
//! only produce it when decoded as windows-1252, and its UTF-8 bytes only
//! when decoded as UTF-8, so `search café --grain file` shows which files
//! were decoded how.
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_memory-graph");
const CP1252_CAFE: &[u8] = b"caf\xe9\n";
/// `E9 0A` is not a valid Shift_JIS pair: lossy under that hint.
const SJIS_LOSSY: &[u8] = b"caf\xe9\n";
const STRICT: &str = "invalid in its encoding (--strict-encoding)";

fn cmd() -> Command {
    let mut c = Command::new(BIN);
    c.env_remove("MEMORY_GRAPH_SERVER")
        .env_remove("MEMORY_GRAPH_READ")
        .env_remove("MEMORY_GRAPH_ENCODING")
        .env_remove("MEMORY_GRAPH_WRITE_DEADLINE")
        .env_remove("MEMORY_GRAPH_READ_DEADLINE")
        .env("MEMORY_GRAPH_LOCK_WAIT_MS", "300");
    c
}

fn run_env(args: &[&str], env: &[(&str, &str)]) -> Output {
    cmd().envs(env.iter().copied()).args(args).output().unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn ok_env(args: &[&str], env: &[(&str, &str)]) -> String {
    let o = run_env(args, env);
    assert!(o.status.success(), "{args:?}: {}", text(&o));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn ok(args: &[&str]) -> String {
    ok_env(args, &[])
}

/// `--db <db>` or `--server <addr>`.
#[derive(Clone)]
enum Tgt {
    Db(String),
    Server(String),
}

impl Tgt {
    fn args(&self) -> Vec<&str> {
        match self {
            Tgt::Db(d) => vec!["--db", d],
            Tgt::Server(a) => vec!["--server", a],
        }
    }
}

/// `index --json` of `dir` into `t`; the parsed summary.
fn index(t: &Tgt, dir: &Path, extra: &[&str], env: &[(&str, &str)]) -> serde_json::Value {
    let mut a = t.args();
    a.extend(["index", "--org", "o", "--repo", "r", "--json"]);
    a.extend_from_slice(extra);
    let d = dir.to_str().unwrap();
    a.push(d);
    let out = ok_env(&a, env);
    let mut v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let o = v.as_object_mut().unwrap();
    o.remove("elapsed_ms");
    o.remove("forwarded_to_leader"); // a --server run only
    v
}

/// The files `café` is found in.
fn cafe_files(t: &Tgt) -> BTreeSet<String> {
    let mut a = t.args();
    a.extend(["search", "café", "--grain", "file", "--json"]);
    let out = ok(&a);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let files = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let s = r.to_string();
            [
                "bom.txt",
                "legacy/a.txt",
                "other/b.txt",
                "plain.md",
                "sj/c.txt",
            ]
            .into_iter()
            .find(|f| s.contains(f))
            .unwrap_or_else(|| panic!("unexpected row {s}"))
            .to_string()
        })
        .collect();
    files
}

fn set(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// A tree with one file per precedence level:
/// * `bom.txt`: a UTF-8 BOM (wins over everything);
/// * `legacy/a.txt`: windows-1252 bytes, matched by a windows-1252 glob;
/// * `other/b.txt`: the same bytes, matched by a `utf-8` glob (so lossy);
/// * `plain.md`: UTF-8, matched by no glob (auto).
///
/// The last glob would turn every `.txt` into UTF-16LE: it never applies,
/// because an earlier glob matches first.
fn tree(root: &Path) -> PathBuf {
    let src = root.join("src");
    std::fs::create_dir_all(src.join("legacy")).unwrap();
    std::fs::create_dir_all(src.join("other")).unwrap();
    std::fs::write(src.join("bom.txt"), b"\xef\xbb\xbfcaf\xc3\xa9\n").unwrap();
    std::fs::write(src.join("legacy/a.txt"), CP1252_CAFE).unwrap();
    std::fs::write(src.join("other/b.txt"), CP1252_CAFE).unwrap();
    std::fs::write(src.join("plain.md"), "café\n").unwrap();
    write_config(&src, "utf-8");
    src
}

fn write_config(src: &Path, other: &str) {
    std::fs::write(
        src.join(".memory-graph.toml"),
        format!(
            "[encoding]\n\"legacy/**\" = \"windows-1252\"\n\"other/**\" = \"{other}\"\n\"**/*.txt\" = \"utf-16le\"\n"
        ),
    )
    .unwrap();
}

#[test]
fn precedence_bom_then_cli_then_first_glob_then_auto() {
    let d = tempfile::tempdir().unwrap();
    let src = tree(d.path());
    let db = Tgt::Db(d.path().join("g.redb").display().to_string());

    // Globs (first match wins), auto for the rest, the BOM over its glob.
    let s = index(&db, &src, &[], &[]);
    assert_eq!(s["files"], 5, "{s}"); // 4 + the config itself
    assert_eq!(
        cafe_files(&db),
        set(&["bom.txt", "legacy/a.txt", "plain.md"])
    );

    // --encoding beats every glob and auto; the BOM still wins. The new
    // hint re-indexes the affected files without --reindex.
    let s = index(&db, &src, &["--encoding", "windows-1252"], &[]);
    assert_eq!(
        cafe_files(&db),
        set(&["bom.txt", "legacy/a.txt", "other/b.txt"])
    );
    // Only bom.txt (BOM) and legacy/a.txt (same decode) are unchanged; the
    // all-ASCII config is re-indexed too, since an explicit hint is recorded
    // (and fingerprinted) even for ASCII.
    assert_eq!(s["unchanged"], 2, "{s}");

    // `auto` on the command line is no override: the globs apply again.
    index(&db, &src, &["--encoding", "auto"], &[]);
    assert_eq!(
        cafe_files(&db),
        set(&["bom.txt", "legacy/a.txt", "plain.md"])
    );
}

#[test]
fn memory_graph_encoding_env_is_the_default_and_the_flag_wins() {
    let d = tempfile::tempdir().unwrap();
    let src = tree(d.path());
    let db = Tgt::Db(d.path().join("g.redb").display().to_string());
    index(&db, &src, &[], &[("MEMORY_GRAPH_ENCODING", "windows-1252")]);
    assert_eq!(
        cafe_files(&db),
        set(&["bom.txt", "legacy/a.txt", "other/b.txt"])
    );
    index(
        &db,
        &src,
        &["--encoding", "auto"],
        &[("MEMORY_GRAPH_ENCODING", "windows-1252")],
    );
    assert_eq!(
        cafe_files(&db),
        set(&["bom.txt", "legacy/a.txt", "plain.md"])
    );
    // A bad value in the environment is refused like a bad flag.
    let o = run_env(
        &["--db", "x.redb", "index", "--org", "o", "--repo", "r", "."],
        &[("MEMORY_GRAPH_ENCODING", "klingon")],
    );
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(text(&o).contains("unknown encoding label"), "{}", text(&o));
}

#[test]
fn editing_the_config_reindexes_exactly_the_affected_files() {
    let d = tempfile::tempdir().unwrap();
    let src = tree(d.path());
    let db = Tgt::Db(d.path().join("g.redb").display().to_string());
    index(&db, &src, &[], &[]);
    write_config(&src, "windows-1252");
    let s = index(&db, &src, &[], &[]);
    // other/b.txt (new decode) and the edited config itself change; the
    // other three are unchanged.
    assert_eq!(s["files"], 5, "{s}");
    assert_eq!(s["unchanged"], 3, "{s}");
    assert_eq!(
        cafe_files(&db),
        set(&["bom.txt", "legacy/a.txt", "other/b.txt", "plain.md"])
    );
}

/// Both kinds of strict refusal land in one bucket: `NotUtf8` (a file
/// decoded as UTF-8) and the `Rejected` naming another encoding.
fn strict_tree(root: &Path) -> PathBuf {
    let src = tree(root);
    std::fs::create_dir_all(src.join("sj")).unwrap();
    std::fs::write(src.join("sj/c.txt"), SJIS_LOSSY).unwrap();
    std::fs::write(
        src.join(".memory-graph.toml"),
        "[encoding]\n\"legacy/**\" = \"windows-1252\"\n\"other/**\" = \"utf-8\"\n\"sj/**\" = \"shift_jis\"\n",
    )
    .unwrap();
    src
}

fn check_strict(s: &serde_json::Value) {
    let refused: BTreeSet<String> = s["skipped_by_reason"][STRICT]
        .as_array()
        .unwrap_or_else(|| panic!("no strict bucket: {s}"))
        .iter()
        .map(|p| p.as_str().unwrap().replace('\\', "/"))
        .collect();
    assert_eq!(refused, set(&["other/b.txt", "sj/c.txt"]), "{s}");
    assert_eq!(s["files"], 4, "{s}");
}

#[test]
fn strict_encoding_refuses_lossy_files_and_indexes_the_rest() {
    let d = tempfile::tempdir().unwrap();
    let src = strict_tree(d.path());
    let db = Tgt::Db(d.path().join("g.redb").display().to_string());
    check_strict(&index(&db, &src, &["--strict-encoding"], &[]));
    assert_eq!(
        cafe_files(&db),
        set(&["bom.txt", "legacy/a.txt", "plain.md"])
    );

    // The text summary names the bucket too.
    let db2 = d.path().join("g2.redb").display().to_string();
    let out = ok(&[
        "--db",
        &db2,
        "index",
        "--org",
        "o",
        "--repo",
        "r",
        "--strict-encoding",
        src.to_str().unwrap(),
    ]);
    assert!(out.contains(STRICT), "{out}");

    // Without --strict-encoding the same files are stored (lossy).
    let db3 = Tgt::Db(d.path().join("g3.redb").display().to_string());
    let s = index(&db3, &src, &[], &[]);
    assert_eq!(s["files"], 6, "{s}");
}

#[test]
fn index_file_encoding_and_strict() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("a.txt");
    std::fs::write(&f, CP1252_CAFE).unwrap();
    let f = f.to_str().unwrap();
    let db = d.path().join("g.redb").display().to_string();
    let base = [
        "--db",
        db.as_str(),
        "index-file",
        "--org",
        "o",
        "--repo",
        "r",
    ];

    // utf-8 + strict: today's NotUtf8 refusal; nothing stored.
    let mut a = base.to_vec();
    a.extend(["--encoding", "utf-8", "--strict-encoding", f]);
    let o = run_env(&a, &[]);
    assert!(!o.status.success());
    assert!(text(&o).contains("is not valid UTF-8"), "{}", text(&o));

    let mut a = base.to_vec();
    a.extend(["--encoding", "latin1", "--strict-encoding", f]);
    ok(&a);
    let hits = ok(&["--db", &db, "search", "café", "--json"]);
    assert!(hits.contains("a.txt"), "{hits}");

    for (label, why) in [
        ("klingon", "unknown encoding label"),
        ("replacement", "`replacement` encoding"),
    ] {
        let mut a = base.to_vec();
        a.extend(["--encoding", label, f]);
        let o = run_env(&a, &[]);
        assert_eq!(o.status.code(), Some(2), "{}", text(&o));
        assert!(text(&o).contains(why), "{label}: {}", text(&o));
    }
}

#[test]
fn config_errors_name_the_file_and_key_and_write_nothing() {
    for (body, want) in [
        ("[encoding]\n\"legacy/**\" = \"klingon\"\n", "legacy/**"),
        (
            "[encoding]\n\"legacy/**\" = \"replacement\"\n",
            "replacement",
        ),
        ("[encoding]\n\"a[\" = \"utf-8\"\n", "invalid glob"),
        ("[encodings]\n", "unknown key `encodings`"),
        ("[encoding\n", "TOML"),
    ] {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(src.join(".memory-graph.toml"), body).unwrap();
        let db = d.path().join("g.redb");
        let o = run_env(
            &[
                "--db",
                db.to_str().unwrap(),
                "index",
                "--org",
                "o",
                "--repo",
                "r",
                src.to_str().unwrap(),
            ],
            &[],
        );
        let t = text(&o);
        assert!(!o.status.success(), "{body}: {t}");
        assert!(t.contains(".memory-graph.toml"), "{body}: {t}");
        if want != "TOML" {
            assert!(t.contains(want), "{body}: {t}");
        }
        assert!(!db.exists(), "{body}: a database was written");
    }
}

/// `ansi` is the Windows system code page, resolved on the client.
#[cfg(windows)]
#[test]
fn ansi_is_the_system_code_page_on_windows() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("a.txt");
    std::fs::write(&f, b"plain ascii\n").unwrap();
    let db = d.path().join("g.redb");
    let o = run_env(
        &[
            "--db",
            db.to_str().unwrap(),
            "index-file",
            "--org",
            "o",
            "--repo",
            "r",
            "--encoding",
            "ansi",
            f.to_str().unwrap(),
        ],
        &[],
    );
    match graph_core::encoding::resolve_ansi() {
        Err(e) => {
            assert!(!o.status.success());
            assert!(text(&o).contains(&e.to_string()), "{}", text(&o));
        }
        Ok(enc) => {
            assert!(o.status.success(), "{}", text(&o));
            let nd = d.path().join("g.ndjson");
            ok(&[
                "--db",
                db.to_str().unwrap(),
                "export",
                "--out",
                nd.to_str().unwrap(),
            ]);
            let file = std::fs::read_to_string(&nd)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
                .find(|v| v["kind"] == "file")
                .unwrap();
            // An explicit single-byte hint is recorded even for ASCII.
            assert_eq!(file["encoding"].as_str(), Some(enc.name()), "{file}");
        }
    }
}

// --- the same results embedded and through `--server` ---

struct Server {
    child: Child,
    addr: String,
}

impl Server {
    fn start(db: &Path) -> Server {
        let mut child = cmd()
            .args(["serve", "--db"])
            .arg(db)
            .args(["--listen", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(l);
            }
        });
        let line: String = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("serve printed its listening line");
        let addr = line
            .split("listening on ")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .unwrap_or_else(|| panic!("no address in {line:?}"))
            .to_string();
        Server { child, addr }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Ok(s) =
            graph_client::RemoteStore::connect(graph_client::ClientConfig::new(&self.addr))
        {
            let _ = s.admin_shutdown(Duration::from_secs(10));
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn embedded_and_server_agree() {
    let d = tempfile::tempdir().unwrap();
    let src = strict_tree(d.path());
    let emb = Tgt::Db(d.path().join("e.redb").display().to_string());
    let server = Server::start(&d.path().join("s.redb"));
    let srv = Tgt::Server(server.addr.clone());

    let runs: [&[&str]; 4] = [
        &[],
        &["--strict-encoding"],
        &["--encoding", "windows-1252"],
        &["--encoding", "utf-8", "--strict-encoding"],
    ];
    for extra in runs {
        let a = index(&emb, &src, extra, &[]);
        let b = index(&srv, &src, extra, &[]);
        assert_eq!(a, b, "{extra:?}");
        assert_eq!(cafe_files(&emb), cafe_files(&srv), "{extra:?}");
    }
    let s = index(&srv, &src, &["--strict-encoding"], &[]);
    check_strict(&s);
}
