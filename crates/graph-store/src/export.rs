//! ADR 0003 story 12: `export`, the portable escape hatch (decision D2).
//!
//! `export` walks the whole graph through [`Store`] alone (`roots`, then
//! `children`/`descendants`) and writes one JSON line per node (org, repo,
//! file, symbol, token) -- nothing here is specific to a storage layout.
//!
//! `migrate` (the v1 -> v2 conversion that used to live beside this) was
//! retired with the v1 format (D5); the last release that has it is tagged
//! `v1-last`.
use crate::api::Store;
use crate::StoreError;
use graph_core::Node;

type Result<T> = std::result::Result<T, StoreError>;

/// Dump `store`'s full node graph as newline-delimited JSON, one JSON object
/// per node (org, repo, file, symbol, token; each carries its span). Ids and
/// parent ids are included but are only meaningful *within this one export*
/// (self-consistent snapshot); they are not portable across stores or across
/// two exports. One consistent read view is used throughout
/// (`Store::snapshot`), so a writer running concurrently cannot produce a
/// torn export. Returns the node count.
pub fn export_ndjson(store: &dyn Store, w: &mut dyn std::io::Write) -> Result<usize> {
    let snap = store.snapshot()?;
    let mut n = 0usize;
    let mut write = |node: &Node| -> Result<()> {
        let line = serde_json::to_string(node).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        writeln!(w, "{line}").map_err(|e| StoreError::Storage(e.to_string()))?;
        n += 1;
        Ok(())
    };
    for org in snap.roots()? {
        write(&org)?;
        for repo in snap.children(org.id)? {
            write(&repo)?;
            for file in snap.children(repo.id)? {
                write(&file)?;
                for d in snap.descendants(file.id)? {
                    write(&d)?;
                }
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::StoreRead;
    use crate::V2Store;
    use graph_core::{tokenizer, Extraction as Ex};

    fn ex(text: &str) -> Ex {
        Ex {
            symbols: vec![],
            tokens: tokenizer::tokenize(text),
            has_errors: false,
        }
    }

    fn seed(store: &dyn Store) {
        store
            .ingest_file(
                "acme",
                "widgets",
                "a.rs",
                "rust",
                &ex("fn foo() { bar(); }"),
            )
            .unwrap();
        store
            .ingest_file(
                "acme",
                "widgets",
                "b.rs",
                "rust",
                &ex("struct S { x: i32 }"),
            )
            .unwrap();
        store
            .ingest_file(
                "acme",
                "gadgets",
                "c.zig",
                "zig",
                &ex("pub fn main() void {}"),
            )
            .unwrap();
    }

    #[test]
    fn export_ndjson_is_complete_and_well_formed() {
        let d = tempfile::tempdir().unwrap();
        let store = V2Store::open(d.path().join("g.redb")).unwrap();
        seed(&store);

        let mut buf = Vec::new();
        let n = export_ndjson(&store, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), n);
        assert!(n > 0);

        let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("well-formed JSON");
            let kind = v["kind"].as_str().unwrap().to_string();
            *kinds.entry(kind).or_default() += 1;
        }
        assert_eq!(kinds.get("org").copied().unwrap_or(0), 1);
        assert_eq!(kinds.get("repo").copied().unwrap_or(0), 2);
        assert_eq!(kinds.get("file").copied().unwrap_or(0), 3);
        assert!(kinds.get("token").copied().unwrap_or(0) > 0);
        // Every non-org line names a parent that appeared earlier.
        let mut seen = std::collections::HashSet::new();
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            if let Some(p) = v["parent"].as_u64() {
                assert!(seen.contains(&p), "parent before child: {line}");
            }
            seen.insert(v["id"].as_u64().unwrap());
        }
    }

    #[test]
    fn roots_lists_only_top_level_org_nodes() {
        let d = tempfile::tempdir().unwrap();
        let s = V2Store::open(d.path().join("g.redb")).unwrap();
        s.ingest_file("o1", "r1", "a.txt", "text", &ex("x"))
            .unwrap();
        s.ingest_file("o2", "r1", "a.txt", "text", &ex("x"))
            .unwrap();
        let mut names: Vec<String> = s.roots().unwrap().into_iter().map(|n| n.name).collect();
        names.sort();
        assert_eq!(names, ["o1", "o2"]);
    }
}
