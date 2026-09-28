//! Extractors shared between a store and its reopenings, and the extractor
//! version set hash (ADR 0004 D5).
//!
//! `Registry::register` takes a `Box<dyn Extractor>` and a `Box` is not
//! `Clone`, but a server reopens its store (compact, snapshot install, a
//! test server restart) with the same extractors, so it keeps them as
//! `Arc<dyn Extractor>` and registers a [`SharedExtractor`] that delegates.
use graph_core::{Extraction, Extractor};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// An `Extractor` that delegates to a shared one.
pub struct SharedExtractor(pub Arc<dyn Extractor>);

impl Extractor for SharedExtractor {
    fn language(&self) -> &str {
        self.0.language()
    }
    fn extract(&self, source: &str) -> Extraction {
        self.0.extract(source)
    }
    fn version(&self) -> String {
        self.0.version()
    }
    fn extensions(&self) -> &[&str] {
        self.0.extensions()
    }
}

/// `Box<dyn Extractor>` -> `Arc<dyn Extractor>`.
pub fn share(extractors: Vec<Box<dyn Extractor>>) -> Vec<Arc<dyn Extractor>> {
    extractors.into_iter().map(Arc::from).collect()
}

/// Register shared extractors on a store.
pub fn register_all(store: &mut graph_store::V2Store, extractors: &[Arc<dyn Extractor>]) {
    for e in extractors {
        store.register(Box::new(SharedExtractor(Arc::clone(e))));
    }
}

/// SHA-256 (hex) over the sorted `language\0version\0ext,ext...` lines of
/// the extractor set, plus the store format and fingerprint versions: two
/// nodes with the same hash extract identically, so their stores stay
/// query-identical (ADR 0004 D5). Order-independent.
pub fn extractors_hash(extractors: &[Arc<dyn Extractor>]) -> String {
    let mut lines: Vec<String> = extractors
        .iter()
        .map(|e| {
            let mut ext: Vec<&str> = e.extensions().to_vec();
            ext.sort_unstable();
            format!("{}\0{}\0{}", e.language(), e.version(), ext.join(","))
        })
        .collect();
    lines.sort_unstable();
    let mut h = Sha256::new();
    h.update(format!(
        "format={}\0fingerprint={}\0tokenizer={}\n",
        graph_store::SCHEMA_VERSION,
        graph_store::FINGERPRINT_FORMAT_VERSION,
        graph_core::tokenizer::TOKENIZER_VERSION
    ));
    for l in lines {
        h.update(l);
        h.update(b"\n");
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Toy(&'static str, &'static str);
    impl Extractor for Toy {
        fn language(&self) -> &str {
            self.0
        }
        fn extract(&self, _: &str) -> Extraction {
            Extraction::default()
        }
        fn version(&self) -> String {
            self.1.into()
        }
    }

    #[test]
    fn hash_is_order_independent_and_version_sensitive() {
        let a: Vec<Arc<dyn Extractor>> = vec![Arc::new(Toy("a", "1")), Arc::new(Toy("b", "1"))];
        let b: Vec<Arc<dyn Extractor>> = vec![Arc::new(Toy("b", "1")), Arc::new(Toy("a", "1"))];
        let c: Vec<Arc<dyn Extractor>> = vec![Arc::new(Toy("a", "2")), Arc::new(Toy("b", "1"))];
        assert_eq!(extractors_hash(&a), extractors_hash(&b));
        assert_ne!(extractors_hash(&a), extractors_hash(&c));
        assert_ne!(extractors_hash(&a), extractors_hash(&[]));
        assert_eq!(extractors_hash(&a).len(), 64);
    }
}
