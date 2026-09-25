//! The third-party path end to end: register an out-of-tree extractor through
//! `open_store`, index files by extension, and query its symbols.
use graph_store::{open_store, SymbolQuery};
use toy_extractor::IniExtractor;

#[test]
fn registered_extractor_is_indexed_and_queryable() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir.path().join("g.redb"), vec![Box::new(IniExtractor)]).unwrap();
    store
        .index_bytes(
            "o",
            "r",
            "conf/app.cfg",
            b"[server]\nhost = x\nport = 80\n",
            None,
        )
        .unwrap();
    let hits = store.search_symbols(&SymbolQuery::new("port")).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].language.as_deref(), Some("ini"));
    assert_eq!(hits[0].qualified, "server::port");
    let info = store.describe(None, None).unwrap();
    let kinds = &info[0].languages["ini"].symbol_kinds;
    assert_eq!(kinds.get("module/section"), Some(&1));
    assert_eq!(kinds.get("variable/key"), Some(&2));
}
