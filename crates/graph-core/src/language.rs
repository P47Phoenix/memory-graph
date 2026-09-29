//! Path helpers shared by the CLI and the library so both build the same graph.

/// Lowercased language name from a file extension; unknown extensions use the
/// extension itself, and no extension gives `unknown`.
pub fn detect_language(path: &str) -> String {
    let ext = extension(path).unwrap_or_default();
    match ext.as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "tsx" => "typescript",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "hpp" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "cs" => "csharp",
        "kt" | "kts" => "kotlin",
        "yml" => "yaml",
        "sh" | "bash" | "zsh" => "shell",
        "" => "unknown",
        other => return other.to_string(),
    }
    .to_string()
}

/// Detect a language from the path and, when the path is inconclusive, the
/// content (a `#!` interpreter line). Never needs to be told the language.
pub fn detect_language_from_content(path: &str, src: &str) -> String {
    let by_path = detect_language(path);
    let name = file_name(path);
    match name.to_ascii_lowercase().as_str() {
        "makefile" | "gnumakefile" => return "make".into(),
        "dockerfile" => return "dockerfile".into(),
        _ => {}
    }
    if by_path != "unknown" {
        return by_path;
    }
    let first = src
        .trim_start_matches('\u{feff}')
        .lines()
        .next()
        .unwrap_or("");
    let Some(shebang) = first.strip_prefix("#!") else {
        return by_path;
    };
    // `#!/usr/bin/env python3 -u` or `#!/bin/bash`: take the interpreter's name.
    let mut parts = shebang.split_whitespace();
    let mut prog = parts.next().unwrap_or("").rsplit('/').next().unwrap_or("");
    if prog == "env" {
        prog = parts.find(|a| !a.starts_with('-')).unwrap_or("");
    }
    let prog = prog.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    match prog {
        "" => by_path,
        "python" => "python".into(),
        "node" | "nodejs" => "javascript".into(),
        "bash" | "sh" | "zsh" | "dash" | "ksh" | "mksh" => "shell".into(),
        "Rscript" => "r".into(),
        other => other.to_string(),
    }
}

/// The last component of `path`, splitting on both `/` and `\` whatever the
/// host OS, so a path sent by a Windows client (`dir\.gitignore`) is read the
/// same way on a Linux server (#120).
pub fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Lowercased extension of [`file_name`]`(path)`, with the same rules as
/// [`std::path::Path::extension`] (`.gitignore` has none).
pub fn extension(path: &str) -> Option<String> {
    std::path::Path::new(file_name(path))
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

/// Normalize a path for use as file identity: `/`-separated on every OS
/// (`\` is read as a separator, so `src\a.rs` is `src/a.rs`), no `.`
/// components, `..` resolved where possible. `a.rs`, `./a.rs` and `x/../a.rs`
/// are the same file.
///
/// Because `\` is a separator on every OS, a Unix file literally named
/// `a\b` maps to `a/b` (the directory walk skips such names, visibly, rather
/// than let them collide). Other shapes, which are not repo-relative and are
/// kept only so they stay distinct: a drive path `C:\x\a.rs` becomes
/// `C:/x/a.rs` (relative, `C:` is an ordinary component); a UNC path
/// `\\srv\share\a.rs` becomes `/srv/share/a.rs` (absolute, the doubled
/// separator collapses); a leading `..` is kept on a relative path
/// (`../a.rs`) and dropped at the root of an absolute one (`/../a.rs` is
/// `/a.rs`).
pub fn normalize_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let abs = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                if matches!(parts.last(), Some(p) if *p != "..") {
                    parts.pop();
                } else if !abs {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    format!("{}{}", if abs { "/" } else { "" }, parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(normalize_path("./a.rs"), "a.rs");
        assert_eq!(normalize_path("x/../a.rs"), "a.rs");
        assert_eq!(normalize_path("/tmp//a/./b.rs"), "/tmp/a/b.rs");
        assert_eq!(normalize_path("../a.rs"), "../a.rs");
        assert_eq!(normalize_path(r"src\lib.rs"), "src/lib.rs");
        assert_eq!(normalize_path(r".\a\..\b/c.rs"), "b/c.rs");
        assert_eq!(normalize_path(r"a\b"), "a/b");
        assert_eq!(normalize_path(r"C:\x\a.rs"), "C:/x/a.rs");
        assert_eq!(normalize_path(r"\\srv\share\a.rs"), "/srv/share/a.rs");
        assert_eq!(normalize_path(r"..\a.rs"), "../a.rs");
        assert_eq!(normalize_path("/../a.rs"), "/a.rs");
    }

    #[test]
    fn both_separators_detect_the_same_language() {
        // #120: a Windows client's path read on any host.
        assert_eq!(detect_language(r"dir\.gitignore"), "unknown");
        assert_eq!(detect_language("dir/.gitignore"), "unknown");
        assert_eq!(detect_language(r"a.b\c"), "unknown");
        assert_eq!(detect_language(r"x\lib.RS"), "rust");
        assert_eq!(detect_language_from_content(r"d\Makefile", ""), "make");
        assert_eq!(file_name(r"a\b/c"), "c");
    }

    #[test]
    fn content_detection() {
        let d = detect_language_from_content;
        assert_eq!(d("tool", "#!/usr/bin/env python3\nprint(1)"), "python");
        assert_eq!(d("run", "#!/bin/bash\n"), "shell");
        assert_eq!(d("run", "#!/bin/ksh\n"), "shell");
        assert_eq!(d("run", "#!/usr/bin/env Rscript\n"), "r");
        assert_eq!(d("x", "#!/usr/bin/env -S node --flag\n"), "javascript");
        assert_eq!(d("Makefile", ""), "make");
        assert_eq!(d("a.rs", "#!/usr/bin/env python"), "rust"); // extension wins
        assert_eq!(d("plain", "hello"), "unknown");
    }

    #[test]
    fn languages() {
        assert_eq!(detect_language("A.RS"), "rust");
        assert_eq!(detect_language("x.zig"), "zig");
        assert_eq!(detect_language("Makefile"), "unknown");
    }
}
