//! Epic story 70 (ADR 0011 D4 step 1, amended): an in-memory copy of the
//! distinct symbol names, so an infix or suffix query scans memory instead
//! of walking every key of `sym_fold` (or `sym_idx`) in redb.
//!
//! - The cache is per open store, built lazily by the first infix or suffix
//!   query (from the walk that query does anyway) and never persisted.
//! - It is tagged with the store's **write generation** ([`GenDb`]): the
//!   count of write transactions that have finished. A read uses the cache
//!   only when its read transaction saw exactly that generation with no
//!   write in flight; otherwise it walks redb as before. So a cached answer
//!   always equals the walk's answer for the same read transaction.
//! - Its size is bounded ([`DEFAULT_NAME_CACHE_MAX_BYTES`]); a store whose
//!   names do not fit is never cached and always walks. The ADR 0008 read
//!   cache pool does not exist yet; when it does, this budget moves there.
use redb::{Database, ReadTransaction, WriteTransaction};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};

/// The default bound on one name cache (names, offsets and ids), per case
/// mode. The A5 index (877M tokens, 2.0M distinct folded names) needs about
/// 100 MB per mode.
pub const DEFAULT_NAME_CACHE_MAX_BYTES: usize = 256 << 20;

/// A redb [`Database`] that counts its write transactions, so a reader can
/// tell which committed state it sees (see the module docs). Every write
/// goes through [`GenDb::begin_write`] or [`GenDb::raw_write`].
pub(crate) struct GenDb {
    db: Database,
    /// Write transactions begun (bumped before `begin_write`).
    started: AtomicU64,
    /// Write transactions finished (committed, aborted or dropped).
    done: AtomicU64,
}

impl Deref for GenDb {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}

impl GenDb {
    pub(crate) fn new(db: Database) -> Self {
        Self {
            db,
            started: AtomicU64::new(0),
            done: AtomicU64::new(0),
        }
    }

    /// `Database::begin_write`, counted.
    pub(crate) fn begin_write(&self) -> Result<GenWt<'_>, crate::StoreError> {
        self.started.fetch_add(1, SeqCst);
        match self.db.begin_write() {
            Ok(wt) => Ok(GenWt {
                wt: Some(wt),
                done: &self.done,
            }),
            Err(e) => {
                self.done.fetch_add(1, SeqCst);
                Err(e.into())
            }
        }
    }

    /// Run a helper that writes through a plain `&Database`, counted as
    /// one write.
    pub(crate) fn raw_write<T>(&self, f: impl FnOnce(&Database) -> T) -> T {
        self.started.fetch_add(1, SeqCst);
        let out = f(&self.db);
        self.done.fetch_add(1, SeqCst);
        out
    }

    /// Begin a read transaction, with the write generation it sees: `None`
    /// when a write began or finished around it, so the state it sees is
    /// not known.
    pub(crate) fn begin_read_gen(
        &self,
    ) -> Result<(ReadTransaction, Option<u64>), crate::StoreError> {
        let done = self.done.load(SeqCst);
        let rt = self.db.begin_read()?;
        let started = self.started.load(SeqCst);
        Ok((rt, (started == done).then_some(done)))
    }
}

/// A counted write transaction: dereferences to the redb one, and counts
/// as finished when committed, aborted or dropped.
pub(crate) struct GenWt<'a> {
    wt: Option<WriteTransaction>,
    done: &'a AtomicU64,
}

impl GenWt<'_> {
    pub(crate) fn commit(mut self) -> Result<(), redb::CommitError> {
        self.wt.take().expect("live write transaction").commit()
    }

    pub(crate) fn abort(mut self) -> Result<(), redb::StorageError> {
        self.wt.take().expect("live write transaction").abort()
    }
}

impl Deref for GenWt<'_> {
    type Target = WriteTransaction;
    fn deref(&self) -> &WriteTransaction {
        self.wt.as_ref().expect("live write transaction")
    }
}

impl DerefMut for GenWt<'_> {
    fn deref_mut(&mut self) -> &mut WriteTransaction {
        self.wt.as_mut().expect("live write transaction")
    }
}

impl Drop for GenWt<'_> {
    fn drop(&mut self) {
        // Drop the redb transaction (an implicit abort) before counting it.
        drop(self.wt.take());
        self.done.fetch_add(1, SeqCst);
    }
}

/// The distinct names of one symbol index, with each name's ids.
pub(crate) struct NameCache {
    generation: u64,
    /// Every name, concatenated (valid UTF-8, as each name is).
    names: String,
    /// The end offset of each name in `names`.
    ends: Vec<u32>,
    /// The end offset of each name's ids in `ids`.
    id_ends: Vec<u32>,
    ids: Vec<u64>,
}

impl NameCache {
    fn bytes(&self) -> usize {
        self.names.len() + 4 * (self.ends.len() + self.id_ends.len()) + 8 * self.ids.len()
    }

    /// The ids of every name containing `needle` that `matches` accepts.
    pub(crate) fn collect(&self, needle: &str, matches: impl Fn(&str) -> bool, out: &mut Vec<u64>) {
        let mut pos = 0usize;
        while let Some(m) = self.names[pos..].find(needle) {
            let at = pos + m;
            // The name holding the match's first byte.
            let i = self.ends.partition_point(|&e| e as usize <= at);
            if i >= self.ends.len() {
                break;
            }
            let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
            let end = self.ends[i] as usize;
            if matches(&self.names[start..end]) {
                let lo = if i == 0 {
                    0
                } else {
                    self.id_ends[i - 1] as usize
                };
                out.extend_from_slice(&self.ids[lo..self.id_ends[i] as usize]);
            }
            // The rest of this name is decided; an empty needle still moves.
            pos = end.max(at + 1);
            if pos > self.names.len() {
                break;
            }
        }
    }
}

