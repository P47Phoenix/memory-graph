//! Client-side encoding hints (ADR 0007 C4, C8; epic story 42): the
//! `--encoding` argument and the `[encoding]` table of a `.memory-graph.toml`
//! at the indexed root. Both are resolved here, on the client, into a
//! concrete `encoding_rs` encoding sent with each file, so an embedded run
//! and a `--server` run decode the same way and a server never reads repo
//! files or consults its own code page.
//!
//! Precedence per file: BOM (applied by the decoder) > `--encoding` (other
//! than `auto`) > the first matching `.memory-graph.toml` glob > auto.

use graph_core::encoding::{hint_from_label, resolve_ansi, Encoding, EncodingError};
use std::path::{Path, PathBuf};

/// The file name looked for at the indexed root.
pub const CONFIG_FILE: &str = ".memory-graph.toml";

/// A parsed `--encoding` value: `auto` (detect) or a concrete encoding
/// (`ansi` already resolved to the system code page).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodingArg(pub Option<&'static Encoding>);

/// Resolves a label as `--encoding` and `.memory-graph.toml` accept it:
/// `auto` (no hint), `ansi` (ADR 0007 C4), or any `encoding_rs` label other
/// than `replacement`.
pub fn resolve_label(label: &str) -> Result<Option<&'static Encoding>, EncodingError> {
    let l = label.trim();
    if l.eq_ignore_ascii_case("auto") {
        Ok(None)
    } else if l.eq_ignore_ascii_case("ansi") {
        resolve_ansi().map(Some)
    } else {
        hint_from_label(l).map(Some)
    }
}

/// The clap value parser for `--encoding` (and `MEMORY_GRAPH_ENCODING`).
pub fn parse_encoding_arg(s: &str) -> Result<EncodingArg, String> {
    resolve_label(s).map(EncodingArg).map_err(|e| e.to_string())
}

/// The `[encoding]` globs of a `.memory-graph.toml`, in file order.
#[derive(Debug, Default)]
pub struct EncodingConfig {
    rules: Vec<(globset::GlobMatcher, Option<&'static Encoding>)>,
}

impl EncodingConfig {
    /// Reads `<root>/.memory-graph.toml`; no file is an empty config. Any
    /// problem (unreadable, not TOML, an unknown key, a bad glob or label)
    /// is an error naming the file and the key, returned before anything is
    /// written.
    pub fn load(root: &Path) -> anyhow::Result<EncodingConfig> {
        let path = root.join(CONFIG_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text, &path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow::anyhow!("cannot read `{}`: {e}", path.display())),
        }
    }

    /// Parses the text of a `.memory-graph.toml` (`path` names it in errors).
    pub fn parse(text: &str, path: &Path) -> anyhow::Result<EncodingConfig> {
        let err = |msg: String| anyhow::anyhow!("invalid `{}`: {msg}", path.display());
        let doc: toml::Table = text.parse().map_err(|e| err(format!("{e}")))?;
        let mut rules = Vec::new();
        for (key, value) in &doc {
            if key != "encoding" {
                return Err(err(format!(
                    "unknown key `{key}` (the only table is `[encoding]`)"
                )));
            }
            let table = value.as_table().ok_or_else(|| {
                err("`encoding` must be a table of glob = \"label\" entries".into())
            })?;
            for (glob, label) in table {
                let label = label.as_str().ok_or_else(|| {
                    err(format!(
                        "`encoding.\"{glob}\"` must be a string (an encoding label, `ansi` or `auto`)"
                    ))
                })?;
                let enc =
                    resolve_label(label).map_err(|e| err(format!("`encoding.\"{glob}\"`: {e}")))?;
                let matcher = globset::GlobBuilder::new(glob)
                    .literal_separator(true)
                    .build()
                    .map_err(|e| err(format!("`encoding.\"{glob}\"`: invalid glob: {e}")))?
                    .compile_matcher();
                rules.push((matcher, enc));
            }
        }
        Ok(EncodingConfig { rules })
    }

