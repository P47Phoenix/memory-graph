//! Verified restores (ADR 0006 E8, E10).
//!
//! `--restore file://<dir>/<cluster_id>/snap-T-I.redb` (or `.../latest`,
//! the highest committed index there): read the `.meta`, check the store
//! format and the extractors against this binary, check the disk, download
//! the data to `<store>.restore.tmp` while hashing it, check size and
//! sha256, and only then put it in place ([`crate::paths::place_restore`]).
//! Any refusal removes `<store>.restore.tmp`, the only file written.
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

/// Resolve `key` (a `snap-T-I.redb` name or `latest`) in `sink`, relative
/// to `dir_key` (`""` or `<something>/`): the data key and its sidecar.
fn resolve(
    sink: &dyn BackupSink,
    dir_key: &str,
    name: &str,
) -> Result<(String, SnapshotSidecar), StoreError> {
    let io = |what: &str, e: std::io::Error| refuse(format!("{what}: {e}"));
    let stem = if name == "latest" {
        let objects = sink
            .list(dir_key)
            .map_err(|e| io(&format!("listing `{}`", sink.describe()), e))?;
        let best = objects
            .iter()
            .filter(|o| o.key.ends_with(".meta") && !o.key[dir_key.len()..].contains('/'))
            .filter_map(|o| parse_stem(&o.key).map(|(t, i)| ((i, t), o.key.clone())))
            .max();
        let Some((_, meta)) = best else {
            return Err(refuse(format!(
                "no committed backup (no snap-*.meta) under {}/{dir_key}",
                sink.describe()
            )));
        };
        meta.strip_suffix(".meta").expect("a meta").to_string()
    } else {
        let Some(stem) = name.strip_suffix(".redb") else {
            return Err(refuse(format!(
                "`{name}`: name a snapshot (`snap-<term>-<index>.redb`) or `latest`"
            )));
        };
        format!("{dir_key}{stem}")
    };
    let meta_key = format!("{stem}.meta");
    let mut buf = Vec::new();
    match sink.get(&meta_key, &mut buf) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(refuse(format!(
                "`{meta_key}` does not exist, so `{stem}.redb` is not a committed backup (an \
                 upload that never finished)"
            )))
        }
        Err(e) => return Err(io(&format!("reading `{meta_key}`"), e)),
    }
    let side: SnapshotSidecar = serde_json::from_slice(&buf)
        .map_err(|e| refuse(format!("`{meta_key}` is not a snapshot meta: {e}")))?;
    Ok((format!("{stem}.redb"), side))
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

/// Download, verify and place a backup from `sink` into `store`.
pub fn restore_from_sink(
    sink: &dyn BackupSink,
    dir_key: &str,
    name: &str,
    store: &Path,
    c: &RestoreChecks,
) -> Result<SnapshotSidecar, StoreError> {
    let (data_key, side) = resolve(sink, dir_key, name)?;
    check_versions(&side, c)?;
    if let Some(probe) = &c.probe {
        let dir = store
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if let Some(free) = probe(dir) {
            let need = c.min_free_disk.saturating_add(side.size);
            if free < need {
                return Err(StoreError::Storage(format!(
                    "disk full: --restore needs {need} bytes free in `{}` (the backup's {} \
                     plus --min-free-disk {}), and {free} are",
                    dir.display(),
                    side.size,
                    c.min_free_disk
                )));
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
            .map_err(|e| refuse(format!("creating `{}`: {e}", tmp.display())))?,
        hash: Sha256::new(),
        n: 0,
    };
    let got = sink
        .get(&data_key, &mut w)
        .and_then(|n| w.f.sync_all().map(|()| n));
    if let Err(e) = got {
        drop(w);
        return Err(fail(refuse(format!("downloading `{data_key}`: {e}"))));
    }
    let sha = hex(&w.hash.clone().finalize());
    let n = w.n;
    drop(w);
    if n != side.size || sha != side.sha256 {
        return Err(fail(refuse(format!(
            "`{data_key}` is {n} bytes with sha256 {sha}, but its meta records {} bytes with \
             sha256 {}: the backup is damaged; refusing it",
            side.size, side.sha256
        ))));
    }
    crate::paths::place_restore(&tmp, store)?;
    tracing::info!(
        from = %format!("{}/{data_key}", sink.describe()),
        index = side.index,
        term = side.term,
        "restored a verified backup"
    );
    Ok(side)
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
