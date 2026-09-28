//! Property tests: every converted type round-trips through its message
//! (and through prost bytes), `StoreError` / `WireError` round-trip through
//! `tonic::Status`, and prost's decoder never panics on arbitrary bytes.
use crate::error::{status_to_store_error, store_error_to_status};
use crate::{pb, ConvertError, View, WireError};
use graph_core::{
    Extraction, Node, NodeKind, SchemaError, Span, SymbolDecl, SymbolKind, TokenClass, TokenDecl,
};
use graph_store::{
    CompactStats, Grain, Hit, IndexOptions, IngestStats, LanguageInfo, Page, Query, RepoInfo,
    SnapshotStats, StoreError, SymbolHit, SymbolQuery, VacuumStats,
};
use proptest::collection::{btree_map, vec};
use proptest::option;
use proptest::prelude::*;
use prost::Message;
use std::collections::BTreeMap;
use std::time::Duration;
use tonic::{Code, Status};

// ------------------------------------------------------------ strategies --

fn text() -> impl Strategy<Value = String> {
    // Anything valid UTF-8, including empty, spaces and non-ASCII.
    "[a-zA-Z0-9 _./:*\\-éß€]{0,12}"
}

fn node_kind() -> impl Strategy<Value = NodeKind> {
    prop_oneof![
        Just(NodeKind::Org),
        Just(NodeKind::Repo),
        Just(NodeKind::File),
        Just(NodeKind::Symbol),
        Just(NodeKind::Token),
    ]
}

fn symbol_kind() -> impl Strategy<Value = SymbolKind> {
    prop_oneof![
        Just(SymbolKind::Module),
        Just(SymbolKind::Type),
        Just(SymbolKind::Function),
        Just(SymbolKind::Method),
        Just(SymbolKind::Variable),
        Just(SymbolKind::Constant),
        Just(SymbolKind::Other),
    ]
}

fn token_class() -> impl Strategy<Value = TokenClass> {
    prop_oneof![
        Just(TokenClass::Identifier),
        Just(TokenClass::Keyword),
        Just(TokenClass::Literal),
        Just(TokenClass::Operator),
        Just(TokenClass::Punctuation),
        Just(TokenClass::Comment),
        Just(TokenClass::Other),
    ]
}

fn grain() -> impl Strategy<Value = Grain> {
    prop_oneof![
        Just(Grain::Token),
        Just(Grain::Symbol),
        Just(Grain::Method),
        Just(Grain::Class),
        Just(Grain::File),
        Just(Grain::Repo),
        Just(Grain::Org),
    ]
}

fn span() -> impl Strategy<Value = Span> {
    (
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
    )
        .prop_map(
            |(start, end, start_line, start_col, end_line, end_col)| Span {
                start,
                end,
                start_line,
                start_col,
                end_line,
                end_col,
            },
        )
}

fn node() -> impl Strategy<Value = Node> {
    (
        any::<u64>(),
        option::of(any::<u64>()),
        node_kind(),
        text(),
        option::of(text()),
        option::of(symbol_kind()),
        option::of(text()),
        option::of(token_class()),
        any::<bool>(),
        option::of(text()),
        option::of(text()),
        option::of(span()),
    )
        .prop_map(
            |(
                id,
                parent,
                kind,
                name,
                language,
                symbol_kind,
                lang_kind,
                token_class,
                has_errors,
                origin,
                fingerprint,
                span,
            )| Node {
                id,
                parent,
                kind,
                name,
                language,
                symbol_kind,
                lang_kind,
                token_class,
                has_errors,
                origin,
                fingerprint,
                span,
            },
        )
}

fn query() -> impl Strategy<Value = Query> {
    (
        text(),
        option::of(text()),
        option::of(text()),
        option::of(text()),
        option::of(token_class()),
        grain(),
        option::of(text()),
        option::of(any::<usize>()),
        option::of(any::<usize>()),
    )
        .prop_map(
            |(text, language, org, repo, class, grain, symbol_kind, limit, offset)| Query {
                text,
                language,
                org,
                repo,
                class,
                grain,
                symbol_kind,
                limit,
                offset,
            },
        )
}

fn symbol_query() -> impl Strategy<Value = SymbolQuery> {
    (
        text(),
        option::of(text()),
        option::of(text()),
        option::of(text()),
        option::of(text()),
        option::of(text()),
        option::of(any::<usize>()),
        option::of(any::<usize>()),
    )
        .prop_map(
            |(pattern, kind, language, org, repo, file, limit, offset)| SymbolQuery {
                pattern,
                kind,
                language,
                org,
                repo,
                file,
                limit,
                offset,
            },
        )
}

