//! [`PowerCutDisk`]: an in-memory redb [`StorageBackend`] that can lose
//! power (the durability tests of ADR 0004 D7).
//!
//! Every file is two images: `current` (what reads see, every write lands
//! here) and `durable` (what survives a power cut: `current` as of the last
//! `sync_data`). [`PowerCutDisk::power_cut`] throws away everything written
//! since the last sync of every file under a directory, and cuts the old
//! handles off (a node still running on them afterwards writes nowhere), so
//! the next open sees exactly what a machine that lost power would find.
//! The disk also records the order of writes and syncs per file
//! ([`DiskEvent`]) and can die after the n-th sync of a file
//! ([`PowerCutDisk::fail_after_syncs`]: every later write, resize and sync
//! fails), a crash at a deterministic point inside a commit sequence.
//!
//! Model: a sync is a full barrier. What a power cut keeps of the writes
//! made after a file's last sync is a [`CutMode`]:
//!
//! * [`CutMode::DropUnsynced`]: nothing (the kindest disk).
//! * [`CutMode::Subset`]: a seeded random subset of them, in any
//!   combination: each write is lost, kept, or torn, where a torn write
//!   keeps a random subset of its 4 KiB pages and the last page it keeps
//!   may itself be cut at a random byte (a partial page); a resize is kept
//!   or lost. What a disk that reorders its cache may leave.
//! * [`CutMode::Prefix`]: a seeded random prefix of them, the write at the
//!   cut torn at a random byte (an in-order disk that lost power mid-write).
//!
//! These check both sides: the server acknowledges nothing before its bytes
//! were synced, and redb's recovery (checksummed commit slots, rollback to
//! the last good commit) turns any such leftover into a consistent file.
//!
//! Both `graph.redb` (the store, through `V2Store::open_with_backend`) and
//! `raft.redb` (the log) of a node run on it when a test sets
//! `ServeConfig::storage_backend`. Out of scope, on the real file system:
//! the snapshot files under `snapshots/` and `node.json`. They are not redb
//! files and have their own crash protocol (a temporary file renamed into
//! place; `node.json` fsynced first; a snapshot checked against its
//! recorded size on load and its SHA-256 when received), which the
//! crash-point tests in `raft::snapshot_dir` and `paths` cover; this disk
//! models redb's backend interface only.
//!
//! Test-only: `serve` never builds one.
use redb::StorageBackend;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Opens the storage of the store or log file at a path
/// ([`crate::ServeConfig::storage_backend`]).
pub type BackendFactory = Arc<dyn Fn(&Path) -> Box<dyn StorageBackend> + Send + Sync>;

/// One step of a file's I/O, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskEvent {
    Write { offset: u64, len: u64 },
    SetLen(u64),
    Sync,
}

/// What a power cut keeps of the writes since a file's last sync (see
/// the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutMode {
    DropUnsynced,
    Subset { seed: u64 },
    Prefix { seed: u64 },
}

impl CutMode {
    /// Every mode, the seeded ones with `seed`.
    pub fn all(seed: u64) -> [CutMode; 3] {
        [
            CutMode::DropUnsynced,
            CutMode::Subset { seed },
            CutMode::Prefix { seed },
        ]
    }
}

/// Torn writes lose or keep whole pages of this size (and the last one
/// kept may be partial).
pub const TEAR_PAGE: usize = 4096;

/// An unsynced write or resize, kept for replay by a power cut.
#[derive(Debug, Clone)]
enum Pending {
    Write { offset: usize, data: Vec<u8> },
    SetLen(usize),
}

/// splitmix64: a tiny seeded generator (no dependency, stable forever, so
/// a seed in a failure message reproduces it).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`).
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn put(img: &mut Vec<u8>, offset: usize, data: &[u8]) {
    let end = offset + data.len();
    if img.len() < end {
        img.resize(end, 0);
    }
    img[offset..end].copy_from_slice(data);
}

