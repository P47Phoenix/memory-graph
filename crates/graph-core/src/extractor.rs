use crate::schema::{Span, SymbolKind, TokenDecl};

/// A symbol declared by an extractor. Parents are derived from span
/// containment, so extractors need not track hierarchy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolDecl {
    pub name: String,
    pub kind: SymbolKind,
    pub lang_kind: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extraction {
    pub symbols: Vec<SymbolDecl>,
    pub tokens: Vec<TokenDecl>,
    /// The source had syntax errors; the extractor fell back to tokens only.
    pub has_errors: bool,
}

/// Language support plugs in here. Implementations use only schema types.
///
/// This trait, [`SymbolDecl`], [`Extraction`], `TokenDecl` and `Span` are the
/// plugin API for third-party languages; see `docs/adding-a-language.md`.
pub trait Extractor: Send + Sync {
    /// Language string stored on File nodes.
    fn language(&self) -> &str;
    fn extract(&self, source: &str) -> Extraction;
    /// Version of this extractor's output. Bump it whenever a change would
    /// alter what `extract` returns, so stored files are re-indexed.
    ///
    /// Extractors that use the shared tokenizer should include
    /// `tokenizer::TOKENIZER_VERSION` in it.
    fn version(&self) -> String {
        "1".to_string()
    }
    /// File extensions (without the dot, any case) this extractor claims.
    /// [`Registry::detect_language`] maps them to [`Extractor::language`]
    /// before anything else (the built-in extension table, well-known file
    /// names, the `#!` line), so an extractor can add a language the table
    /// does not know, or take over an extension the table maps elsewhere.
    ///
    /// An extension is the text after the file name's last dot, as
    /// `std::path::Path::extension` defines it: a claim such as `"d.ts"` can
    /// never match (and is ignored), and a dotfile such as `.ini` has no
    /// extension, so it is not matched by an `"ini"` claim.
    fn extensions(&self) -> &[&str] {
        &[]
    }
}

/// Base version of the fallback (tokenizer-only) extraction; its `version()`
/// also carries `tokenizer::TOKENIZER_VERSION`.
pub const FALLBACK_EXTRACTOR_VERSION: &str = "fallback-1";

fn fallback_version() -> String {
    format!(
        "{FALLBACK_EXTRACTOR_VERSION}+tok{}",
        crate::tokenizer::TOKENIZER_VERSION
    )
}

/// Fallback: tokens only, no symbols. Works for any language.
pub struct FallbackExtractor {
    language: String,
}

impl FallbackExtractor {
    pub fn new(language: impl Into<String>) -> Self {
        Self {
            language: language.into(),
        }
    }
}

impl Extractor for FallbackExtractor {
    fn language(&self) -> &str {
        &self.language
    }
    fn version(&self) -> String {
        fallback_version()
    }
    fn extract(&self, source: &str) -> Extraction {
        Extraction {
            symbols: vec![],
            tokens: crate::tokenizer::tokenize(source),
            has_errors: false,
        }
    }
}

/// Maps language names to extractors; unknown languages use the fallback.
///
/// Language names and extensions are case-insensitive. When two extractors
/// register the same language or claim the same extension, the last
/// registration wins, so a caller can override a built-in extractor.
#[derive(Default)]
pub struct Registry {
    extractors: std::collections::HashMap<String, Box<dyn Extractor>>,
    /// Lowercased extension -> lowercased language.
    extensions: std::collections::HashMap<String, String>,
}

impl Registry {
    pub fn register(&mut self, e: Box<dyn Extractor>) {
        let lang = e.language().to_ascii_lowercase();
        for ext in e.extensions() {
            // Only single-segment extensions can ever match (see
            // `Extractor::extensions`); drop empty and dotted claims.
            let ext = ext.strip_prefix('.').unwrap_or(ext).to_ascii_lowercase();
            if !ext.is_empty() && !ext.contains('.') {
                self.extensions.insert(ext, lang.clone());
            }
        }
        self.extractors.insert(lang, e);
    }

    fn get(&self, language: &str) -> Option<&dyn Extractor> {
        self.extractors
            .get(&language.to_ascii_lowercase())
            .map(|e| &**e)
    }