fn hit() -> impl Strategy<Value = Hit> {
    (
        (
            grain(),
            text(),
            option::of(text()),
            option::of(text()),
            option::of(text()),
            option::of(text()),
            option::of(symbol_kind()),
        ),
        (
            option::of(text()),
            option::of(token_class()),
            option::of(span()),
            any::<usize>(),
            any::<bool>(),
            any::<bool>(),
        ),
    )
        .prop_map(
            |(
                (grain, org, repo, file, language, symbol, symbol_kind),
                (lang_kind, token_class, span, count, no_symbols, no_matching_symbol),
            )| Hit {
                grain,
                org,
                repo,
                file,
                language,
                symbol,
                symbol_kind,
                lang_kind,
                token_class,
                span,
                count,
                no_symbols,
                no_matching_symbol,
            },
        )
}

fn symbol_hit() -> impl Strategy<Value = SymbolHit> {
    (
        text(),
        text(),
        text(),
        option::of(text()),
        text(),
        text(),
        symbol_kind(),
        option::of(text()),
        option::of(span()),
    )
        .prop_map(
            |(org, repo, file, language, name, qualified, kind, lang_kind, span)| SymbolHit {
                org,
                repo,
                file,
                language,
                name,
                qualified,
                kind,
                lang_kind,
                span,
            },
        )
}

fn counts() -> impl Strategy<Value = BTreeMap<String, usize>> {
    btree_map(text(), any::<usize>(), 0..4)
}

fn language_info() -> impl Strategy<Value = LanguageInfo> {
    (any::<usize>(), any::<usize>(), any::<usize>(), counts()).prop_map(
        |(files, symbols, tokens, symbol_kinds)| LanguageInfo {
            files,
            symbols,
            tokens,
            symbol_kinds,
        },
    )
}

fn repo_info() -> impl Strategy<Value = RepoInfo> {
    (
        text(),
        text(),
        any::<usize>(),
        btree_map(text(), language_info(), 0..3),
        counts(),
        any::<bool>(),
    )
        .prop_map(
            |(org, repo, files, languages, token_classes, open_batch)| RepoInfo {
                org,
                repo,
                files,
                languages,
                token_classes,
                open_batch,
            },
        )
}

fn ingest_stats() -> impl Strategy<Value = IngestStats> {
    (
        any::<u64>(),
        any::<usize>(),
        any::<usize>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        text(),
        text(),
    )
        .prop_map(
            |(file_id, symbols, tokens, replaced, unchanged, has_errors, path, language)| {
                IngestStats {
                    file_id,
                    symbols,
                    tokens,
                    replaced,
                    unchanged,
                    has_errors,
                    path,
                    language,
                }
            },
        )
}

fn extraction() -> impl Strategy<Value = Extraction> {
    (
        vec(
            (text(), symbol_kind(), option::of(text()), span()).prop_map(
                |(name, kind, lang_kind, span)| SymbolDecl {
                    name,
                    kind,
                    lang_kind,
                    span,
                },
            ),
            0..4,
        ),
        vec(
            (text(), token_class(), span()).prop_map(|(text, class, span)| TokenDecl {
                text,
                class,
                span,
            }),
            0..6,
        ),
        any::<bool>(),
    )
        .prop_map(|(symbols, tokens, has_errors)| Extraction {
            symbols,
            tokens,
            has_errors,
        })
}

fn snapshot_stats() -> impl Strategy<Value = SnapshotStats> {
    // Whole milliseconds: the wire carries ms.
    (any::<usize>(), option::of(any::<u64>()), any::<u64>()).prop_map(
        |(open_count, ms, store_size_bytes)| SnapshotStats {
            open_count,
            oldest_age: ms.map(Duration::from_millis),
            store_size_bytes,
        },
    )
}

fn store_error() -> impl Strategy<Value = StoreError> {
    prop_oneof![
        text().prop_map(StoreError::Locked),
        any::<u64>().prop_map(|found| StoreError::SchemaMismatch { found }),
        (text(), any::<u64>())
            .prop_map(|(path, version)| StoreError::LegacyFormat { path, version }),
        (text(), text()).prop_map(|(path, reason)| StoreError::OpenFailed { path, reason }),
        text().prop_map(StoreError::Rejected),
        text().prop_map(StoreError::NotUtf8),
        text().prop_map(StoreError::TooLarge),
        text().prop_map(StoreError::InvalidSpan),
        text().prop_map(StoreError::Corrupt),
        (node_kind(), node_kind())
            .prop_map(|(p, c)| StoreError::Schema(SchemaError::InvalidContainment(p, c))),
        text().prop_map(StoreError::Storage),
        any::<bool>().prop_map(|_| StoreError::Storage(
            "redb: No space left on device (os error 28)".into()
        )),
        any::<bool>().prop_map(|_| StoreError::Storage(
            "There is not enough space on the disk. (os error 112)".into()
        )),
        (any::<u64>(), any::<u64>()).prop_map(|(age_secs, max_age_secs)| {
            StoreError::SnapshotExpired {
                age_secs,
                max_age_secs,
            }
        }),
        (option::of(any::<u64>()), option::of(text())).prop_map(|(leader_id, leader_addr)| {
            StoreError::NotLeader {
                leader_id,
                leader_addr,
            }
        }),
        any::<u64>().prop_map(|retry_after_ms| StoreError::NoLeader { retry_after_ms }),
        text().prop_map(StoreError::Protocol),
    ]
}

