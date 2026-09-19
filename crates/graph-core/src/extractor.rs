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

    pub fn has(&self, language: &str) -> bool {
        self.extractors.contains_key(language)
    }
}
