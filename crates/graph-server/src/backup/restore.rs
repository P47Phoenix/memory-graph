//! Verified restores (ADR 0006 E8, E10).
//!
//! `--restore file://<dir>/<cluster_id>/snap-T-I.redb` (or `.../latest`,
//! the highest committed index there): read the `.meta`, check the store
//! format and the extractors against this binary, check the disk, download
//! the data to `<store>.restore.tmp` while hashing it, check size and
//! sha256, and only then put it in place ([`crate::paths::place_restore`]).
//! Any refusal removes `<store>.restore.tmp`; the data directory is left
//! without a store, so the same restore can be retried.
//!
//! A plain-path `--restore <file>` verifies a sibling `.meta` the same way
//! when there is one, and warns when there is none (a `cluster snapshot
//! --out` file has none).
use super::{parse_stem, parse_url, BackupSink, FileSink, Location};
use crate::disk::FreeSpaceProbe;
use crate::raft::snapshot_dir::{hex, meta_path_of, read_sidecar, sha256_file, SnapshotSidecar};
use graph_store::StoreError;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;

/// What a restore checks the backup against.
#[derive(Clone)]
pub struct RestoreChecks {
    /// This binary's extractors hash.
    pub extractors_hash: String,
    /// `--restore-allow-extractor-mismatch`: accept a backup made by other
    /// extractors (the store stays valid; affected files re-extract on the
    /// next index). The format version check has no override.
    pub allow_extractor_mismatch: bool,
    /// `--min-free-disk` (0: only the object's own size is needed).
    pub min_free_disk: u64,
    /// The free-space probe (`None`: the check is skipped).
    pub probe: Option<FreeSpaceProbe>,
}

fn refuse(msg: String) -> StoreError {
    StoreError::Rejected(format!("--restore: {msg}"))
}

/// The version checks of a sidecar against this binary.
pub fn check_versions(side: &SnapshotSidecar, c: &RestoreChecks) -> Result<(), StoreError> {
    if side.store_format_version != graph_store::SCHEMA_VERSION {
        return Err(refuse(format!(
            "the backup is store format {} and this binary reads format {} (no override)",
            side.store_format_version,
            graph_store::SCHEMA_VERSION
        )));
    }
    if side.extractors_hash != c.extractors_hash {
        if !c.allow_extractor_mismatch {
            return Err(refuse(format!(
                "the backup was made with extractors {} and this binary has {}; restore with \
                 the same build, or pass --restore-allow-extractor-mismatch (files re-extract \
                 on their next index)",
                side.extractors_hash, c.extractors_hash
            )));
        }
        tracing::warn!(
            backup = %side.extractors_hash,
            binary = %c.extractors_hash,
            "--restore-allow-extractor-mismatch: restoring a backup made with other extractors"
        );
    }
    Ok(())
}

/// A failed attempt at one backup: `next` says whether `latest` may fall
/// back to the next-highest committed pair (a damaged or incomplete pair),
/// or must stop (versions, disk, local I/O).
struct Attempt {
    err: StoreError,
    next: bool,
}

fn stop(err: StoreError) -> Attempt {
    Attempt { err, next: false }
}

fn skip(err: StoreError) -> Attempt {
    Attempt { err, next: true }
}

/// The committed stems (`<dir_key>snap-T-I`) under `dir_key`, highest
/// index first.
fn committed(sink: &dyn BackupSink, dir_key: &str) -> Result<Vec<String>, StoreError> {
    let objects = sink
        .list(dir_key)
        .map_err(|e| refuse(format!("listing `{}`: {e}", sink.describe())))?;
    let mut metas: Vec<((u64, u64), String)> = objects
        .iter()
        .filter(|o| o.key.ends_with(".meta") && !o.key[dir_key.len()..].contains('/'))
        .filter_map(|o| parse_stem(&o.key).map(|(t, i)| ((i, t), o.key.clone())))
        .collect();
    metas.sort_unstable_by(|a, b| b.cmp(a));
    Ok(metas
        .into_iter()
        .map(|(_, k)| k.strip_suffix(".meta").expect("a meta").to_string())
        .collect())
}

/// Read the sidecar of `stem`.
fn fetch_meta(sink: &dyn BackupSink, stem: &str) -> Result<SnapshotSidecar, Attempt> {
    let meta_key = format!("{stem}.meta");
    let mut buf = Vec::new();
    match sink.get(&meta_key, &mut buf) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(skip(refuse(format!(
                "`{meta_key}` does not exist, so `{stem}.redb` is not a committed backup (an \
                 upload that never finished)"
            ))))
        }
        Err(e) => return Err(stop(refuse(format!("reading `{meta_key}`: {e}")))),
    }
    serde_json::from_slice(&buf)
        .map_err(|e| skip(refuse(format!("`{meta_key}` is not a snapshot meta: {e}"))))
}

/// Writes through to a file, hashing and counting.
struct HashingFile {
    f: std::fs::File,
    hash: Sha256,
    n: u64,
}

impl Write for HashingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let k = self.f.write(buf)?;
        self.hash.update(&buf[..k]);
        self.n += k as u64;
        Ok(k)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.f.flush()
    }
}