fn wire_error() -> impl Strategy<Value = WireError> {
    prop_oneof![
        store_error().prop_map(WireError::from),
        (option::of(any::<u64>()), option::of(text())).prop_map(|(leader_id, leader_addr)| {
            WireError::NotLeader {
                leader_id,
                leader_addr,
            }
        }),
        any::<u64>().prop_map(|retry_after_ms| WireError::NoLeader { retry_after_ms }),
        text().prop_map(WireError::Protocol),
    ]
}

// ------------------------------------------------------------ round trips --

/// `T -> M -> bytes -> M -> T` equals `T`; `M -> T -> M` equals `M`.
fn round_trip<T, M>(value: T) -> Result<(), TestCaseError>
where
    T: Clone + PartialEq + std::fmt::Debug + Into<M> + TryFrom<M, Error = ConvertError>,
    M: Message + Default + Clone + PartialEq + std::fmt::Debug,
{
    let msg: M = value.clone().into();
    let bytes = msg.encode_to_vec();
    let decoded = M::decode(bytes.as_slice()).map_err(|e| TestCaseError::fail(e.to_string()))?;
    prop_assert_eq!(&decoded, &msg);
    let back = T::try_from(decoded).map_err(|e| TestCaseError::fail(e.to_string()))?;
    prop_assert_eq!(&back, &value);
    let again: M = back.into();
    prop_assert_eq!(again, msg);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn span_round_trips(s in span()) { round_trip::<Span, pb::Span>(s)?; }

    #[test]
    fn node_round_trips(n in node()) { round_trip::<Node, pb::Node>(n)?; }

    #[test]
    fn node_page_round_trips(items in vec(node(), 0..4), has_more in any::<bool>()) {
        round_trip::<Page<Node>, pb::NodePage>(Page { items, has_more })?;
    }

    #[test]
    fn node_batch_round_trips(nodes in vec(node(), 0..4)) {
        round_trip::<Vec<Node>, pb::NodeBatch>(nodes)?;
    }

    #[test]
    fn query_round_trips(q in query()) {
        // `Query` has no `PartialEq`: compare its JSON form.
        let msg: pb::Query = q.clone().into();
        let back = Query::try_from(pb::Query::decode(msg.encode_to_vec().as_slice()).unwrap()).unwrap();
        prop_assert_eq!(serde_json_str(&back), serde_json_str(&q));
        prop_assert_eq!(pb::Query::from(back), msg);
    }

    #[test]
    fn symbol_query_round_trips(q in symbol_query()) {
        let msg: pb::SymbolQuery = q.clone().into();
        let back = SymbolQuery::try_from(pb::SymbolQuery::decode(msg.encode_to_vec().as_slice()).unwrap()).unwrap();
        prop_assert_eq!(serde_json_str(&back), serde_json_str(&q));
        prop_assert_eq!(pb::SymbolQuery::from(back), msg);
    }

    #[test]
    fn hit_round_trips(h in hit()) { round_trip::<Hit, pb::Hit>(h)?; }

    #[test]
    fn symbol_hit_round_trips(h in symbol_hit()) { round_trip::<SymbolHit, pb::SymbolHit>(h)?; }

    #[test]
    fn language_info_round_trips(l in language_info()) { round_trip::<LanguageInfo, pb::LanguageInfo>(l)?; }

    #[test]
    fn repo_info_round_trips(r in repo_info()) { round_trip::<RepoInfo, pb::RepoInfo>(r)?; }

    #[test]
    fn ingest_stats_round_trips(s in ingest_stats()) { round_trip::<IngestStats, pb::IngestStats>(s)?; }

    #[test]
    fn extraction_round_trips(e in extraction()) { round_trip::<Extraction, pb::Extraction>(e)?; }

    #[test]
    fn snapshot_stats_round_trips(s in snapshot_stats()) { round_trip::<SnapshotStats, pb::SnapshotStats>(s)?; }

    #[test]
    fn vacuum_stats_round_trips(a in any::<usize>(), b in any::<usize>()) {
        round_trip::<VacuumStats, pb::VacuumStats>(VacuumStats { terms_removed: a, terms_kept: b })?;
    }

    #[test]
    fn compact_stats_round_trips(a in any::<u64>(), b in any::<u64>()) {
        let s = CompactStats { before_bytes: a, after_bytes: b };
        prop_assert_eq!(CompactStats::from(pb::CompactStats::from(s)), s);
    }

    #[test]
    fn index_options_round_trips(reindex in any::<bool>()) {
        let o = IndexOptions { reindex };
        prop_assert_eq!(IndexOptions::from(pb::IndexOptions::from(o)), o);
    }

    #[test]
    fn view_round_trips(v in prop_oneof![
        Just(View::Local), Just(View::Linearizable), any::<u64>().prop_map(View::Snapshot)
    ]) {
        round_trip::<View, pb::View>(v)?;
    }

    #[test]
    fn file_result_round_trips(r in prop_oneof![
        ingest_stats().prop_map(Ok), store_error().prop_map(Err)
    ]) {
        let msg = pb::FileResult::from(clone_result(&r));
        let decoded = pb::FileResult::decode(msg.encode_to_vec().as_slice()).unwrap();
        let back: Result<IngestStats, StoreError> = decoded.try_into().unwrap();
        match (&r, &back) {
            (Ok(a), Ok(b)) => prop_assert_eq!(a, b),
            (Err(a), Err(b)) => prop_assert!(same_store_error(a, b), "{a:?} vs {b:?}"),
            _ => prop_assert!(false, "{r:?} vs {back:?}"),
        }
    }

    #[test]
    fn file_bytes_round_trips(path in text(), bytes in vec(any::<u8>(), 0..16),
                              language in option::of(text()), origin in option::of(text())) {
        let bf = graph_store::BatchFile {
            path: &path, bytes: &bytes, language: language.as_deref(), origin: origin.as_deref(),
        };
        let msg = pb::FileBytes::from(bf);
        let decoded = pb::FileBytes::decode(msg.encode_to_vec().as_slice()).unwrap();
        let back = decoded.as_batch_file();
        prop_assert_eq!(back.path, bf.path);
        prop_assert_eq!(back.bytes, bf.bytes);
        prop_assert_eq!(back.language, bf.language);
        prop_assert_eq!(back.origin, bf.origin);
    }

    /// `WireError -> Status -> WireError` keeps the variant, its fields and
    /// the message; the status code follows the ADR table.
    #[test]
    fn wire_error_round_trips_through_status(e in wire_error()) {
        let status: Status = (&e).into();
        prop_assert_eq!(status.code(), e.code());
        prop_assert_eq!(status.message(), e.to_string());
        prop_assert!(!status.details().is_empty());
        let back = WireError::from(&status);
        prop_assert!(same_wire_error(&e, &back), "{e:?} vs {back:?}");
        prop_assert_eq!(back.to_string(), e.to_string());
        // The detail itself round-trips through prost bytes.
        let d = pb::StoreErrorDetail::decode(status.details()).unwrap();
        prop_assert_eq!(d, e.detail());
    }

    /// `StoreError -> Status -> StoreError` keeps the variant and message.
    #[test]
    fn store_error_round_trips_through_status(e in store_error()) {
        let status = store_error_to_status(&e);
        let back = status_to_store_error(&status);
        prop_assert!(same_store_error(&e, &back), "{e:?} vs {back:?}");
        prop_assert_eq!(back.to_string(), e.to_string());
    }

    /// Without a detail, the code decides the variant and the message is kept.
    #[test]
    fn status_without_detail_maps_by_code(msg in text(), code in prop_oneof![
        Just(Code::InvalidArgument), Just(Code::Unavailable), Just(Code::FailedPrecondition),
        Just(Code::DataLoss), Just(Code::ResourceExhausted), Just(Code::Internal),
        Just(Code::Unimplemented), Just(Code::DeadlineExceeded), Just(Code::Unknown),
        Just(Code::Cancelled), Just(Code::Aborted),
    ]) {
        let status = Status::new(code, msg.clone());
        let e = WireError::from(&status);
        let expected_code = match code {
            Code::ResourceExhausted => Code::Internal, // plain text: not a disk-full message
            // A bare FAILED_PRECONDITION is a refusal, not a protocol error.
            Code::FailedPrecondition => Code::InvalidArgument,
            // Only a missing method is a protocol mismatch.
            Code::Unimplemented => Code::FailedPrecondition,
            // A lost connection is a storage-class error, never Protocol.
            Code::DeadlineExceeded | Code::Unknown | Code::Cancelled | Code::Aborted => Code::Internal,
            c => c,
        };
        prop_assert_eq!(e.code(), expected_code);
        prop_assert!(e.to_string().contains(&msg), "{e} lacks {msg:?}");
        prop_assert_eq!(
            matches!(e, WireError::Protocol(_)),
            code == Code::Unimplemented,
            "only Unimplemented maps to Protocol without a detail"
        );
    }

    /// A garbage `grpc-status-details-bin` payload falls back to the code
    /// (unless the bytes happen to be a valid detail, which then wins).
    #[test]
    fn undecodable_detail_falls_back_to_code(junk in vec(any::<u8>(), 1..32)) {
        let status = Status::with_details(Code::DataLoss, "bad", junk.clone().into());
        let e = WireError::from(&status);
        let valid = pb::StoreErrorDetail::decode(junk.as_slice())
            .ok()
            .and_then(|d| WireError::try_from(d).ok());
        match valid {
            Some(v) => prop_assert!(same_wire_error(&v, &e), "{v:?} vs {e:?}"),
            None => prop_assert!(
                matches!(e, WireError::Store(StoreError::Corrupt(ref m)) if m == "bad"),
                "{e:?}"
            ),
        }
    }
}

