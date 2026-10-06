//! The request log behind ADR 0008 phase 3's gate (epic story 49): how
//! many read RPCs repeat a query this node answered within the last
//! [`REPEAT_WINDOW`] at the same Raft applied index (the store has no
//! generation counter; the applied index advances with every write, so an
//! unchanged index means an unchanged store). A result cache keyed on the
//! request and the generation could only ever serve those repeats, so
//! `mg_query_exact_repeats_total / mg_queries_total` approximates its hit
//! rate (a lower bound).
//!
//! The key leaves out the read view: a `Local` and a `Linearizable` read of
//! the same query at the same applied index give the same answer, and a
//! snapshot read's generation is the handle's frozen applied index.
//!
//! Why only approximate, and mostly low:
//! - the key hashes the encoded request, so the same query with its
//!   repeated fields in another order counts as a different one;
//! - past [`MAX_ENTRIES`] distinct queries per window the oldest are
//!   evicted and their repeats go uncounted;
//! - counts are per node (a client spreading reads over nodes repeats less
//!   on each);
//! - the applied index is read before the read runs, so a write applied
//!   while it runs can, rarely, make two different answers count as a
//!   repeat (a slight over-count).
//!
//! Cost on the hot path: one prost encode of the (small) request into a
//! `Vec`, one SipHash over it, and one short critical section (a hash map
//! lookup and insert plus a deque push, amortised O(1) eviction). The map
//! and the deque are hard-capped at [`MAX_ENTRIES`] entries, so memory stays
//! bounded (about 90-100 B per entry with hash map overhead, about 6 MB at
//! the cap) whatever the request rate.
//!
//! The hash is std's `DefaultHasher`: not stable across processes or
//! releases, which is fine for an in-memory window.
use prost::Message;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long a query stays in the log.
pub const REPEAT_WINDOW: Duration = Duration::from_secs(60);

/// The log's hard entry cap: past it the oldest entry is dropped (a
/// repeat of it then goes uncounted, so under very high distinct-query
/// rates the ratio is a lower bound).
pub const MAX_ENTRIES: usize = 65_536;

/// The read RPCs the log counts: a fixed set, so the `rpc` label's
/// cardinality is fixed too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadRpc {
    Get,
    Parent,
    CountNodes,
    Roots,
    Children,
    ChildrenPage,
    Descendants,
    DescendantsPage,
    Ancestors,
    FileTokens,
    Describe,
    DescribeByScan,
    SearchSymbols,
    Search,
}

impl ReadRpc {
    /// Every variant, in label order.
    pub const ALL: [ReadRpc; 14] = [
        ReadRpc::Get,
        ReadRpc::Parent,
        ReadRpc::CountNodes,
        ReadRpc::Roots,
        ReadRpc::Children,
        ReadRpc::ChildrenPage,
        ReadRpc::Descendants,
        ReadRpc::DescendantsPage,
        ReadRpc::Ancestors,
        ReadRpc::FileTokens,
        ReadRpc::Describe,
        ReadRpc::DescribeByScan,
        ReadRpc::SearchSymbols,
        ReadRpc::Search,
    ];

    /// The `rpc` label (the gRPC method name).
    pub fn as_str(self) -> &'static str {
        match self {
            ReadRpc::Get => "Get",
            ReadRpc::Parent => "Parent",
            ReadRpc::CountNodes => "CountNodes",
            ReadRpc::Roots => "Roots",
            ReadRpc::Children => "Children",
            ReadRpc::ChildrenPage => "ChildrenPage",
            ReadRpc::Descendants => "Descendants",
            ReadRpc::DescendantsPage => "DescendantsPage",
            ReadRpc::Ancestors => "Ancestors",
            ReadRpc::FileTokens => "FileTokens",
            ReadRpc::Describe => "Describe",
            ReadRpc::DescribeByScan => "DescribeByScan",
            ReadRpc::SearchSymbols => "SearchSymbols",
            ReadRpc::Search => "Search",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// One query's identity: the RPC and a hash of its encoded request (the
/// read view is a request field, so different views never collide).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QueryKey {
    rpc: ReadRpc,
    hash: u64,
}

impl QueryKey {
    /// The key of `req` sent to `rpc`. Callers apply server-side defaults
    /// (the default search limit) first, so an omitted field and its
    /// default value are the same query.
    pub fn of(rpc: ReadRpc, req: &impl Message) -> Self {
        let mut h = DefaultHasher::new();
        rpc.hash(&mut h);
        req.encode_to_vec().hash(&mut h);
        Self {
            rpc,
            hash: h.finish(),
        }
    }
}

/// Queries and exact repeats so far for one RPC.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RepeatCounts {
    pub queries: u64,
    pub repeats: u64,
}

