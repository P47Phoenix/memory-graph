//! Pins the extractor versions bumped by PR #150 (#72, #127, #145): their
//! token or symbol output changed, so indexed files must be re-extracted.
//! `+kw1` (#143): reserved words are classed `keyword`.

#[test]
fn bumped_extractor_versions_are_pinned() {
    let extractors = graph_cli::shipped_extractors();
    for (lang, prefix) in [
        ("aspx", "aspx-scan-2+kw1+tok"),
        ("cobol", "cobol-scan-1+kw1+tok"),
        ("asm", "asm-scan-1+kw1+tok"),
        ("typescript", "typescript-scan-2+kw1+tok"),
        ("python", "python-scan-2+kw1+tok"),
        ("csharp", "csharp-scan-1+kw1+tok"),
        ("javascript", "javascript-scan-1+kw1+tok"),
        ("java", "java-scan-1+kw1+tok"),
        ("go", "go-scan-2+kw1+tok"),
        ("c", "c-scan-1+kw1+tok"),
        ("cpp", "cpp-scan-1+kw1+tok"),
        ("scala", "scala-scan-1+kw1+tok"),
        ("haskell", "haskell-scan-1+kw1+tok"),
        ("fsharp", "fsharp-scan-1+kw1+tok"),
        ("gdscript", "gdscript-scan-2+kw1+tok"),
        ("r", "r-scan-2+kw1+tok"),
        ("sql", "sql-scan-2+kw1+tok"),
        ("shell", "shell-scan-2+kw1+tok"),
        ("elixir", "elixir-scan-1+kw1+tok"),
        ("rpg", "rpg-scan-2+kw1+tok"),
    ] {
        let e = extractors
            .iter()
            .find(|e| e.language() == lang)
            .unwrap_or_else(|| panic!("no {lang} extractor"));
        assert!(e.version().starts_with(prefix), "{lang}: {}", e.version());
    }
}