/// Keep a random subset of the pages of a write at `offset`, the last kept
/// page cut at a random byte (a partial page).
fn tear(img: &mut Vec<u8>, offset: usize, data: &[u8], rng: &mut Rng) {
    let mut pages = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let page_end = ((offset + at) / TEAR_PAGE + 1) * TEAR_PAGE - offset;
        let end = page_end.min(data.len());
        pages.push((at, end));
        at = end;
    }
    let kept: Vec<_> = pages.into_iter().filter(|_| rng.below(2) == 0).collect();
    let last = kept.len().checked_sub(1);
    for (i, (a, b)) in kept.into_iter().enumerate() {
        let b = if Some(i) == last {
            a + 1 + rng.below((b - a) as u64) as usize
        } else {
            b
        };
        put(img, offset + a, &data[a..b]);
    }
}

/// What survives of `durable` plus `pending` under `mode`.
fn survivor(durable: &[u8], pending: &[Pending], mode: CutMode, salt: u64) -> Vec<u8> {
    let mut img = durable.to_vec();
    let apply = |img: &mut Vec<u8>, op: &Pending| match op {
        Pending::Write { offset, data } => put(img, *offset, data),
        Pending::SetLen(n) => img.resize(*n, 0),
    };
    match mode {
        CutMode::DropUnsynced => {}
        CutMode::Subset { seed } => {
            let mut rng = Rng(seed ^ salt);
            for op in pending {
                match (op, rng.below(3)) {
                    (_, 0) => {}
                    (Pending::Write { offset, data }, 2) => tear(&mut img, *offset, data, &mut rng),
                    (op, _) => apply(&mut img, op),
                }
            }
        }
        CutMode::Prefix { seed } => {
            let mut rng = Rng(seed ^ salt);
            let n = rng.below(pending.len() as u64 + 1) as usize;
            for op in &pending[..n] {
                apply(&mut img, op);
            }
            if let Some(Pending::Write { offset, data }) = pending.get(n) {
                if !data.is_empty() {
                    let cut = rng.below(data.len() as u64) as usize;
                    put(&mut img, *offset, &data[..cut]);
                }
            }
        }
    }
    img
}

#[derive(Debug, Default)]
struct FileState {
    durable: Vec<u8>,
    current: Vec<u8>,
    /// Writes and resizes since the last sync, in order.
    pending: Vec<Pending>,
    events: Vec<DiskEvent>,
    syncs: u64,
    /// The disk dies once `syncs` reaches this.
    fail_at_sync: Option<u64>,
    /// The sync that would make `syncs` this fails instead, and the disk
    /// dies.
    fail_before_sync: Option<u64>,
    /// Writes, resizes and syncs fail (the disk died, or a power cut
    /// detached this state from the file).
    dead: bool,
}

type Shared = Arc<Mutex<FileState>>;

fn lock(s: &Shared) -> std::sync::MutexGuard<'_, FileState> {
    s.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A set of in-memory files that can lose power together.
#[derive(Clone, Default)]
pub struct PowerCutDisk {
    files: Arc<Mutex<BTreeMap<PathBuf, Shared>>>,
}

impl std::fmt::Debug for PowerCutDisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PowerCutDisk")
    }
}