/// Collects a [`NameCache`] during a full walk, giving up once it passes
/// the size bound.
pub(crate) struct NameCacheBuilder {
    cache: Option<NameCache>,
    max_bytes: usize,
}

impl NameCacheBuilder {
    pub(crate) fn push_name(&mut self, name: &str) {
        let Some(c) = self.cache.as_mut() else {
            return;
        };
        c.names.push_str(name);
        match u32::try_from(c.names.len()) {
            Ok(end) => c.ends.push(end),
            Err(_) => self.cache = None,
        }
    }

    pub(crate) fn push_id(&mut self, id: u64) {
        if let Some(c) = self.cache.as_mut() {
            c.ids.push(id);
        }
    }

    pub(crate) fn end_name(&mut self) {
        let Some(c) = self.cache.as_mut() else {
            return;
        };
        match u32::try_from(c.ids.len()) {
            Ok(end) => c.id_ends.push(end),
            Err(_) => {
                self.cache = None;
                return;
            }
        }
        if c.bytes() > self.max_bytes {
            self.cache = None;
        }
    }
}

/// The two name caches of an open store (folded and exact case), shared
/// by its snapshots.
pub(crate) struct NameCaches {
    fold: Mutex<Option<Arc<NameCache>>>,
    exact: Mutex<Option<Arc<NameCache>>>,
    max_bytes: AtomicUsize,
}

impl Default for NameCaches {
    fn default() -> Self {
        Self {
            fold: Mutex::new(None),
            exact: Mutex::new(None),
            max_bytes: AtomicUsize::new(DEFAULT_NAME_CACHE_MAX_BYTES),
        }
    }
}

impl NameCaches {
    fn slot(&self, exact_case: bool) -> &Mutex<Option<Arc<NameCache>>> {
        if exact_case {
            &self.exact
        } else {
            &self.fold
        }
    }

    pub(crate) fn set_max_bytes(&self, n: usize) {
        self.max_bytes.store(n, SeqCst);
        for slot in [&self.fold, &self.exact] {
            let mut g = slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if g.as_ref().is_some_and(|c| c.bytes() > n) {
                *g = None;
            }
        }
    }

    /// The cache for `generation`, if one is held.
    pub(crate) fn get(&self, exact_case: bool, generation: u64) -> Option<Arc<NameCache>> {
        let g = self
            .slot(exact_case)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.as_ref()
            .filter(|c| c.generation == generation)
            .map(Arc::clone)
    }

    /// A builder for `generation`, or `None` when caching is off.
    pub(crate) fn builder(&self, generation: u64) -> Option<NameCacheBuilder> {
        let max_bytes = self.max_bytes.load(SeqCst);
        (max_bytes > 0).then(|| NameCacheBuilder {
            cache: Some(NameCache {
                generation,
                names: String::new(),
                ends: Vec::new(),
                id_ends: Vec::new(),
                ids: Vec::new(),
            }),
            max_bytes,
        })
    }

    /// Keep a finished build, unless the slot already holds a newer one.
    pub(crate) fn put(&self, exact_case: bool, b: NameCacheBuilder) {
        let Some(mut c) = b.cache else {
            return;
        };
        c.names.shrink_to_fit();
        c.ends.shrink_to_fit();
        c.id_ends.shrink_to_fit();
        c.ids.shrink_to_fit();
        let mut g = self
            .slot(exact_case)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if g.as_ref().is_none_or(|old| old.generation < c.generation) {
            *g = Some(Arc::new(c));
        }
    }

    /// Bytes held, `(folded, exact)`; for tests and diagnostics.
    pub(crate) fn held_bytes(&self) -> (usize, usize) {
        let b = |s: &Mutex<Option<Arc<NameCache>>>| {
            s.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .map_or(0, |c| c.bytes())
        };
        (b(&self.fold), b(&self.exact))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(names: &[(&str, &[u64])]) -> NameCache {
        let caches = NameCaches::default();
        let mut b = caches.builder(0).unwrap();
        for (n, ids) in names {
            b.push_name(n);
            for &id in *ids {
                b.push_id(id);
            }
            b.end_name();
        }
        b.cache.unwrap()
    }

    #[test]
    fn collect_matches_each_name_once_and_never_across_names() {
        let c = cache(&[("ab", &[1]), ("cab", &[2, 3]), ("abab", &[4]), ("x", &[])]);
        let run = |needle: &str, suffix: bool| {
            let mut out = Vec::new();
            c.collect(
                needle,
                |n| {
                    if suffix {
                        n.ends_with(needle)
                    } else {
                        n.contains(needle)
                    }
                },
                &mut out,
            );
            out
        };
        assert_eq!(run("ab", false), [1, 2, 3, 4]);
        assert_eq!(run("ab", true), [1, 2, 3, 4]);
        assert_eq!(run("ba", false), [4]);
        // "b" + "c" spans "ab|cab": no name holds it.
        assert_eq!(run("bc", false), Vec::<u64>::new());
    }

    #[test]
    fn a_build_past_the_bound_is_dropped() {
        let caches = NameCaches::default();
        caches.set_max_bytes(16);
        let mut b = caches.builder(7).unwrap();
        for i in 0..10 {
            b.push_name(&format!("name{i}"));
            b.push_id(i);
            b.end_name();
        }
        caches.put(false, b);
        assert!(caches.get(false, 7).is_none());
        caches.set_max_bytes(0);
        assert!(caches.builder(7).is_none());
    }
}
