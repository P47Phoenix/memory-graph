//! ADR 0009 D9 (epic story 53): log messages are exported as OTLP record
//! bodies, which no allow-list filters, so a message must be a constant
//! string. Values go in fields (`why = %why`), where they are either
//! allow-listed or stripped. This test scans the `tracing` events of the
//! serving crates (graph-server, graph-client, graph-cli) and fails on a
//! message format string that interpolates anything (`{x}`, `{}`, `{:?}`).
use std::path::{Path, PathBuf};

const CRATES: [&str; 3] = ["graph-server", "graph-client", "graph-cli"];
const MACROS: [&str; 5] = ["trace!(", "debug!(", "info!(", "warn!(", "error!("];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The macro's argument text, from just after `(` to its matching `)`.
fn args(s: &[u8], mut i: usize) -> &[u8] {
    let start = i;
    let mut depth = 1;
    while i < s.len() && depth > 0 {
        match s[i] {
            b'"' => {
                i += 1;
                while i < s.len() && s[i] != b'"' {
                    i += if s[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    &s[start..i.saturating_sub(1)]
}

/// The message: the first top-level string literal that is not a field's
/// or the target's value (not preceded by `=` or `:`); literals nested in
/// a field's expression (`error = %format!("{e:#}")`) are skipped.
fn message(a: &[u8]) -> Option<String> {
    let mut i = 0;
    let mut depth = 0;
    while i < a.len() {
        if matches!(a[i], b'(' | b'[' | b'{') {
            depth += 1;
            i += 1;
        } else if matches!(a[i], b')' | b']' | b'}') {
            depth -= 1;
            i += 1;
        } else if a[i] == b'"' && depth > 0 {
            i += 1;
            while i < a.len() && a[i] != b'"' {
                i += if a[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
        } else if a[i] == b'"' {
            let start = i + 1;
            let mut j = start;
            while j < a.len() && a[j] != b'"' {
                j += if a[j] == b'\\' { 2 } else { 1 };
            }
            let before = a[..i].iter().rev().find(|c| !c.is_ascii_whitespace());
            if !matches!(before, Some(b'=') | Some(b':')) {
                return Some(String::from_utf8_lossy(&a[start..j.min(a.len())]).into_owned());
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    None
}

fn interpolates(msg: &str) -> bool {
    msg.replace("{{", "").replace("}}", "").contains('{')
}

#[test]
fn log_messages_interpolate_nothing() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for c in CRATES {
        rust_files(&root.join(c).join("src"), &mut files);
    }
    let mut bad = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap();
        let s = text.as_bytes();
        for m in MACROS {
            let mut from = 0;
            while let Some(k) = text[from..].find(m) {
                let at = from + k;
                from = at + m.len();
                // `tracing::warn!(` or `warn!(`, not `eprintln!(` etc.
                let prev = s[..at].last().copied().unwrap_or(b' ');
                if prev.is_ascii_alphanumeric() || prev == b'_' {
                    continue;
                }
                if let Some(msg) = message(args(s, from)) {
                    if interpolates(&msg) {
                        let line = text[..at].matches('\n').count() + 1;
                        bad.push(format!("{}:{line}: {msg:?}", f.display()));
                    }
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "log messages must be constant (put values in fields):\n{}",
        bad.join("\n")
    );
}

#[test]
fn the_scanner_finds_messages() {
    assert_eq!(
        message(br#"target: "x", node = 1, peer = "a", "hello {x}", x"#).as_deref(),
        Some("hello {x}")
    );
    assert!(interpolates("a {}"));
    assert!(!interpolates("a {{literal}}"));
    assert_eq!(message(b"node = id, \"done\"").as_deref(), Some("done"));
    assert_eq!(
        message(br#"error = %format!("{e:#}"), code, "failed""#).as_deref(),
        Some("failed")
    );
}