impl PowerCutDisk {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self, path: &Path) -> Shared {
        let mut g = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(g.entry(path.to_path_buf()).or_default())
    }

    /// The backend of the file at `path` (created empty on first use).
    pub fn open(&self, path: &Path) -> PowerCutFile {
        PowerCutFile {
            state: self.state(path),
        }
    }

    /// A factory for [`crate::ServeConfig::storage_backend`].
    pub fn factory(&self) -> BackendFactory {
        let disk = self.clone();
        Arc::new(move |p: &Path| Box::new(disk.open(p)) as Box<dyn StorageBackend>)
    }

    /// Lose power under `dir`: every file there goes back to what was last
    /// synced, and whoever still holds the old handles writes nowhere.
    pub fn power_cut(&self, dir: &Path) {
        self.power_cut_with(dir, CutMode::DropUnsynced);
    }

    /// [`power_cut`](Self::power_cut), keeping what `mode` says of the
    /// unsynced writes (each file draws from its own stream of the seed).
    /// Returns how many unsynced bytes were written back, so a test can
    /// tell a cut that left leftovers from one that had none.
    pub fn power_cut_with(&self, dir: &Path, mode: CutMode) -> u64 {
        let mut g = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut leftovers = 0u64;
        for (i, (path, st)) in g.iter_mut().enumerate() {
            if !path.starts_with(dir) {
                continue;
            }
            let fresh = {
                let mut old = lock(st);
                old.dead = true;
                let img = survivor(&old.durable, &old.pending, mode, i as u64 + 1);
                leftovers += img
                    .iter()
                    .zip(old.durable.iter().chain(std::iter::repeat(&0)))
                    .filter(|(a, b)| a != b)
                    .count() as u64
                    + old.durable.len().saturating_sub(img.len()) as u64;
                FileState {
                    // What is on the platter now is durable.
                    durable: img.clone(),
                    current: img,
                    ..FileState::default()
                }
            };
            *st = Arc::new(Mutex::new(fresh));
        }
        leftovers
    }

    /// The disk under `path` dies right after its next `n`-th sync
    /// completes (`n == 0`: before any further write).
    pub fn fail_after_syncs(&self, path: &Path, n: u64) {
        let st = self.state(path);
        let mut g = lock(&st);
        g.fail_at_sync = Some(g.syncs + n);
        if n == 0 {
            g.dead = true;
        }
    }

    /// The disk under `path` dies at its next `n`-th sync (`n >= 1`),
    /// which fails without taking effect: the writes made since the sync
    /// before stay unsynced, for a [`power_cut_with`](Self::power_cut_with)
    /// to keep parts of. (With [`CutMode::DropUnsynced`] this is the same
    /// crash point as [`fail_after_syncs`](Self::fail_after_syncs)`(n - 1)`.)
    pub fn fail_at_sync(&self, path: &Path, n: u64) {
        assert!(n >= 1, "the first sync is 1");
        let st = self.state(path);
        let mut g = lock(&st);
        g.fail_before_sync = Some(g.syncs + n);
    }

    /// Whether the disk under `path` died.
    pub fn is_dead(&self, path: &Path) -> bool {
        lock(&self.state(path)).dead
    }

    /// Syncs of `path` since it was created (or last power cut).
    pub fn syncs(&self, path: &Path) -> u64 {
        lock(&self.state(path)).syncs
    }

    /// The I/O of `path` since it was created (or last power cut).
    pub fn events(&self, path: &Path) -> Vec<DiskEvent> {
        lock(&self.state(path)).events.clone()
    }

    /// Bytes written to `path` but not synced yet.
    pub fn unsynced(&self, path: &Path) -> bool {
        let st = self.state(path);
        let g = lock(&st);
        g.current != g.durable
    }
}

/// One file of a [`PowerCutDisk`].
#[derive(Debug)]
pub struct PowerCutFile {
    state: Shared,
}

fn dead() -> io::Error {
    io::Error::other("power-cut disk: the disk is gone (testing)")
}

impl StorageBackend for PowerCutFile {
    fn len(&self) -> Result<u64, io::Error> {
        Ok(lock(&self.state).current.len() as u64)
    }

    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, io::Error> {
        let g = lock(&self.state);
        let start = usize::try_from(offset).map_err(io::Error::other)?;
        let end = start
            .checked_add(len)
            .filter(|e| *e <= g.current.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "read past the end"))?;
        Ok(g.current[start..end].to_vec())
    }

    fn set_len(&self, len: u64) -> Result<(), io::Error> {
        let mut g = lock(&self.state);
        if g.dead {
            return Err(dead());
        }
        let n = usize::try_from(len).map_err(io::Error::other)?;
        g.current.resize(n, 0);
        g.pending.push(Pending::SetLen(n));
        g.events.push(DiskEvent::SetLen(len));
        Ok(())
    }

    fn sync_data(&self, _eventual: bool) -> Result<(), io::Error> {
        let mut g = lock(&self.state);
        if g.dead {
            return Err(dead());
        }
        if g.fail_before_sync == Some(g.syncs + 1) {
            g.dead = true;
            return Err(dead());
        }
        g.durable = g.current.clone();
        g.pending.clear();
        g.syncs += 1;
        g.events.push(DiskEvent::Sync);
        if g.fail_at_sync.is_some_and(|n| g.syncs >= n) {
            g.dead = true;
        }
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> Result<(), io::Error> {
        let mut g = lock(&self.state);
        if g.dead {
            return Err(dead());
        }
        let start = usize::try_from(offset).map_err(io::Error::other)?;
        put(&mut g.current, start, data);
        g.pending.push(Pending::Write {
            offset: start,
            data: data.to_vec(),
        });
        g.events.push(DiskEvent::Write {
            offset,
            len: data.len() as u64,
        });
        Ok(())
    }
}

