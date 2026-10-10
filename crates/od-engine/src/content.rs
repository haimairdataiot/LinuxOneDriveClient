//! Local file content, keyed by inode number.
//!
//! Layout under the account directory:
//!   content/<ino % 256 as 2 hex>/<ino>   current content of a hydrated file
//!   partial/<ino>.<nonce>                in-progress download (same filesystem)
//!   quarantine/                          content preserved after DB recovery
//!
//! Corruption prevention:
//! * downloads never write into `content/` directly: they stream into
//!   `partial/`, are verified (size + QuickXorHash) and fsynced by the Graph
//!   client, then atomically `rename(2)`d over the old content and the
//!   directory is fsynced. A crash leaves either the old or the new version,
//!   never a mix;
//! * eviction ("Free up space") only removes content the caller has checked
//!   to be clean (identical to the server).

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A copy of a file deleted online, kept for a while.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TrashEntry {
    pub id: String,
    pub name: String,
    /// Path relative to the OneDrive folder where the file was.
    pub rel_path: String,
    /// Unix seconds.
    pub removed_at: i64,
    pub size: u64,
}

pub struct ContentStore {
    root: PathBuf,
    nonce: AtomicU64,
}

impl ContentStore {
    pub fn open(account_dir: &Path) -> io::Result<ContentStore> {
        for sub in ["content", "partial", "quarantine", "trash"] {
            od_core::paths::ensure_private_dir(&account_dir.join(sub))?;
        }
        let cs = ContentStore {
            root: account_dir.to_path_buf(),
            nonce: AtomicU64::new(std::process::id() as u64 * 1_000_000),
        };
        cs.clean_partials();
        Ok(cs)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn shard(ino: u64) -> String {
        format!("{:02x}", ino % 256)
    }

    pub fn path(&self, ino: u64) -> PathBuf {
        self.root.join("content").join(Self::shard(ino)).join(ino.to_string())
    }

    pub fn exists(&self, ino: u64) -> bool {
        self.path(ino).exists()
    }

    /// A fresh path for an in-progress download.
    pub fn partial_path(&self, ino: u64) -> PathBuf {
        let n = self.nonce.fetch_add(1, Ordering::Relaxed);
        self.root.join("partial").join(format!("{ino}.{n}"))
    }

    /// A stable partial path for resumable downloads of `ino`.
    pub fn resumable_partial_path(&self, ino: u64) -> PathBuf {
        self.root.join("partial").join(format!("{ino}.resume"))
    }

    fn ensure_shard(&self, ino: u64) -> io::Result<PathBuf> {
        let dir = self.root.join("content").join(Self::shard(ino));
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }
        Ok(dir)
    }

    /// Create (or truncate) empty content for a new local file.
    pub fn create_empty(&self, ino: u64) -> io::Result<File> {
        self.ensure_shard(ino)?;
        OpenOptions::new().read(true).write(true).create(true).truncate(true).mode(0o600).open(self.path(ino))
    }

    pub fn open_read(&self, ino: u64) -> io::Result<File> {
        File::open(self.path(ino))
    }