    /// Extract with the registered extractor for `language`, else the fallback.
    pub fn extract(&self, language: &str, source: &str) -> Extraction {
        match self.get(language) {
            Some(e) => e.extract(source),
            None => FallbackExtractor::new(language.to_ascii_lowercase()).extract(source),
        }
    }

    /// Version of the extractor that `extract` would use for `language`.
    pub fn version(&self, language: &str) -> String {
        match self.get(language) {
            Some(e) => e.version(),
            None => fallback_version(),
        }
    }

    pub fn has(&self, language: &str) -> bool {
        self.get(language).is_some()
    }

    /// Language for a file: an extension claimed by a registered extractor
    /// first, then [`crate::detect_language_from_content`] (built-in extension
    /// table, well-known file names, `#!` line).
    pub fn detect_language(&self, path: &str, src: &str) -> String {
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        if let Some(lang) = ext.and_then(|e| self.extensions.get(&e)) {
            return lang.clone();
        }
        crate::language::detect_language_from_content(path, src)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Toy(&'static str, &'static [&'static str]);
    impl Extractor for Toy {
        fn language(&self) -> &str {
            self.0
        }
        fn extensions(&self) -> &[&str] {
            self.1
        }
        fn version(&self) -> String {
            format!("toy-{}", self.0)
        }
        fn extract(&self, source: &str) -> Extraction {
            Extraction {
                has_errors: true,
                ..FallbackExtractor::new(self.0).extract(source)
            }
        }
    }

    #[test]
    fn claimed_extensions_win_then_builtin_table() {
        let mut r = Registry::default();
        r.register(Box::new(Toy("Toy", &["TOY", ".tk"])));
        assert_eq!(r.detect_language("a/b.toy", ""), "toy");
        assert_eq!(r.detect_language("a/b.TK", ""), "toy");
        assert_eq!(r.detect_language("a/b.rs", ""), "rust");
        assert_eq!(r.detect_language("x", "#!/bin/sh\n"), "shell");
        assert_eq!(r.detect_language("Makefile", ""), "make");
        // A claim overrides the built-in table.
        r.register(Box::new(Toy("myrust", &["rs"])));
        assert_eq!(r.detect_language("a.rs", ""), "myrust");
    }

    #[test]
    fn last_registration_wins() {
        let mut r = Registry::default();
        r.register(Box::new(Toy("a", &["x"])));
        r.register(Box::new(Toy("b", &["x"])));
        assert_eq!(r.detect_language("f.x", ""), "b");
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let mut r = Registry::default();
        r.register(Box::new(Toy("Toy", &[])));
        assert!(r.has("toy") && r.has("TOY"));
        assert_eq!(r.version("ToY"), "toy-Toy");
        assert!(!r.has("other"));
        assert_eq!(r.version("other"), fallback_version());
        // `extract` uses the registered extractor whatever the case (the toy
        // marks its output with `has_errors`).
        assert!(r.extract("TOY", "x").has_errors);
        assert!(!r.extract("Other", "x").has_errors);
    }

    #[test]
    fn empty_and_dotted_claims_are_ignored() {
        let mut r = Registry::default();
        r.register(Box::new(Toy("toy", &["", ".", "d.ts", "..x"])));
        assert!(r.extensions.is_empty());
        assert_eq!(r.detect_language("a.d.ts", ""), "typescript");
        assert_eq!(r.detect_language("a.", ""), "unknown");
        // A dotfile has no extension, so it is never claimed.
        r.register(Box::new(Toy("ini", &["ini"])));
        assert_eq!(r.detect_language("x/.ini", ""), "unknown");
        assert_eq!(r.detect_language("x/a.ini", ""), "ini");
    }

    proptest::proptest! {
        /// With nothing claimed, detection is exactly the free function's, so
        /// existing databases keep their languages and fingerprints.
        #[test]
        fn unclaimed_detection_matches_free_function(
            path in "[a-zA-Z./_é-]{0,16}",
            src in "(#!/usr/bin/env (python3|node|bash) -u\n)?[a-z ]{0,8}",
        ) {
            let mut r = Registry::default();
            r.register(Box::new(Toy("toy", &["toy"])));
            if !path.to_ascii_lowercase().ends_with(".toy") {
                proptest::prop_assert_eq!(
                    r.detect_language(&path, &src),
                    crate::language::detect_language_from_content(&path, &src)
                );
            }
        }
    }
}