/// Download, verify and place the backup `stem` into `store`.
fn restore_one(
    sink: &dyn BackupSink,
    stem: &str,
    store: &Path,
    c: &RestoreChecks,
) -> Result<SnapshotSidecar, Attempt> {
    let side = fetch_meta(sink, stem)?;
    let data_key = format!("{stem}.redb");
    check_versions(&side, c).map_err(stop)?;
    if let Some(probe) = &c.probe {
        let dir = store
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if let Some(free) = probe(dir) {
            let need = c.min_free_disk.saturating_add(side.size);
            if free < need {
                return Err(stop(StoreError::Storage(format!(
                    "disk full: --restore needs {need} bytes free in `{}` (the backup's {} \
                     plus --min-free-disk {}), and {free} are",
                    dir.display(),
                    side.size,
                    c.min_free_disk
                ))));
            }
        }
    }
    let tmp = crate::paths::restore_tmp(store);
    let _ = std::fs::remove_file(&tmp);
    let fail = |e: StoreError| {
        let _ = std::fs::remove_file(&tmp);
        e
    };
    let mut w = HashingFile {
        f: std::fs::File::create(&tmp)
            .map_err(|e| stop(refuse(format!("creating `{}`: {e}", tmp.display()))))?,
        hash: Sha256::new(),
        n: 0,
    };
    let got = sink
        .get(&data_key, &mut w)
        .and_then(|n| w.f.sync_all().map(|()| n));
    if let Err(e) = got {
        drop(w);
        let next = e.kind() == std::io::ErrorKind::NotFound;
        return Err(Attempt {
            err: fail(refuse(format!("downloading `{data_key}`: {e}"))),
            next,
        });
    }
    let sha = hex(&w.hash.clone().finalize());
    let n = w.n;
    drop(w);
    if n != side.size || sha != side.sha256 {
        return Err(skip(fail(refuse(format!(
            "`{data_key}` is {n} bytes with sha256 {sha}, but its meta records {} bytes with \
             sha256 {}: the backup is damaged; refusing it",
            side.size, side.sha256
        )))));
    }
    crate::paths::place_restore(&tmp, store).map_err(stop)?;
    tracing::info!(
        from = %format!("{}/{data_key}", sink.describe()),
        index = side.index,
        term = side.term,
        "restored a verified backup"
    );
    Ok(side)
}

/// Download, verify and place a backup from `sink` into `store`: `name` is
/// `snap-T-I.redb` under `dir_key` (`""` or `<something>/`), or `latest`,
/// which falls back to the next-highest committed pair when one fails
/// verification (e.g. a pair two nodes sharing a URL wrote interleaved).
pub fn restore_from_sink(
    sink: &dyn BackupSink,
    dir_key: &str,
    name: &str,
    store: &Path,
    c: &RestoreChecks,
) -> Result<SnapshotSidecar, StoreError> {
    if name != "latest" {
        let valid = name
            .strip_suffix(".redb")
            .filter(|s| parse_stem(&format!("{s}.redb")).is_some());
        let Some(stem) = valid else {
            return Err(refuse(format!(
                "`{name}`: name a backup (`snap-<term>-<index>.redb`) or `latest`"
            )));
        };
        return restore_one(sink, &format!("{dir_key}{stem}"), store, c).map_err(|a| a.err);
    }
    let stems = committed(sink, dir_key)?;
    let mut last = None;
    for stem in &stems {
        match restore_one(sink, stem, store, c) {
            Ok(side) => return Ok(side),
            Err(Attempt { err, next: true }) => {
                tracing::warn!(backup = %stem, error = %err, "--restore latest: skipping it");
                last = Some(err);
            }
            Err(Attempt { err, .. }) => return Err(err),
        }
    }
    Err(match last {
        Some(e) => refuse(format!(
            "no committed backup under {}/{dir_key} verifies; the newest failure: {e}",
            sink.describe()
        )),
        None => refuse(format!(
            "no committed backup (no snap-*.meta) under {}/{dir_key}",
            sink.describe()
        )),
    })
}
/// `--restore <url>`: a `file://` URL naming `.../snap-T-I.redb` or
/// `.../latest`.
pub fn restore_from_url(url: &str, store: &Path, c: &RestoreChecks) -> Result<(), StoreError> {
    let Location::File(path) = parse_url(url).map_err(refuse)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| refuse(format!("`{url}` names no snapshot (or `latest`)")))?;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let sink = FileSink::new(dir);
    restore_from_sink(&sink, "", &name, store, c).map(|_| ())
}

/// `--restore <file>`: verify a sibling `.meta` if there is one (warn if
/// not), then [`crate::paths::restore_into`].
pub fn restore_from_path(
    snapshot: &Path,
    store: &Path,
    c: &RestoreChecks,
) -> Result<(), StoreError> {
    let meta = meta_path_of(snapshot);
    if meta.exists() {
        let side = read_sidecar(snapshot).map_err(|e| refuse(e.to_string()))?;
        check_versions(&side, c)?;
        let (sha, size) = sha256_file(snapshot)?;
        if size != side.size || sha != side.sha256 {
            return Err(refuse(format!(
                "`{}` is {size} bytes with sha256 {sha}, but `{}` records {} bytes with sha256 \
                 {}: refusing it",
                snapshot.display(),
                meta.display(),
                side.size,
                side.sha256
            )));
        }
        tracing::info!(meta = %meta.display(), "--restore: the snapshot matches its .meta");
    } else {
        tracing::warn!(
            snapshot = %snapshot.display(),
            "--restore: no .meta beside the snapshot, so its checksum and versions cannot be \
             verified (restoring anyway; a file from `cluster snapshot --out` has none)"
        );
    }
    crate::paths::restore_into(snapshot, store)
}