    /// The hint of the first glob matching `rel` (a `/`-separated path
    /// relative to the root): `None` when no glob matches, `Some(None)` when
    /// the match says `auto`.
    pub fn lookup(&self, rel: &str) -> Option<Option<&'static Encoding>> {
        let rel = PathBuf::from(rel);
        self.rules
            .iter()
            .find(|(m, _)| m.is_match(&rel))
            .map(|(_, e)| *e)
    }

    /// The hint sent for `rel`: `--encoding` (when not `auto`) over the
    /// first matching glob; `None` means auto-detect. A BOM still wins, in
    /// the decoder.
    pub fn resolve(&self, cli: Option<&'static Encoding>, rel: &str) -> Option<&'static Encoding> {
        cli.or_else(|| self.lookup(rel).flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(l: &str) -> &'static Encoding {
        Encoding::for_label(l.as_bytes()).unwrap()
    }

    fn cfg(text: &str) -> anyhow::Result<EncodingConfig> {
        EncodingConfig::parse(text, Path::new("root/.memory-graph.toml"))
    }

    #[test]
    fn labels_resolve_and_refuse() {
        assert_eq!(resolve_label("auto"), Ok(None));
        assert_eq!(resolve_label(" Latin1 "), Ok(Some(enc("windows-1252"))));
        assert_eq!(resolve_label("ansi"), resolve_ansi().map(Some));
        assert!(matches!(
            resolve_label("replacement"),
            Err(EncodingError::Replacement(_))
        ));
        assert!(matches!(
            resolve_label("klingon"),
            Err(EncodingError::UnknownLabel(_))
        ));
        assert!(parse_encoding_arg("klingon")
            .unwrap_err()
            .contains("unknown encoding label"));
    }

    #[test]
    fn first_matching_glob_wins_in_file_order() {
        let c = cfg(r#"
[encoding]
"legacy/**/*.pas" = "windows-1252"
"legacy/**" = "shift_jis"
"*.txt" = "auto"
"#)
        .unwrap();
        assert_eq!(c.lookup("legacy/a/b.pas"), Some(Some(enc("windows-1252"))));
        assert_eq!(c.lookup("legacy/b.pas"), Some(Some(enc("windows-1252"))));
        assert_eq!(c.lookup("legacy/b.cs"), Some(Some(enc("shift_jis"))));
        assert_eq!(c.lookup("a.txt"), Some(None));
        // `*` does not cross `/`.
        assert_eq!(c.lookup("sub/a.txt"), None);
        assert_eq!(c.resolve(None, "legacy/b.cs"), Some(enc("shift_jis")));
        assert_eq!(
            c.resolve(Some(enc("utf-16le")), "legacy/b.cs"),
            Some(enc("utf-16le"))
        );
        assert_eq!(c.resolve(None, "other.rs"), None);
    }

    #[test]
    fn errors_name_the_file_and_key() {
        let e = cfg("[encoding]\n\"*.pas\" = \"klingon\"\n")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains(".memory-graph.toml") && e.contains("*.pas") && e.contains("klingon"),
            "{e}"
        );
        let e = cfg("[encoding]\n\"*.pas\" = \"replacement\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("*.pas") && e.contains("replacement"), "{e}");
        let e = cfg("[encoding]\n\"*.pas\" = 3\n").unwrap_err().to_string();
        assert!(e.contains("*.pas") && e.contains("must be a string"), "{e}");
        let e = cfg("[encodings]\n").unwrap_err().to_string();
        assert!(e.contains("unknown key `encodings`"), "{e}");
        let e = cfg("encoding = 1\n").unwrap_err().to_string();
        assert!(e.contains("must be a table"), "{e}");
        let e = cfg("[encoding]\n\"a[\" = \"utf-8\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("a[") && e.contains("invalid glob"), "{e}");
        let e = cfg("[encoding\n").unwrap_err().to_string();
        assert!(e.contains(".memory-graph.toml"), "{e}");
    }

    #[test]
    fn a_missing_file_is_an_empty_config() {
        let d = std::env::temp_dir().join(format!("mg-enc-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let c = EncodingConfig::load(&d).unwrap();
        assert_eq!(c.lookup("x.rs"), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