fn serde_json_str<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap()
}

fn clone_result(r: &Result<IngestStats, StoreError>) -> Result<IngestStats, StoreError> {
    match r {
        Ok(s) => Ok(s.clone()),
        Err(e) => Err(crate::error::wire_view(e).into()),
    }
}

fn same_store_error(a: &StoreError, b: &StoreError) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b) && a.to_string() == b.to_string()
}

fn same_wire_error(a: &WireError, b: &WireError) -> bool {
    match (a, b) {
        (WireError::Store(a), WireError::Store(b)) => same_store_error(a, b),
        (
            WireError::NotLeader {
                leader_id: a1,
                leader_addr: a2,
            },
            WireError::NotLeader {
                leader_id: b1,
                leader_addr: b2,
            },
        ) => a1 == b1 && a2 == b2,
        (WireError::NoLeader { retry_after_ms: a }, WireError::NoLeader { retry_after_ms: b }) => {
            a == b
        }
        (WireError::Protocol(a), WireError::Protocol(b)) => a == b,
        _ => false,
    }
}

// ------------------------------------------------------- code mapping --

#[test]
fn code_table_matches_adr_0004() {
    use StoreError as E;
    let cases: Vec<(WireError, Code)> = vec![
        (E::Rejected("x".into()).into(), Code::InvalidArgument),
        (E::NotUtf8("x".into()).into(), Code::InvalidArgument),
        (E::TooLarge("x".into()).into(), Code::InvalidArgument),
        (E::InvalidSpan("x".into()).into(), Code::InvalidArgument),
        (
            E::Schema(SchemaError::InvalidContainment(
                NodeKind::Org,
                NodeKind::File,
            ))
            .into(),
            Code::InvalidArgument,
        ),
        (E::Locked("x".into()).into(), Code::Unavailable),
        (
            E::SchemaMismatch { found: 9 }.into(),
            Code::FailedPrecondition,
        ),
        (
            E::LegacyFormat {
                path: "p".into(),
                version: 1,
            }
            .into(),
            Code::FailedPrecondition,
        ),
        (
            E::SnapshotExpired {
                age_secs: 1,
                max_age_secs: 0,
            }
            .into(),
            Code::FailedPrecondition,
        ),
        (E::Corrupt("x".into()).into(), Code::DataLoss),
        (E::Storage("x".into()).into(), Code::Internal),
        (
            E::Storage("No space left on device (os error 28)".into()).into(),
            Code::ResourceExhausted,
        ),
        (
            E::Storage("disk full".into()).into(),
            Code::ResourceExhausted,
        ),
        (
            E::Storage("write failed: os error 112".into()).into(),
            Code::ResourceExhausted,
        ),
        (
            E::OpenFailed {
                path: "p".into(),
                reason: "r".into(),
            }
            .into(),
            Code::Internal,
        ),
        (
            WireError::NotLeader {
                leader_id: Some(2),
                leader_addr: None,
            },
            Code::Unavailable,
        ),
        (
            WireError::NoLeader { retry_after_ms: 50 },
            Code::Unavailable,
        ),
        (WireError::Protocol("v9".into()), Code::FailedPrecondition),
    ];
    for (e, code) in cases {
        assert_eq!(e.code(), code, "{e:?}");
        assert_eq!(Status::from(&e).code(), code, "{e:?}");
    }
}

