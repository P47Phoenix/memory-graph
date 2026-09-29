//! A larger hand-written fixture (no suitable permissively licensed shell
//! repo is vendored in the corpus).
use graph_core::{Extractor, SymbolKind};
use graph_lang_shell::ShellExtractor;

#[test]
fn deploy_script_symbols() {
    let src = include_str!("fixtures/deploy.sh");
    let ex = ShellExtractor.extract(src);
    for t in &ex.tokens {
        assert_eq!(t.text, src[t.span.start as usize..t.span.end as usize]);
    }
    let got: Vec<(&str, SymbolKind)> = ex
        .symbols
        .iter()
        .map(|s| (s.name.as_str(), s.kind))
        .collect();
    use SymbolKind::*;
    assert_eq!(
        got,
        [
            ("SCRIPT_DIR", Constant),
            ("DEPLOY_ENV", Variable),
            ("LOG_LEVEL", Variable),
            ("MAX_RETRIES", Constant),
            ("log", Function),
            ("die", Function),
            ("retry", Function),
            ("deploy_target", Function),
            ("cleanup", Function),
            ("main", Function),
            ("on_error", Function),
        ]
    );
    let text = |n: &str| {
        let s = ex.symbols.iter().find(|s| s.name == n).unwrap();
        &src[s.span.start as usize..s.span.end as usize]
    };
    assert!(text("deploy_target").ends_with("EOF\n}"));
    assert!(text("cleanup").ends_with("rm -rf ./tmp/*\n)"));
    assert!(text("main").ends_with("done\n}"));
}
