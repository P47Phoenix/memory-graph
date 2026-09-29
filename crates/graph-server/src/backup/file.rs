//! [`FileSink`]: `file://<dir>`, a local or mounted directory.
//!
//! A `put` writes `<key>.part-<random>` beside the target, syncs it and
//! renames it over the key, so a reader never sees a partial object under
//! the key; a crash leaves only the `.part-*` temporary, which has no
//! `.meta` and so is never restorable, and the orphan sweep removes it.
use super::{BackupSink, ObjectInfo};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Marks a sink's temporary file (never a key the uploader writes).
pub const PART_MARK: &str = ".part-";

pub struct FileSink {
    root: PathBuf,
}

impl FileSink {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file of `key`; refuses keys that would leave the root.
    fn path(&self, key: &str) -> std::io::Result<PathBuf> {
        let bad = key.is_empty()
            || key.contains('\\')
            || key.starts_with('/')
            || key
                .split('/')
                .any(|s| s.is_empty() || s == "." || s == ".." || s.contains(':'));
        if bad {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("backup key `{key}` is not a relative object name"),
            ));
        }
        let mut p = self.root.clone();
        p.extend(key.split('/'));
        Ok(p)
    }
}

fn sync_dir(dir: &Path) {
    // Directory fsync makes the rename durable on Unix; Windows has no
    // equivalent through std (and NTFS journals the rename).
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

impl BackupSink for FileSink {
    fn put(&self, key: &str, src: &mut dyn Read) -> std::io::Result<u64> {
        let target = self.path(key)?;
        let dir = target
            .parent()
            .expect("a key has a file name")
            .to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let mut part = target.as_os_str().to_owned();
        part.push(format!("{PART_MARK}{:08x}", rand::random::<u32>()));
        let part = PathBuf::from(part);
        let written = (|| {
            let mut f = std::fs::File::create(&part)?;
            let n = std::io::copy(src, &mut f)?;
            f.sync_all()?;
            Ok::<u64, std::io::Error>(n)
        })();
        let n = match written {
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                return Err(e);
            }
        };
        if let Err(e) = crate::raft::snapshot_dir::replace_file(&part, &target) {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        sync_dir(&dir);
        Ok(n)
    }

    fn get(&self, key: &str, dst: &mut dyn Write) -> std::io::Result<u64> {
        let mut f = std::fs::File::open(self.path(key)?)?;
        std::io::copy(&mut f, dst)
    }

    fn list(&self, prefix: &str) -> std::io::Result<Vec<ObjectInfo>> {
        let mut out = Vec::new();
        let mut stack = vec![(self.root.clone(), String::new())];
        while let Some((dir, rel)) = stack.pop() {
            let rd = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            for e in rd {
                let e = e?;
                let name = e.file_name().to_string_lossy().into_owned();
                let key = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                let md = e.metadata()?;
                if md.is_dir() {
                    // Only descend where the prefix can still match.
                    let k = format!("{key}/");
                    if k.starts_with(prefix) || prefix.starts_with(&k) {
                        stack.push((e.path(), key));
                    }
                } else if key.starts_with(prefix) {
                    out.push(ObjectInfo {
                        key,
                        size: md.len(),
                        modified: md.modified()?,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    fn delete(&self, key: &str) -> std::io::Result<()> {
        match std::fs::remove_file(self.path(key)?) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }

    fn describe(&self) -> String {
        format!("file://{}", self.root.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_list_delete_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let s = FileSink::new(d.path().join("b"));
        assert!(s.list("").unwrap().is_empty(), "a missing root lists empty");
        assert_eq!(s.put("c/x.redb", &mut &b"hello"[..]).unwrap(), 5);
        s.put("c/x.meta", &mut &b"{}"[..]).unwrap();
        s.put("other/y.meta", &mut &b"{}"[..]).unwrap();
        let keys: Vec<String> = s.list("c/").unwrap().into_iter().map(|o| o.key).collect();
        assert_eq!(keys, ["c/x.meta", "c/x.redb"]);
        let mut out = Vec::new();
        s.get("c/x.redb", &mut out).unwrap();
        assert_eq!(out, b"hello");
        s.delete("c/x.redb").unwrap();
        s.delete("c/x.redb").unwrap();
        assert_eq!(
            s.get("c/x.redb", &mut Vec::new()).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        for bad in ["", "/abs", "a/../b", "a//b", "a\\b", "C:/x"] {
            assert!(s.put(bad, &mut &b""[..]).is_err(), "{bad}");
        }
    }

    /// A reader failing mid-stream (a crash mid-copy) leaves no object
    /// under the key.
    #[test]
    fn an_interrupted_put_leaves_no_object() {
        struct Broken(usize);
        impl Read for Broken {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 == 0 {
                    return Err(std::io::Error::other("cut"));
                }
                let n = buf.len().min(self.0);
                buf[..n].fill(7);
                self.0 -= n;
                Ok(n)
            }
        }
        let d = tempfile::tempdir().unwrap();
        let s = FileSink::new(d.path());
        assert!(s.put("c/snap-1-5.redb", &mut Broken(1000)).is_err());
        assert!(s.list("").unwrap().is_empty());
    }
}