#[test]
fn cluster_errors_map_to_the_real_store_error_variants() {
    let e: StoreError = WireError::NotLeader {
        leader_id: Some(3),
        leader_addr: Some("h:1".into()),
    }
    .into();
    assert!(
        matches!(
            &e,
            StoreError::NotLeader { leader_id: Some(3), leader_addr: Some(a) } if a == "h:1"
        ),
        "{e:?}"
    );
    assert!(e.to_string().contains("node 3 at h:1"), "{e}");
    let e: StoreError = WireError::NoLeader { retry_after_ms: 7 }.into();
    assert!(matches!(e, StoreError::NoLeader { retry_after_ms: 7 }));
    let e: StoreError = WireError::Protocol("v9".into()).into();
    assert!(matches!(e, StoreError::Protocol(ref m) if m == "v9"));
    let e: StoreError = ConvertError("bad".into()).into();
    assert!(matches!(e, StoreError::Protocol(ref m) if m.contains("malformed message: bad")));
    // And back: a `WireError::Store` never wraps one of the three.
    for e in [
        StoreError::NotLeader {
            leader_id: None,
            leader_addr: None,
        },
        StoreError::NoLeader { retry_after_ms: 1 },
        StoreError::Protocol("p".into()),
    ] {
        let text = e.to_string();
        let w = WireError::from(e);
        assert!(!matches!(w, WireError::Store(_)), "{w:?}");
        assert_eq!(w.to_string(), text, "same text either way");
    }
}

