//! Language-agnostic schema, `Extractor` trait and fallback tokenizer.
//! No storage or query dependencies; no language-specific types.
pub mod extractor;
pub mod language;
pub mod scan;
pub mod schema;
pub mod tokenizer;

pub use extractor::{
    Extraction, Extractor, FallbackExtractor, Registry, SymbolDecl, FALLBACK_EXTRACTOR_VERSION,
};
pub use language::{detect_language, detect_language_from_content, normalize_path};
pub use schema::*;
