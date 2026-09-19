//! Path helpers shared by the CLI and the library so both build the same graph.

/// Lowercased language name from a file extension; unknown extensions use the
/// extension itself, and no extension gives `unknown`.
pub fn detect_language(path: &str) -> String {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
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
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
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
        "bash" | "sh" | "zsh" | "dash" => "shell".into(),
        other => other.to_string(),
    }
}

/// Normalize a path for use as file identity: no `.`
/// components, `..` resolved where possible. `a.rs`, `./a.rs` and `x/../a.rs`
/// are the same file.
pub fn normalize_path(path: &str) -> String {
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
    }

    #[test]
    fn content_detection() {
        let d = detect_language_from_content;
        assert_eq!(d("tool", "#!/usr/bin/env python3\nprint(1)"), "python");
        assert_eq!(d("run", "#!/bin/bash\n"), "shell");
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