/// `store_error_to_status` keeps the cluster-level variants in their own
/// class (mutation M5: `Protocol` must stay FAILED_PRECONDITION, not fall
/// into the `Store` arm).
#[test]
fn store_error_to_status_codes_the_cluster_level_variants() {
    use crate::error::store_error_to_status;
    assert_eq!(
        store_error_to_status(&StoreError::Protocol("v9".into())).code(),
        Code::FailedPrecondition
    );
    assert_eq!(
        store_error_to_status(&StoreError::NoLeader { retry_after_ms: 1 }).code(),
        Code::Unavailable
    );
}

/// A full disk as the store actually reports it: an io error wrapped by
/// redb and converted into `StoreError::Storage` (not a hand-written
/// string) is RESOURCE_EXHAUSTED. 28 is ENOSPC (Linux/macOS), 112 is
/// ERROR_DISK_FULL (Windows); only the current platform's code is real.
#[test]
fn a_redb_wrapped_disk_full_io_error_is_resource_exhausted() {
    use crate::error::{is_disk_full, store_error_to_status};
    let code = if cfg!(windows) { 112 } else { 28 };
    let io = std::io::Error::from_raw_os_error(code);
    let e = StoreError::from(redb::StorageError::Io(io));
    let msg = e.to_string();
    assert!(matches!(e, StoreError::Storage(_)), "{e:?}");
    assert!(is_disk_full(&msg), "{msg}");
    assert_eq!(store_error_to_status(&e).code(), Code::ResourceExhausted);
    let other = StoreError::from(redb::StorageError::Io(std::io::Error::other("boom")));
    assert_eq!(store_error_to_status(&other).code(), Code::Internal);
}

/// `AlreadyApplied` is server-internal; should it ever reach the wire it
/// travels as a `Rejected` refusal (INVALID_ARGUMENT) with its text.
#[test]
fn already_applied_travels_as_a_rejection() {
    use crate::error::{status_to_store_error, store_error_to_status};
    let s = store_error_to_status(&StoreError::AlreadyApplied { index: 7 });
    assert_eq!(s.code(), Code::InvalidArgument);
    let back = status_to_store_error(&s);
    assert!(
        matches!(back, StoreError::Rejected(ref m) if m == "raft marker 7 already applied"),
        "{back:?}"
    );
}

#[test]
fn schema_detail_with_unknown_kinds_keeps_the_text() {
    let d = pb::StoreErrorDetail {
        kind: Some(pb::store_error_detail::Kind::Schema(
            pb::store_error_detail::Schema {
                msg: "invalid containment: X cannot contain Y".into(),
                parent: 0,
                child: 99,
            },
        )),
    };
    let e = WireError::try_from(d).unwrap();
    assert!(
        matches!(e, WireError::Store(StoreError::Rejected(ref m)) if m.contains("X cannot contain Y"))
    );
}

// ----------------------------------------------------- malformed input --

#[test]
fn unspecified_enums_and_missing_fields_are_errors_not_defaults() {
    let n = pb::Node::default(); // kind = UNSPECIFIED
    assert!(Node::try_from(n).is_err());
    let mut n = pb::Node {
        kind: pb::NodeKind::File as i32,
        ..Default::default()
    };
    assert!(Node::try_from(n.clone()).is_ok());
    n.symbol_kind = Some(0);
    assert!(Node::try_from(n.clone()).is_err());
    n.symbol_kind = Some(1234);
    assert!(Node::try_from(n).is_err());

    assert!(Query::try_from(pb::Query::default()).is_err());
    assert!(Hit::try_from(pb::Hit::default()).is_err());
    assert!(SymbolHit::try_from(pb::SymbolHit::default()).is_err());
    // A SymbolDecl / TokenDecl without a span.
    let s = pb::SymbolDecl {
        kind: pb::SymbolKind::Type as i32,
        span: None,
        ..Default::default()
    };
    assert!(SymbolDecl::try_from(s).is_err());
    let t = pb::TokenDecl {
        class: pb::TokenClass::Other as i32,
        span: None,
        ..Default::default()
    };
    assert!(TokenDecl::try_from(t).is_err());
    assert!(matches!(
        Result::<IngestStats, StoreError>::try_from(pb::FileResult::default()),
        Err(ConvertError(_))
    ));
    assert!(WireError::try_from(pb::StoreErrorDetail::default()).is_err());
    // A View with an unknown read mode.
    let v = pb::View {
        v: Some(pb::view::V::Mode(7)),
    };
    assert!(View::try_from(v).is_err());
    assert_eq!(View::try_from(pb::View::default()).unwrap(), View::Local);
    assert_eq!(View::try_from(None).unwrap(), View::Local);
    // A ConvertError becomes a FAILED_PRECONDITION status with a Protocol detail.
    let st: Status = ConvertError("x".into()).into();
    assert_eq!(st.code(), Code::FailedPrecondition);
    assert!(matches!(WireError::from(&st), WireError::Protocol(_)));
}

