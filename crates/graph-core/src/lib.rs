//! Language-agnostic schema, `Extractor` trait and fallback tokenizer.
//! No storage or query dependencies; no language-specific types.
pub mod encoding;
pub mod extractor;
pub mod language;
pub mod scan;
pub mod schema;
pub mod scratch;
pub mod tokenizer;

pub use extractor::{
    Extraction, Extractor, FallbackExtractor, Registry, SymbolDecl, EXTRACT_STACK_BYTES,
    FALLBACK_EXTRACTOR_VERSION,
};
pub use language::{detect_language, detect_language_from_content, normalize_path};
pub use schema::*;
pub use scratch::{reserve_scratch, with_scratch_budget, ScratchBudget, ScratchReservation};