#[derive(Default)]
struct State {
    /// key -> (applied index, last seen).
    last: HashMap<QueryKey, (u64, Instant)>,
    /// Insertion order for eviction; an entry whose instant no longer
    /// matches `last` is stale (the key was seen again since) and is
    /// skipped.
    order: VecDeque<(QueryKey, Instant)>,
    counts: [RepeatCounts; ReadRpc::ALL.len()],
}

impl State {
    /// Drop the front of `order` (and its `last` entry, unless the key was
    /// seen again since).
    fn pop_oldest(&mut self) {
        if let Some((k, t)) = self.order.pop_front() {
            if self.last.get(&k).is_some_and(|(_, seen)| *seen == t) {
                self.last.remove(&k);
            }
        }
    }
}

/// The bounded request log (see the module docs).
pub struct RepeatLog {
    window: Duration,
    cap: usize,
    state: Mutex<State>,
}

impl Default for RepeatLog {
    fn default() -> Self {
        Self::new(REPEAT_WINDOW, MAX_ENTRIES)
    }
}

impl RepeatLog {
    pub fn new(window: Duration, cap: usize) -> Self {
        Self {
            window,
            cap: cap.max(1),
            state: Mutex::default(),
        }
    }

    /// Count one answered query `key` at `applied` (the Raft applied index
    /// it was answered at), seen at `now`. Returns whether it was an exact
    /// repeat: the same key at the same applied index within the window.
    pub fn note(&self, key: QueryKey, applied: u64, now: Instant) -> bool {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        while s
            .order
            .front()
            .is_some_and(|(_, t)| now.saturating_duration_since(*t) > self.window)
        {
            s.pop_oldest();
        }
        let repeat = s.last.get(&key).is_some_and(|(idx, t)| {
            *idx == applied && now.saturating_duration_since(*t) <= self.window
        });
        while s.order.len() >= self.cap {
            s.pop_oldest();
        }
        s.last.insert(key, (applied, now));
        s.order.push_back((key, now));
        let c = &mut s.counts[key.rpc.index()];
        c.queries += 1;
        c.repeats += u64::from(repeat);
        repeat
    }

    /// Counts per RPC, in [`ReadRpc::ALL`] order.
    pub fn counts(&self) -> [(ReadRpc, RepeatCounts); ReadRpc::ALL.len()] {
        let s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        ReadRpc::ALL.map(|r| (r, s.counts[r.index()]))
    }

    /// Entries held now (at most the cap).
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graph_proto::pb;

    fn search(text: &str, view: Option<pb::View>) -> pb::SearchRequest {
        pb::SearchRequest {
            query: Some(pb::Query {
                text: text.into(),
                ..Default::default()
            }),
            view,
        }
    }

    fn key(text: &str) -> QueryKey {
        QueryKey::of(ReadRpc::Search, &search(text, None))
    }

    fn counts(log: &RepeatLog, rpc: ReadRpc) -> RepeatCounts {
        log.counts()
            .into_iter()
            .find(|(r, _)| *r == rpc)
            .map(|(_, c)| c)
            .expect("every rpc has counts")
    }

    #[test]
    fn a_repeat_within_the_window_counts() {
        let log = RepeatLog::default();
        let t = Instant::now();
        assert!(!log.note(key("a"), 5, t));
        assert!(log.note(key("a"), 5, t + Duration::from_secs(59)));
        assert_eq!(
            counts(&log, ReadRpc::Search),
            RepeatCounts {
                queries: 2,
                repeats: 1
            }
        );
    }

