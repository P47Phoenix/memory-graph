//! Pins the extractor versions bumped by PR #150 (#72, #127, #145): their
//! token or symbol output changed, so indexed files must be re-extracted.

#[test]
fn bumped_extractor_versions_are_pinned() {
    let extractors = graph_cli::shipped_extractors();
    for (lang, prefix) in [
        ("aspx", "aspx-scan-2+tok"),
        ("typescript", "typescript-scan-2+tok"),
        ("python", "python-scan-2+tok"),
        ("gdscript", "gdscript-scan-2+tok"),
        ("r", "r-scan-2+tok"),
        ("sql", "sql-scan-2+tok"),
        ("shell", "shell-scan-2+tok"),
        ("rpg", "rpg-scan-2+tok"),
    ] {
        let e = extractors
            .iter()
            .find(|e| e.language() == lang)
            .unwrap_or_else(|| panic!("no {lang} extractor"));
        assert!(e.version().starts_with(prefix), "{lang}: {}", e.version());
    }
}
