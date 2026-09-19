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
pub trait Extractor {
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
#[derive(Default)]
pub struct Registry {
    extractors: std::collections::HashMap<String, Box<dyn Extractor>>,
}

impl Registry {
    pub fn register(&mut self, e: Box<dyn Extractor>) {
        self.extractors.insert(e.language().to_ascii_lowercase(), e);
    }

    /// Extract with the registered extractor for `language`, else the fallback.
    pub fn extract(&self, language: &str, source: &str) -> Extraction {
        match self.extractors.get(language) {
            Some(e) => e.extract(source),
            None => FallbackExtractor::new(language).extract(source),
        }
    }

    /// Version of the extractor that `extract` would use for `language`.
    pub fn version(&self, language: &str) -> String {
        match self.extractors.get(language) {
            Some(e) => e.version(),
            None => fallback_version(),
        }
    }

    pub fn has(&self, language: &str) -> bool {
        self.extractors.contains_key(language)
    }
}