    #[test]
    fn a_repeat_outside_the_window_does_not() {
        let log = RepeatLog::default();
        let t = Instant::now();
        log.note(key("a"), 5, t);
        assert!(!log.note(key("a"), 5, t + Duration::from_secs(61)));
        assert_eq!(counts(&log, ReadRpc::Search).repeats, 0);
    }

    #[test]
    fn a_changed_applied_index_does_not_count() {
        let log = RepeatLog::default();
        let t = Instant::now();
        log.note(key("a"), 5, t);
        assert!(!log.note(key("a"), 6, t + Duration::from_secs(1)));
        // ...but the next one at the new index does.
        assert!(log.note(key("a"), 6, t + Duration::from_secs(2)));
    }

    #[test]
    fn the_cap_evicts_the_oldest() {
        let log = RepeatLog::new(REPEAT_WINDOW, 2);
        let t = Instant::now();
        log.note(key("a"), 1, t);
        log.note(key("b"), 1, t);
        log.note(key("c"), 1, t); // evicts a
        assert_eq!(log.len(), 2);
        assert!(log.note(key("c"), 1, t)); // evicts b
        assert!(!log.note(key("a"), 1, t), "a was evicted");
        assert!(log.len() <= 2);
    }

    #[test]
    fn repeats_of_one_key_stay_within_the_cap() {
        let log = RepeatLog::new(REPEAT_WINDOW, 3);
        let t = Instant::now();
        for _ in 0..100 {
            log.note(key("a"), 1, t);
        }
        assert_eq!(log.len(), 1);
        assert!(log.state.lock().unwrap().order.len() <= 3);
        assert!(log.note(key("a"), 1, t), "the live entry survives");
    }

    #[test]
    fn expired_entries_are_dropped() {
        let log = RepeatLog::default();
        let t = Instant::now();
        log.note(key("a"), 1, t);
        log.note(key("b"), 1, t + Duration::from_secs(61));
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn views_collide_once_cleared_but_rpcs_do_not() {
        let log = RepeatLog::default();
        let t = Instant::now();
        let mode = |m: pb::ReadMode| {
            search(
                "a",
                Some(pb::View {
                    v: Some(pb::view::V::Mode(m as i32)),
                }),
            )
        };
        // The service clears the view before keying (`view_free_key!`).
        let cleared = |mut r: pb::SearchRequest| {
            r.view = None;
            QueryKey::of(ReadRpc::Search, &r)
        };
        let local = mode(pb::ReadMode::Local);
        log.note(cleared(local.clone()), 1, t);
        assert!(log.note(cleared(mode(pb::ReadMode::Linearizable)), 1, t));
        assert!(!log.note(QueryKey::of(ReadRpc::DescribeByScan, &local), 1, t));
        assert_eq!(counts(&log, ReadRpc::Search).queries, 2);
        assert_eq!(counts(&log, ReadRpc::DescribeByScan).queries, 1);
    }

    #[test]
    fn concurrent_notes_of_one_key_count_consistently() {
        let log = std::sync::Arc::new(RepeatLog::default());
        let t = Instant::now();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let log = std::sync::Arc::clone(&log);
                std::thread::spawn(move || {
                    for _ in 0..500 {
                        log.note(key("a"), 1, t);
                    }
                })
            })
            .collect();
        for h in threads {
            h.join().expect("noting thread");
        }
        let c = counts(&log, ReadRpc::Search);
        assert_eq!(c.queries, 4000);
        assert_eq!(c.repeats, 3999, "all but the first are repeats");
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn labels_are_distinct_and_in_index_order() {
        for (i, r) in ReadRpc::ALL.iter().enumerate() {
            assert_eq!(r.index(), i);
        }
        let mut names: Vec<_> = ReadRpc::ALL.iter().map(|r| r.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ReadRpc::ALL.len());
    }
}