    pub fn open_rw(&self, ino: u64) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).open(self.path(ino))
    }

    /// Atomically replace the content of `ino` with a verified, fsynced partial file.
    pub fn install(&self, ino: u64, partial: &Path) -> io::Result<()> {
        let dir = self.ensure_shard(ino)?;
        fs::rename(partial, self.path(ino))?;
        fsync_dir(&dir)
    }

    /// Move content from one inode to another (atomic-save rename handling,
    /// conflict copies).
    pub fn transfer(&self, from: u64, to: u64) -> io::Result<()> {
        let dir = self.ensure_shard(to)?;
        fs::rename(self.path(from), self.path(to))?;
        fsync_dir(&dir)
    }

    /// Copy content (used when a conflict copy must keep the original too).
    pub fn copy(&self, from: u64, to: u64) -> io::Result<()> {
        self.ensure_shard(to)?;
        let tmp = self.partial_path(to);
        fs::copy(self.path(from), &tmp)?;
        File::open(&tmp)?.sync_all()?;
        self.install(to, &tmp)
    }

    /// Drop content. Open file descriptors keep working (POSIX unlink semantics).
    pub fn remove(&self, ino: u64) -> io::Result<()> {
        match fs::remove_file(self.path(ino)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Keep a copy of content we could not reconcile (DB rebuilt, unknown items).
    pub fn quarantine(&self, ino: u64, name: &str) -> io::Result<PathBuf> {
        let safe: String = name.chars().map(|c| if c == '/' { '_' } else { c }).collect();
        let dest =
            self.root.join("quarantine").join(format!("{}-{ino}-{safe}", chrono::Utc::now().format("%Y%m%d%H%M%S")));
        fs::rename(self.path(ino), &dest)?;
        Ok(dest)
    }

    /// Tag a content file with its item name and whether it holds unsynced
    /// changes, so a salvage pass can recover it even without the database.
    pub fn tag(&self, ino: u64, name: &str, dirty: bool) {
        let p = self.path(ino);
        let _ = set_xattr(&p, "user.lod.name", name.as_bytes());
        let _ = set_xattr(&p, "user.lod.dirty", if dirty { b"1" } else { b"0" });
    }

    /// Tag a file that is not yet installed (a verified partial download).
    pub fn tag_path(&self, path: &Path, name: &str, dirty: bool) {
        let _ = set_xattr(path, "user.lod.name", name.as_bytes());
        let _ = set_xattr(path, "user.lod.dirty", if dirty { b"1" } else { b"0" });
    }

    /// (name, dirty) recorded by [`ContentStore::tag`]. Untagged content counts as dirty:
    /// when in doubt, keep it.
    pub fn read_tag(&self, ino: u64) -> (String, bool) {
        let p = self.path(ino);
        let name = get_xattr(&p, "user.lod.name").map(|v| String::from_utf8_lossy(&v).into_owned()).unwrap_or_default();
        let dirty = get_xattr(&p, "user.lod.dirty").map(|v| v != b"0").unwrap_or(true);
        (name, dirty)
    }

    /// Keep the clean cached copy of a file deleted online for a while
    /// ("Recently removed"), instead of deleting it. The original path is
    /// recorded so the file can be restored to where it was.
    pub fn trash(&self, ino: u64, name: &str, rel_path: &str) -> io::Result<()> {
        if !self.exists(ino) {
            return Ok(());
        }
        let src = self.path(ino);
        let _ = set_xattr(&src, "user.lod.relpath", rel_path.as_bytes());
        let _ = set_xattr(&src, "user.lod.name", name.as_bytes());
        let safe: String = name.chars().map(|c| if c == '/' { '_' } else { c }).collect();
        let dest = self.root.join("trash").join(format!("{}-{ino}-{safe}", chrono::Utc::now().timestamp()));
        fs::rename(src, dest)
    }

    /// Copies of files deleted online that are still kept, newest first.
    pub fn list_trash(&self) -> Vec<TrashEntry> {
        let mut out = Vec::new();
        if let Ok(rd) = fs::read_dir(self.root.join("trash")) {
            for e in rd.flatten() {
                let id = e.file_name().to_string_lossy().into_owned();
                let mut parts = id.splitn(3, '-');
                let removed_at: i64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                let fallback_name = parts.nth(1).unwrap_or("").to_string();
                let path = e.path();
                let name = get_xattr(&path, "user.lod.name")
                    .map(|v| String::from_utf8_lossy(&v).into_owned())
                    .unwrap_or(fallback_name);
                let rel_path = get_xattr(&path, "user.lod.relpath")
                    .map(|v| String::from_utf8_lossy(&v).into_owned())
                    .unwrap_or_else(|_| name.clone());
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                out.push(TrashEntry { id, name, rel_path, removed_at, size });
            }
        }
        out.sort_by(|a, b| b.removed_at.cmp(&a.removed_at));
        out
    }

    /// Location of a kept copy (validated: only names from `list_trash`).
    pub fn trash_path(&self, id: &str) -> Option<PathBuf> {
        if id.contains('/') || id.starts_with('.') {
            return None;
        }
        let p = self.root.join("trash").join(id);
        p.is_file().then_some(p)
    }

    /// Delete trashed copies older than `max_age`.
    pub fn purge_trash(&self, max_age: std::time::Duration) -> usize {
        let cutoff = chrono::Utc::now().timestamp() - max_age.as_secs() as i64;
        let mut n = 0;
        if let Ok(rd) = fs::read_dir(self.root.join("trash")) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let ts: i64 = name.split('-').next().and_then(|v| v.parse().ok()).unwrap_or(0);
                if ts < cutoff && fs::remove_file(e.path()).is_ok() {
                    n += 1;
                }
            }
        }
        n
    }

    /// Size of the cached content on disk.
    pub fn len(&self, ino: u64) -> io::Result<u64> {
        Ok(fs::metadata(self.path(ino))?.len())
    }

    /// Bytes available to us on the filesystem holding the content.
    pub fn free_space(&self) -> io::Result<u64> {
        free_space(&self.root)
    }

    /// Remove stale partial downloads left by a crash (non-resumable ones).
    pub fn clean_partials(&self) {
        if let Ok(rd) = fs::read_dir(self.root.join("partial")) {
            for e in rd.flatten() {
                let name = e.file_name();
                if !name.to_string_lossy().ends_with(".resume") {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
    }

    /// Inodes that have content on disk (for consistency checks).
    pub fn all_inodes(&self) -> Vec<u64> {
        let mut out = Vec::new();
        if let Ok(shards) = fs::read_dir(self.root.join("content")) {
            for shard in shards.flatten() {
                if let Ok(files) = fs::read_dir(shard.path()) {
                    for f in files.flatten() {
                        if let Ok(ino) = f.file_name().to_string_lossy().parse::<u64>() {
                            out.push(ino);
                        }
                    }
                }
            }
        }
        out
    }
}

fn cpath(p: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| io::ErrorKind::InvalidInput.into())
}

pub fn set_xattr(p: &Path, name: &str, value: &[u8]) -> io::Result<()> {
    let cp = cpath(p)?;
    let cn = std::ffi::CString::new(name).map_err(|_| io::ErrorKind::InvalidInput)?;
    // SAFETY: valid NUL-terminated strings and a buffer of the given length.
    let rc = unsafe { libc::setxattr(cp.as_ptr(), cn.as_ptr(), value.as_ptr().cast(), value.len(), 0) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

pub fn get_xattr(p: &Path, name: &str) -> io::Result<Vec<u8>> {
    let cp = cpath(p)?;
    let cn = std::ffi::CString::new(name).map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut buf = vec![0u8; 4096];
    // SAFETY: as above; the kernel writes at most buf.len() bytes.
    let n = unsafe { libc::getxattr(cp.as_ptr(), cn.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(n as usize);
    Ok(buf)
}

pub fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

pub fn free_space(path: &Path) -> io::Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid C string and out-pointer.
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st.f_bavail * st.f_frsize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn install_transfer_remove() {
        let dir = tempfile::tempdir().unwrap();
        let cs = ContentStore::open(dir.path()).unwrap();
        let mut f = cs.create_empty(300).unwrap();
        f.write_all(b"v1").unwrap();
        assert_eq!(fs::read(cs.path(300)).unwrap(), b"v1");
        assert!(cs.path(300).to_string_lossy().contains("/content/2c/300"));

        let p = cs.partial_path(300);
        fs::write(&p, b"v2").unwrap();
        // A reader holding the old file keeps seeing v1 after the swap.
        let old = cs.open_read(300).unwrap();
        cs.install(300, &p).unwrap();
        assert_eq!(fs::read(cs.path(300)).unwrap(), b"v2");
        let mut buf = String::new();
        use std::io::Read;
        (&old).read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "v1");

        // Tags survive and default to "dirty" when missing.
        cs.tag(300, "report.docx", false);
        assert_eq!(cs.read_tag(300), ("report.docx".to_string(), false));
        cs.tag(300, "report.docx", true);
        assert!(cs.read_tag(300).1);

        cs.transfer(300, 7).unwrap();
        assert!(!cs.exists(300) && cs.exists(7));
        cs.copy(7, 8).unwrap();
        assert_eq!(fs::read(cs.path(8)).unwrap(), b"v2");
        cs.remove(7).unwrap();
        cs.remove(7).unwrap();
        // Kept copies remember where they came from.
        cs.create_empty(9).unwrap();
        cs.trash(9, "gone.txt", "Docs/gone.txt").unwrap();
        let t = cs.list_trash();
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].name.as_str(), t[0].rel_path.as_str()), ("gone.txt", "Docs/gone.txt"));
        assert!(cs.trash_path(&t[0].id).is_some());
        assert!(cs.trash_path("../content").is_none());
        let mut inos = cs.all_inodes();
        inos.sort();
        assert_eq!(inos, vec![8]);
        assert!(cs.free_space().unwrap() > 0);

        // Stale partials are cleaned on open; resumable ones are kept.
        fs::write(cs.partial_path(1), b"x").unwrap();
        fs::write(cs.resumable_partial_path(2), b"x").unwrap();
        let cs2 = ContentStore::open(dir.path()).unwrap();
        let left: Vec<_> = fs::read_dir(cs2.root().join("partial")).unwrap().flatten().collect();
        assert_eq!(left.len(), 1);
    }
}