/// A boxed backend as a concrete one (redb's builder takes `impl
/// StorageBackend`).
#[derive(Debug)]
pub struct DynBackend(pub Box<dyn StorageBackend>);

impl StorageBackend for DynBackend {
    fn len(&self) -> Result<u64, io::Error> {
        self.0.len()
    }
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, io::Error> {
        self.0.read(offset, len)
    }
    fn set_len(&self, len: u64) -> Result<(), io::Error> {
        self.0.set_len(len)
    }
    fn sync_data(&self, eventual: bool) -> Result<(), io::Error> {
        self.0.sync_data(eventual)
    }
    fn write(&self, offset: u64, data: &[u8]) -> Result<(), io::Error> {
        self.0.write(offset, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::{Database, TableDefinition};

    const T: TableDefinition<u64, u64> = TableDefinition::new("t");

    fn open(disk: &PowerCutDisk, p: &Path) -> Database {
        Database::builder()
            .create_with_backend(disk.open(p))
            .unwrap()
    }

    fn get(db: &Database, k: u64) -> Option<u64> {
        let rt = db.begin_read().unwrap();
        match rt.open_table(T) {
            Ok(t) => t.get(k).unwrap().map(|v| v.value()),
            Err(_) => None,
        }
    }

    /// A committed (synced) transaction survives a power cut; the old
    /// handle is cut off; a reopen sees the synced state.
    #[test]
    fn a_power_cut_keeps_what_was_synced() {
        let disk = PowerCutDisk::new();
        let p = Path::new("/n/raft.redb");
        let db = open(&disk, p);
        let wt = db.begin_write().unwrap();
        wt.open_table(T).unwrap().insert(1, 10).unwrap();
        wt.commit().unwrap();
        assert!(disk.syncs(p) > 0);
        assert!(!disk.unsynced(p));
        disk.power_cut(Path::new("/n"));
        // The old handle writes nowhere now.
        let wt = db.begin_write().unwrap();
        wt.open_table(T).unwrap().insert(2, 20).unwrap();
        assert!(wt.commit().is_err());
        drop(db);
        let db = open(&disk, p);
        assert_eq!(get(&db, 1), Some(10));
        assert_eq!(get(&db, 2), None);
    }

    /// A transaction committed with no durability is not synced and is
    /// lost by a power cut, and the file still opens.
    #[test]
    fn a_power_cut_drops_unsynced_commits() {
        let disk = PowerCutDisk::new();
        let p = Path::new("/n/g.redb");
        let db = open(&disk, p);
        let wt = db.begin_write().unwrap();
        wt.open_table(T).unwrap().insert(1, 10).unwrap();
        wt.commit().unwrap();
        let mut wt = db.begin_write().unwrap();
        wt.set_durability(redb::Durability::None);
        wt.open_table(T).unwrap().insert(2, 20).unwrap();
        wt.commit().unwrap();
        assert_eq!(get(&db, 2), Some(20));
        assert!(disk.unsynced(p));
        disk.power_cut(Path::new("/n"));
        drop(db);
        let db = open(&disk, p);
        assert_eq!(get(&db, 1), Some(10));
        assert_eq!(get(&db, 2), None);
    }

    /// The seeded modes keep only bytes that were written, in whole pages
    /// except possibly the last kept one, and are reproducible.
    #[test]
    fn seeded_cuts_keep_parts_of_the_unsynced_writes() {
        let durable = vec![0u8; 3 * TEAR_PAGE];
        let pending = vec![
            Pending::Write {
                offset: 100,
                data: vec![1; 2 * TEAR_PAGE],
            },
            Pending::SetLen(4 * TEAR_PAGE),
            Pending::Write {
                offset: 3 * TEAR_PAGE,
                data: vec![2; TEAR_PAGE],
            },
        ];
        assert_eq!(
            survivor(&durable, &pending, CutMode::DropUnsynced, 1),
            durable
        );
        let mut partial = false;
        let mut kept_some = false;
        for seed in 0..200 {
            for mode in [CutMode::Subset { seed }, CutMode::Prefix { seed }] {
                let img = survivor(&durable, &pending, mode, 1);
                assert_eq!(img, survivor(&durable, &pending, mode, 1), "seeded");
                for (i, b) in img.iter().enumerate() {
                    let written = match i {
                        100..=8291 => 1,
                        i if i >= 3 * TEAR_PAGE => 2,
                        _ => 0,
                    };
                    assert!(*b == 0 || *b == written, "{mode:?}: byte {i} = {b}");
                }
                let ones = img.iter().filter(|b| **b == 1).count();
                kept_some |= ones > 0;
                partial |= ones > 0 && ones % TEAR_PAGE != 0 && ones != 2 * TEAR_PAGE;
            }
        }
        assert!(kept_some && partial, "the seeds cover kept and torn writes");
    }

    /// redb's recovery under every mode, across seeds: a power cut in the
    /// middle of a commit (the disk dies after its n-th sync of that
    /// commit, then loses or keeps parts of what was not synced) leaves a
    /// file that opens and holds exactly the last commit that completed or
    /// the one before, never a mix, and every commit that returned is kept.
    #[test]
    fn redb_recovers_from_torn_unsynced_writes() {
        let mut leftovers = 0u64;
        for seed in 0..12u64 {
            for mode in CutMode::all(seed) {
                for k in 1..4u64 {
                    let disk = PowerCutDisk::new();
                    let p = Path::new("/n/t.redb");
                    let db = open(&disk, p);
                    let wt = db.begin_write().unwrap();
                    {
                        let mut t = wt.open_table(T).unwrap();
                        for i in 0..200 {
                            t.insert(i, 1).unwrap();
                        }
                    }
                    wt.commit().unwrap();
                    disk.fail_at_sync(p, k);
                    let wt = db.begin_write().unwrap();
                    {
                        let mut t = wt.open_table(T).unwrap();
                        for i in 0..200 {
                            t.insert(i, 2).unwrap();
                        }
                    }
                    let committed = wt.commit().is_ok();
                    leftovers += disk.power_cut_with(Path::new("/n"), mode);
                    drop(db);
                    let db = Database::builder()
                        .create_with_backend(disk.open(p))
                        .unwrap_or_else(|e| panic!("{mode:?} k={k}: reopen: {e}"));
                    let vals: Vec<_> = (0..200).map(|i| get(&db, i)).collect();
                    let all = |v| vals.iter().all(|x| *x == Some(v));
                    assert!(
                        all(1) || all(2),
                        "{mode:?} k={k}: a torn state {:?}",
                        &vals[..4]
                    );
                    if committed {
                        assert!(all(2), "{mode:?} k={k}: a returned commit was lost");
                    }
                }
            }
        }
        assert!(
            leftovers > 0,
            "the seeded cuts wrote some unsynced bytes back"
        );
    }

    #[test]
    fn a_dead_disk_fails_writes_after_the_armed_sync() {
        let disk = PowerCutDisk::new();
        let p = Path::new("/n/x.redb");
        let db = open(&disk, p);
        disk.fail_after_syncs(p, 1);
        let wt = db.begin_write().unwrap();
        wt.open_table(T).unwrap().insert(1, 10).unwrap();
        // redb syncs once or more per commit; the disk dies at the first.
        let _ = wt.commit();
        assert!(disk.is_dead(p));
        let wt = db.begin_write();
        let failed = match wt {
            Ok(wt) => {
                wt.open_table(T).unwrap().insert(2, 20).unwrap();
                wt.commit().is_err()
            }
            Err(_) => true,
        };
        assert!(failed, "writes after the disk died fail");
        assert!(disk.events(p).iter().any(|e| matches!(e, DiskEvent::Sync)));
    }
}
