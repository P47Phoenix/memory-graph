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
        "" => "unknown",
        other => return other.to_string(),
    }
    .to_string()
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
    fn languages() {
        assert_eq!(detect_language("A.RS"), "rust");
        assert_eq!(detect_language("x.zig"), "zig");
        assert_eq!(detect_language("Makefile"), "unknown");
    }
}
