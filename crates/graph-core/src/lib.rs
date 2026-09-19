//! Language-agnostic schema, `Extractor` trait and fallback tokenizer.
//! No storage or query dependencies; no language-specific types.
pub mod extractor;
pub mod schema;
pub mod tokenizer;

pub use extractor::{Extraction, Extractor, FallbackExtractor, SymbolDecl};
pub use schema::*;