/// Review items 13 and 15: the exact variants for detail-less statuses.
#[test]
fn detail_less_failed_precondition_and_transport_loss_are_not_protocol() {
    let e: StoreError = WireError::from(&Status::failed_precondition("nope")).into();
    assert!(
        matches!(e, StoreError::Rejected(ref m) if m == "nope"),
        "{e:?}"
    );
    let e: StoreError = WireError::from(&Status::unknown("transport error")).into();
    assert!(
        matches!(e, StoreError::Storage(ref m) if m.contains("connection lost") && m.contains("transport error")),
        "{e:?}"
    );
    for code in [Code::Cancelled, Code::DeadlineExceeded, Code::Aborted] {
        let e: StoreError = WireError::from(&Status::new(code, "x")).into();
        assert!(matches!(e, StoreError::Storage(_)), "{code:?}: {e:?}");
    }
}

#[test]
fn every_enum_value_round_trips_and_zero_is_refused() {
    use crate::convert::Wire;
    for v in 0..16i32 {
        let ok = Wire::<NodeKind>::try_from(v).is_ok();
        assert_eq!(ok, (1..=5).contains(&v), "NodeKind {v}");
        let ok = Wire::<SymbolKind>::try_from(v).is_ok();
        assert_eq!(ok, (1..=7).contains(&v), "SymbolKind {v}");
        let ok = Wire::<TokenClass>::try_from(v).is_ok();
        assert_eq!(ok, (1..=7).contains(&v), "TokenClass {v}");
        let ok = Wire::<Grain>::try_from(v).is_ok();
        assert_eq!(ok, (1..=7).contains(&v), "Grain {v}");
    }
}

// --------------------------------------------- decode never panics --

/// prost's decoder must reject, never panic on, any byte string, for every
/// top-level message of the four services (requests, responses, stream
/// items and the replicated log entry).
macro_rules! decode_never_panics {
    ($($name:ident: $ty:ty),* $(,)?) => {
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(128))]
            $(
                #[test]
                fn $name(bytes in vec(any::<u8>(), 0..256)) {
                    let _ = <$ty>::decode(bytes.as_slice());
                    // A decoded message re-encodes without panicking either.
                    if let Ok(m) = <$ty>::decode(bytes.as_slice()) {
                        let _ = m.encode_to_vec();
                    }
                }
            )*
        }
    };
}

