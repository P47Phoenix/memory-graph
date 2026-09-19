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
        }
    }
}
