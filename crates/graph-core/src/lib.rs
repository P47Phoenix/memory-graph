//! Language-agnostic schema, `Extractor` trait and fallback tokenizer.
//! No storage or query dependencies; no language-specific types.
pub mod extractor;
pub mod language;
pub mod schema;
pub mod tokenizer;

pub use extractor::{Extraction, Extractor, FallbackExtractor, Registry, SymbolDecl};
pub use language::{detect_language, normalize_path};
pub use schema::*;