decode_never_panics! {
    // common
    dnp_span: pb::Span, dnp_node: pb::Node, dnp_node_batch: pb::NodeBatch,
    dnp_node_page: pb::NodePage, dnp_query: pb::Query, dnp_symbol_query: pb::SymbolQuery,
    dnp_hit: pb::Hit, dnp_symbol_hit: pb::SymbolHit, dnp_language_info: pb::LanguageInfo,
    dnp_repo_info: pb::RepoInfo, dnp_ingest_stats: pb::IngestStats, dnp_file_bytes: pb::FileBytes,
    dnp_index_options: pb::IndexOptions, dnp_snapshot_stats: pb::SnapshotStats,
    dnp_vacuum_stats: pb::VacuumStats, dnp_compact_stats: pb::CompactStats,
    dnp_symbol_decl: pb::SymbolDecl, dnp_token_decl: pb::TokenDecl, dnp_extraction: pb::Extraction,
    dnp_store_error_detail: pb::StoreErrorDetail, dnp_view: pb::View,
    // store
    dnp_hello_request: pb::HelloRequest, dnp_hello_response: pb::HelloResponse,
    dnp_get_request: pb::GetRequest, dnp_get_response: pb::GetResponse,
    dnp_parent_request: pb::ParentRequest, dnp_parent_response: pb::ParentResponse,
    dnp_count_nodes_request: pb::CountNodesRequest, dnp_count_nodes_response: pb::CountNodesResponse,
    dnp_roots_request: pb::RootsRequest, dnp_roots_response: pb::RootsResponse,
    dnp_children_request: pb::ChildrenRequest, dnp_children_response: pb::ChildrenResponse,
    dnp_children_page_request: pb::ChildrenPageRequest,
    dnp_descendants_request: pb::DescendantsRequest, dnp_descendants_page_request: pb::DescendantsPageRequest,
    dnp_ancestors_request: pb::AncestorsRequest, dnp_ancestors_response: pb::AncestorsResponse,
    dnp_file_tokens_request: pb::FileTokensRequest,
    dnp_describe_request: pb::DescribeRequest, dnp_describe_response: pb::DescribeResponse,
    dnp_search_symbols_request: pb::SearchSymbolsRequest, dnp_search_symbols_response: pb::SearchSymbolsResponse,
    dnp_search_request: pb::SearchRequest, dnp_search_response: pb::SearchResponse,
    dnp_open_snapshot_request: pb::OpenSnapshotRequest, dnp_open_snapshot_response: pb::OpenSnapshotResponse,
    dnp_close_snapshot_request: pb::CloseSnapshotRequest, dnp_close_snapshot_response: pb::CloseSnapshotResponse,
    dnp_snapshot_stats_request: pb::SnapshotStatsRequest, dnp_snapshot_stats_response: pb::SnapshotStatsResponse,
    // write
    dnp_index_header: pb::IndexHeader, dnp_index_request: pb::IndexRequest,
    dnp_file_result: pb::FileResult, dnp_index_response: pb::IndexResponse,
    dnp_index_file_request: pb::IndexFileRequest, dnp_index_file_response: pb::IndexFileResponse,
    dnp_ingest_extraction_request: pb::IngestExtractionRequest,
    dnp_ingest_extraction_response: pb::IngestExtractionResponse,
    dnp_prune_request: pb::PruneRequest, dnp_prune_response: pb::PruneResponse,
    dnp_vacuum_request: pb::VacuumRequest, dnp_vacuum_response: pb::VacuumResponse,
    // admin
    dnp_status_request: pb::StatusRequest, dnp_status_response: pb::StatusResponse,
    dnp_sysinfo_request: pb::SysInfoRequest, dnp_sysinfo_response: pb::SysInfoResponse,
    dnp_compact_request: pb::CompactRequest, dnp_compact_response: pb::CompactResponse,
    dnp_shutdown_request: pb::ShutdownRequest, dnp_shutdown_response: pb::ShutdownResponse,
    dnp_member: pb::Member, dnp_members_request: pb::MembersRequest, dnp_members_response: pb::MembersResponse,
    dnp_leader_request: pb::LeaderRequest, dnp_leader_response: pb::LeaderResponse,
    dnp_add_learner_request: pb::AddLearnerRequest, dnp_add_learner_response: pb::AddLearnerResponse,
    dnp_promote_request: pb::PromoteRequest, dnp_promote_response: pb::PromoteResponse,
    dnp_remove_request: pb::RemoveRequest, dnp_remove_response: pb::RemoveResponse,
    dnp_transfer_leader_request: pb::TransferLeaderRequest, dnp_transfer_leader_response: pb::TransferLeaderResponse,
    dnp_trigger_snapshot_request: pb::TriggerSnapshotRequest, dnp_trigger_snapshot_response: pb::TriggerSnapshotResponse,
    dnp_metrics_request: pb::MetricsRequest, dnp_metrics_response: pb::MetricsResponse,
    // raft
    dnp_append_entries_request: pb::AppendEntriesRequest, dnp_append_entries_response: pb::AppendEntriesResponse,
    dnp_vote_request: pb::VoteRequest, dnp_vote_response: pb::VoteResponse,
    dnp_install_snapshot_header: pb::InstallSnapshotHeader, dnp_install_snapshot_request: pb::InstallSnapshotRequest,
    dnp_install_snapshot_response: pb::InstallSnapshotResponse, dnp_log_command: pb::LogCommand,
    dnp_raft_vote: pb::RaftVote, dnp_raft_log_id: pb::RaftLogId, dnp_raft_empty: pb::RaftEmpty,
    dnp_peer_lag: pb::PeerLag, dnp_snapshot_info: pb::SnapshotInfo,
}

// A fuzzed message that decodes must convert without panicking (to the Rust
// type or to a `ConvertError`), for every converted type.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]
    #[test]
    fn conversions_never_panic_on_decoded_garbage(bytes in vec(any::<u8>(), 0..256)) {
        let b = bytes.as_slice();
        if let Ok(m) = pb::Node::decode(b) { let _ = Node::try_from(m); }
        if let Ok(m) = pb::Query::decode(b) { let _ = Query::try_from(m); }
        if let Ok(m) = pb::SymbolQuery::decode(b) { let _ = SymbolQuery::try_from(m); }
        if let Ok(m) = pb::Hit::decode(b) { let _ = Hit::try_from(m); }
        if let Ok(m) = pb::SymbolHit::decode(b) { let _ = SymbolHit::try_from(m); }
        if let Ok(m) = pb::RepoInfo::decode(b) { let _ = RepoInfo::try_from(m); }
        if let Ok(m) = pb::IngestStats::decode(b) { let _ = IngestStats::try_from(m); }
        if let Ok(m) = pb::Extraction::decode(b) { let _ = Extraction::try_from(m); }
        if let Ok(m) = pb::SnapshotStats::decode(b) { let _ = SnapshotStats::try_from(m); }
        if let Ok(m) = pb::StoreErrorDetail::decode(b) { let _ = WireError::try_from(m); }
        if let Ok(m) = pb::FileResult::decode(b) { let _ = Result::<IngestStats, StoreError>::try_from(m); }
        if let Ok(m) = pb::View::decode(b) { let _ = View::try_from(m); }
        if let Ok(m) = pb::NodePage::decode(b) { let _ = Page::<Node>::try_from(m); }
    }
}

#[test]
fn protocol_version_is_one() {
    assert_eq!(crate::PROTOCOL_VERSION, 1);
    assert_eq!(crate::STREAM_BATCH_NODES, 4096);
    assert_eq!(crate::RAFT_ENTRY_MAX_BYTES, 8 << 20);
}
