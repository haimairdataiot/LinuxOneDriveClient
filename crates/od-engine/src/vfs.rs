//! The FUSE filesystem: Files On-Demand.
//!
//! * Browsing (lookup/getattr/readdir) is answered from SQLite only: instant
//!   and available offline.
//! * Opening an online-only file for reading is lazy: small reads near the
//!   start (content-type sniffing) are served with a ranged request; any
//!   other read downloads (hydrates) the file once, verified, then reads
//!   locally. Thumbnailers and indexers never trigger downloads.
//! * Every namespace change is committed to the store before the reply, so
//!   the kernel never sees a state the database does not have.
//! * Rename-over (the atomic-save pattern of editors) hands the server
//!   identity of the replaced file to the new one, preserving version
//!   history and sharing.

use crate::engine::{Engine, NoticeKind, now_ms, now_ns};
use crate::guard::{Caller, Guard};
use crate::shortcuts::{LINK_INO_BASE, LinkEntry};
use crate::store::{self, Item, LocalOnly, NewItem, ROOT_INO, StoreError};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, FopenFlags, Generation, INodeNo, KernelConfig, LockOwner, OpenFlags,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use od_core::types::{PinState, xattr};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(10);
/// Reads entirely inside this prefix of an online-only file are served with a
/// ranged request instead of a full download (GIO sniffs up to 16 KiB; the
/// kernel's readahead rounds that up).
const PREFIX_WINDOW: u64 = 512 * 1024;
/// Files up to this size are simply downloaded on first read.
const SMALL_FILE: u64 = 2 * 1024 * 1024;

struct Handle {
    ino: u64,
    write: bool,
    background: bool,
    file: Mutex<Option<Arc<File>>>,
    prefix: Mutex<Option<Arc<Vec<u8>>>>,
    wrote: AtomicBool,
}

struct DirSnapshot {
    entries: Vec<(u64, FileType, String)>,
}

pub struct OneDriveFs {
    engine: Arc<Engine>,
    rt: tokio::runtime::Handle,
    handles: Arc<Mutex<HashMap<u64, Arc<Handle>>>>,
    dirs: Mutex<HashMap<u64, Arc<DirSnapshot>>>,
    next_fh: AtomicU64,
    guard: Guard,
    uid: u32,
    gid: u32,
    /// Online-only files are opened with direct I/O so download errors reach
    /// the application as-is (ENETUNREACH, ENOSPC) instead of the page
    /// cache's generic EIO, while mmap keeps working (kernel 6.6+).
    direct_lazy: bool,
}

fn errno(code: i32) -> Errno {
    Errno::from_i32(code)
}

fn store_errno(e: &StoreError) -> Errno {
    match e {
        StoreError::NotFound => Errno::ENOENT,
        StoreError::Exists => Errno::EEXIST,
        _ => Errno::EIO,
    }
}

fn ts(ns: i64) -> SystemTime {
    if ns <= 0 { UNIX_EPOCH } else { UNIX_EPOCH + Duration::from_nanos(ns as u64) }
}

fn to_ns(t: TimeOrNow) -> i64 {
    match t {
        TimeOrNow::Now => now_ns(),
        TimeOrNow::SpecificTime(st) => match st.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i64,
            Err(_) => 0,
        },
    }
}

fn file_attr(engine: &Engine, uid: u32, gid: u32, it: &Item) -> FileAttr {
    let size = if it.is_dir { 4096 } else { engine.size_override(it.ino).unwrap_or(it.size) };
    let blocks = if it.is_dir || it.hydrated { size.div_ceil(512) } else { 0 };
    let perm = if it.mode & 0o7777 != 0 {
        (it.mode & 0o7777) as u16
    } else if it.is_dir {
        0o755
    } else {
        0o644
    };
    FileAttr {
        ino: INodeNo(it.ino),
        size,
        blocks,
        atime: ts(it.mtime_ns),
        mtime: ts(it.mtime_ns),
        ctime: ts(it.mtime_ns.max(it.ctime_ns)),
        crtime: ts(it.ctime_ns),
        kind: if it.is_dir { FileType::Directory } else { FileType::RegularFile },
        perm,
        nlink: if it.is_dir { 2 } else { 1 },
        uid,
        gid,
        rdev: 0,
        blksize: 64 * 1024,
        flags: 0,
    }
}

/// A shortcut to a shared folder, shown as a symbolic link to its mount.
fn link_attr(uid: u32, gid: u32, e: &LinkEntry) -> FileAttr {
    static SHOWN_SINCE: std::sync::OnceLock<SystemTime> = std::sync::OnceLock::new();
    let t = *SHOWN_SINCE.get_or_init(SystemTime::now);
    let size = e.target.as_os_str().len() as u64;
    FileAttr {
        ino: INodeNo(e.ino),
        size,
        blocks: 0,
        atime: t,
        mtime: t,
        ctime: t,
        crtime: t,
        kind: FileType::Symlink,
        perm: 0o777,
        nlink: 1,
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn visible(it: &Item) -> bool {
    !it.deleted && it.local_only != LocalOnly::Excluded
}

impl OneDriveFs {
    pub fn new(engine: Arc<Engine>, rt: tokio::runtime::Handle) -> Self {
        let cfg = engine.cfg();
        OneDriveFs {
            engine,
            rt,
            handles: Arc::new(Mutex::new(HashMap::new())),
            dirs: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            guard: Guard::default(),
            uid: cfg.uid,
            gid: cfg.gid,
            direct_lazy: false,
        }
    }

    fn attr(&self, it: &Item) -> FileAttr {
        file_attr(&self.engine, self.uid, self.gid, it)
    }

    fn live(&self, ino: u64) -> Result<Item, Errno> {
        match store::get_live(&self.engine.store.lock(), ino) {
            Ok(Some(it)) if visible(&it) || it.ino == ROOT_INO => Ok(it),
            Ok(_) => Err(Errno::ENOENT),
            Err(e) => Err(store_errno(&e)),
        }
    }

    fn new_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::Relaxed)
    }

    fn add_handle(&self, h: Handle) -> u64 {
        let fh = self.new_fh();
        self.engine.note_open(h.ino, h.write);
        self.handles.lock().unwrap().insert(fh, Arc::new(h));
        fh
    }

    fn handle(&self, fh: FileHandle) -> Option<Arc<Handle>> {
        self.handles.lock().unwrap().get(&fh.0).cloned()
    }

    /// First write through a handle marks the item dirty (needs upload).
    fn mark_written(&self, h: &Handle) {
        if !h.wrote.swap(true, Ordering::SeqCst) {
            let c = self.engine.store.lock();
            let _ = store::mark_dirty(&c, h.ino);
            if let Ok(Some(it)) = store::get(&c, h.ino) {
                self.engine.content.tag(h.ino, &it.name, true);
            }
        }
    }

    /// Validate a new name and return it as UTF-8.
    fn new_name<'a>(&self, name: &'a OsStr) -> Result<&'a str, Errno> {
        let s = name.to_str().ok_or(Errno::EINVAL)?;
        if s.is_empty() || s.contains('/') || s.len() > 255 {
            return Err(Errno::EINVAL);
        }
        Ok(s)
    }

    /// Insert a new local item (file or folder) and classify it.
    fn create_item(&self, parent: u64, name: &str, is_dir: bool, mode: u32) -> Result<Item, Errno> {
        let parent_item = self.live(parent)?;
        if !parent_item.is_dir {
            return Err(Errno::ENOTDIR);
        }
        if self.engine.link_child(parent, name).is_some() {
            return Err(Errno::EEXIST);
        }
        let (local_only, issue) = self.engine.classify_name(&parent_item, name);
        let now = now_ns();
        let ino = {
            let c = self.engine.store.lock();
            let ino = store::insert(
                &c,
                &NewItem {
                    parent_ino: parent,
                    name: name.to_string(),
                    is_dir,
                    mode: mode & 0o7777,
                    mtime_ns: now,
                    ctime_ns: now,
                    hydrated: !is_dir,
                    dirty: !is_dir,
                    local_only: Some(local_only),
                    ..Default::default()
                },
            )
            .map_err(|e| store_errno(&e))?;
            if let Some(issue) = issue {
                let _ = store::set_error(&c, ino, issue.code(), &issue.to_string(), now_ms(), true);
            }
            ino
        };
        if !is_dir {
            if let Err(e) = self.engine.content.create_empty(ino) {
                let _ = store::remove_row(&self.engine.store.lock(), ino);
                return Err(errno(e.raw_os_error().unwrap_or(libc::EIO)));
            }
            self.engine.content.tag(ino, name, true);
        }
        self.engine.local_changed();
        self.live(ino)
    }

    fn open_content(&self, ino: u64, write: bool) -> std::io::Result<File> {
        if write { self.engine.content.open_rw(ino) } else { self.engine.content.open_read(ino) }
    }

    /// Before a writer touches a file that is being uploaded, give it its own
    /// copy so the upload keeps reading an immutable snapshot.
    fn detach_if_uploading(&self, ino: u64) -> std::io::Result<()> {
        if self.engine.uploading.lock().unwrap().contains(&ino) {
            self.engine.content.copy(ino, ino)?;
        }
        Ok(())
    }

    fn free_space_ok(&self) -> bool {
        // Below the critical level, writes that grow files fail with ENOSPC so the
        // database can always commit.
        let critical = self.engine.cfg().critical_free;
        self.engine.content.free_space().map(|f| f >= critical).unwrap_or(true)
    }
}

impl fuser::Filesystem for OneDriveFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        use fuser::InitFlags;
        for flag in [
            InitFlags::FUSE_ATOMIC_O_TRUNC,
            InitFlags::FUSE_DO_READDIRPLUS,
            InitFlags::FUSE_READDIRPLUS_AUTO,
            InitFlags::FUSE_PARALLEL_DIROPS,
        ] {
            let _ = config.add_capabilities(flag);
        }
        if config.capabilities().contains(InitFlags::FUSE_DIRECT_IO_ALLOW_MMAP)
            && config.add_capabilities(InitFlags::FUSE_DIRECT_IO_ALLOW_MMAP).is_ok()
        {
            self.direct_lazy = true;
        }
        let _ = config.set_max_write(1024 * 1024);
        let _ = config.set_max_readahead(512 * 1024);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else { return reply.error(Errno::ENOENT) };
        let found = store::child(&self.engine.store.lock(), parent.0, name);
        match found {
            Ok(Some(it)) if visible(&it) => reply.entry(&TTL, &self.attr(&it), Generation(0)),
            Ok(_) => match self.engine.link_child(parent.0, name) {
                Some(l) => reply.entry(&TTL, &link_attr(self.uid, self.gid, &l), Generation(0)),
                None => reply.error(Errno::ENOENT),
            },
            Err(e) => reply.error(store_errno(&e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.engine.link_by_ino(ino.0) {
            Some(l) => {
                use std::os::unix::ffi::OsStrExt;
                reply.data(l.target.as_os_str().as_bytes())
            }
            None => reply.error(Errno::EINVAL),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        if ino.0 >= LINK_INO_BASE {
            return match self.engine.link_by_ino(ino.0) {
                Some(l) => reply.attr(&TTL, &link_attr(self.uid, self.gid, &l)),
                None => reply.error(Errno::ENOENT),
            };
        }
        match self.live(ino.0) {
            Ok(it) => reply.attr(&TTL, &self.attr(&it)),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let it = match self.live(ino.0) {
            Ok(i) => i,
            Err(e) => return reply.error(e),
        };
        if uid.is_some_and(|u| u != self.uid) || gid.is_some_and(|g| g != self.gid) {
            return reply.error(Errno::EPERM);
        }
        if let Some(m) = mode {
            let _ = store::set_mode(&self.engine.store.lock(), it.ino, m & 0o7777);
        }
        if let Some(t) = mtime {
            let _ = store::set_mtime(&self.engine.store.lock(), it.ino, to_ns(t));
        }
        let Some(new_size) = size else {
            return match self.live(ino.0) {
                Ok(it) => reply.attr(&TTL, &self.attr(&it)),
                Err(e) => reply.error(e),
            };
        };
        if it.is_dir {
            return reply.error(Errno::EISDIR);
        }
        let handle = fh.and_then(|f| self.handle(f));
        let engine = self.engine.clone();
        let truncate = move |fs_engine: &Arc<Engine>| -> Result<(), Errno> {
            let file = match handle.as_ref().and_then(|h| h.file.lock().unwrap().clone()) {
                Some(f) => f,
                None => Arc::new(
                    fs_engine.content.open_rw(it.ino).map_err(|e| errno(e.raw_os_error().unwrap_or(libc::EIO)))?,
                ),
            };
            file.set_len(new_size).map_err(|e| errno(e.raw_os_error().unwrap_or(libc::EIO)))?;
            let c = fs_engine.store.lock();
            store::mark_dirty(&c, it.ino).map_err(|e| store_errno(&e))?;
            store::set_size_mtime(&c, it.ino, new_size, now_ns()).map_err(|e| store_errno(&e))?;
            drop(c);
            fs_engine.note_write(it.ino, None, Some(new_size));
            fs_engine.content.tag(it.ino, &it.name, true);
            fs_engine.local_changed();
            Ok(())
        };
        if it.hydrated {
            if let Err(e) = self.detach_if_uploading(it.ino) {
                return reply.error(errno(e.raw_os_error().unwrap_or(libc::EIO)));
            }
            let r = truncate(&engine);
            return match r.and_then(|_| self.live(it.ino)) {
                Ok(it) => reply.attr(&TTL, &self.attr(&it)),
                Err(e) => reply.error(e),
            };
        }
        if new_size == 0 {
            // Truncating an online-only file: no need to download it first.
            if let Err(e) = self.engine.content.create_empty(it.ino) {
                return reply.error(errno(e.raw_os_error().unwrap_or(libc::EIO)));
            }
            let r = truncate(&engine);
            return match r.and_then(|_| self.live(it.ino)) {
                Ok(it) => reply.attr(&TTL, &self.attr(&it)),
                Err(e) => reply.error(e),
            };
        }
        let uid = self.uid;
        let gid = self.gid;
        self.rt.spawn(async move {
            if let Err(e) = engine.hydrate(it.ino).await {
                return reply.error(errno(e.errno()));
            }
            match truncate(&engine) {
                Ok(()) => match store::get_live(&engine.store.lock(), it.ino) {
                    Ok(Some(now)) => reply.attr(&TTL, &file_attr(&engine, uid, gid, &now)),
                    _ => reply.error(Errno::ENOENT),
                },
                Err(e) => reply.error(e),
            }
        });
    }

    fn mknod(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        if mode & libc::S_IFMT != libc::S_IFREG && mode & libc::S_IFMT != 0 {
            return reply.error(Errno::EPERM);
        }
        let name = match self.new_name(name) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        match self.create_item(parent.0, name, false, mode & !umask) {
            Ok(it) => reply.entry(&TTL, &self.attr(&it), Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, mode: u32, umask: u32, reply: ReplyEntry) {
        let name = match self.new_name(name) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        match self.create_item(parent.0, name, true, mode & !umask) {
            Ok(it) => reply.entry(&TTL, &self.attr(&it), Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else { return reply.error(Errno::ENOENT) };
        let real = matches!(store::child(&self.engine.store.lock(), parent.0, name), Ok(Some(ref it)) if visible(it));
        if !real && let Some(l) = self.engine.link_child(parent.0, name) {
            // Removing the link removes the shortcut; the shared folder stays.
            return match self.rt.block_on(self.engine.remove_shortcut(&l.shortcut.id)) {
                Ok(()) => reply.ok(),
                Err(e) => {
                    tracing::warn!("removing the shortcut {name} failed: {e}");
                    reply.error(Errno::EIO)
                }
            };
        }
        let r = (|| -> Result<(), Errno> {
            let c = self.engine.store.lock();
            let it = store::child(&c, parent.0, name).map_err(|e| store_errno(&e))?.ok_or(Errno::ENOENT)?;
            if it.is_dir {
                return Err(Errno::EISDIR);
            }
            if it.item_id.is_some() && it.local_only != LocalOnly::Ignored {
                store::tombstone(&c, it.ino, now_ms()).map_err(|e| store_errno(&e))?;
                self.engine.note_local_delete();
            } else {
                store::remove_row(&c, it.ino).map_err(|e| store_errno(&e))?;
            }
            drop(c);
            let _ = self.engine.content.remove(it.ino);
            Ok(())
        })();
        match r {
            Ok(()) => {
                self.engine.local_changed();
                reply.ok()
            }
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else { return reply.error(Errno::ENOENT) };
        let r = (|| -> Result<(), Errno> {
            let c = self.engine.store.lock();
            let Some(it) = store::child(&c, parent.0, name).map_err(|e| store_errno(&e))? else {
                return Err(if self.engine.link_child(parent.0, name).is_some() {
                    Errno::ENOTDIR
                } else {
                    Errno::ENOENT
                });
            };
            if !it.is_dir {
                return Err(Errno::ENOTDIR);
            }
            if store::count_children(&c, it.ino).map_err(|e| store_errno(&e))? > 0 {
                return Err(Errno::ENOTEMPTY);
            }
            if it.item_id.is_some() && it.local_only != LocalOnly::Ignored {
                store::tombstone(&c, it.ino, now_ms()).map_err(|e| store_errno(&e))?;
                self.engine.note_local_delete();
            } else {
                // Drop tombstoned children too (they belong to a never-synced folder).
                for d in store::descendants(&c, it.ino).map_err(|e| store_errno(&e))? {
                    store::remove_row(&c, d).map_err(|e| store_errno(&e))?;
                }
                store::remove_row(&c, it.ino).map_err(|e| store_errno(&e))?;
            }
            Ok(())
        })();
        match r {
            Ok(()) => {
                self.engine.local_changed();
                reply.ok()
            }
            Err(e) => reply.error(e),
        }
    }

    fn symlink(&self, _req: &Request, _parent: INodeNo, _link_name: &OsStr, _target: &Path, reply: ReplyEntry) {
        // OneDrive has no symbolic links.
        reply.error(Errno::EPERM);
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (Some(name), Ok(newname)) = (name.to_str(), self.new_name(newname)) else {
            return reply.error(Errno::EINVAL);
        };
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            return reply.error(Errno::EINVAL);
        }
        let noreplace = flags.contains(RenameFlags::RENAME_NOREPLACE);
        let real = matches!(store::child(&self.engine.store.lock(), parent.0, name), Ok(Some(ref it)) if visible(it));
        if !real && let Some(l) = self.engine.link_child(parent.0, name) {
            // A shortcut can be renamed in place; moving it would need the web.
            if newparent.0 != parent.0 {
                return reply.error(Errno::EXDEV);
            }
            let clash = matches!(store::child_folded(&self.engine.store.lock(), parent.0, newname), Ok(Some(_)))
                || self.engine.link_child(parent.0, newname).is_some_and(|o| o.ino != l.ino);
            if clash {
                return reply.error(Errno::EEXIST);
            }
            return match self.rt.block_on(self.engine.rename_shortcut(&l.shortcut.id, newname)) {
                Ok(()) => reply.ok(),
                Err(e) => {
                    tracing::warn!("renaming the shortcut {name} failed: {e}");
                    reply.error(Errno::EIO)
                }
            };
        }
        if self.engine.link_child(newparent.0, newname).is_some() {
            return reply.error(Errno::EEXIST);
        }
        let r = self.do_rename(parent.0, name, newparent.0, newname, noreplace);
        match r {
            Ok(()) => {
                self.engine.local_changed();
                reply.ok()
            }
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let it = match self.live(ino.0) {
            Ok(i) => i,
            Err(e) => return reply.error(e),
        };
        if it.is_dir {
            return reply.error(Errno::EISDIR);
        }
        let write = flags.0 & libc::O_ACCMODE != libc::O_RDONLY;
        let trunc = flags.0 & libc::O_TRUNC != 0;
        let _ = store::touch_access(&self.engine.store.lock(), it.ino, chrono::Utc::now().timestamp());

        if it.hydrated && self.engine.content.exists(it.ino) {
            if it.is_stale() {
                // Serve the cached copy now; the newer version downloads in the background.
                self.engine.hydrate_wake.notify_one();
            }
            if write && let Err(e) = self.detach_if_uploading(it.ino) {
                return reply.error(errno(e.raw_os_error().unwrap_or(libc::EIO)));
            }
            let file = match self.open_content(it.ino, write) {
                Ok(f) => f,
                Err(e) => return reply.error(errno(e.raw_os_error().unwrap_or(libc::EIO))),
            };
            let h = Handle {
                ino: it.ino,
                write,
                background: false,
                file: Mutex::new(Some(Arc::new(file))),
                prefix: Mutex::new(None),
                wrote: AtomicBool::new(false),
            };
            if write && trunc {
                if let Some(f) = h.file.lock().unwrap().as_ref()
                    && let Err(e) = f.set_len(0)
                {
                    return reply.error(errno(e.raw_os_error().unwrap_or(libc::EIO)));
                }
                self.mark_written(&h);
                self.engine.note_write(it.ino, None, Some(0));
                let _ = store::set_size_mtime(&self.engine.store.lock(), it.ino, 0, now_ns());
            }
            let flags = if write { FopenFlags::empty() } else { FopenFlags::FOPEN_KEEP_CACHE };
            let fh = self.add_handle(h);
            return reply.opened(FileHandle(fh), flags);
        }

        // ---- online-only
        if write && trunc {
            if let Err(e) = self.engine.content.create_empty(it.ino) {
                return reply.error(errno(e.raw_os_error().unwrap_or(libc::EIO)));
            }
            let file = match self.engine.content.open_rw(it.ino) {
                Ok(f) => f,
                Err(e) => return reply.error(errno(e.raw_os_error().unwrap_or(libc::EIO))),
            };
            let h = Handle {
                ino: it.ino,
                write,
                background: false,
                file: Mutex::new(Some(Arc::new(file))),
                prefix: Mutex::new(None),
                wrote: AtomicBool::new(false),
            };
            self.mark_written(&h);
            self.engine.note_write(it.ino, None, Some(0));
            let _ = store::set_size_mtime(&self.engine.store.lock(), it.ino, 0, now_ns());
            let fh = self.add_handle(h);
            return reply.opened(FileHandle(fh), FopenFlags::empty());
        }
        let caller = self.guard.classify(req.pid());
        if let Caller::Background(name) = &caller
            && self.engine.cfg().block_background_hydration
        {
            tracing::debug!(ino = it.ino, %name, "refusing to download an online-only file for a background process");
            return reply.error(Errno::EIO);
        }
        if !write {
            // Lazy: nothing is downloaded until the first read.
            let h = Handle {
                ino: it.ino,
                write: false,
                background: matches!(caller, Caller::Background(_)),
                file: Mutex::new(None),
                prefix: Mutex::new(None),
                wrote: AtomicBool::new(false),
            };
            let fh = self.add_handle(h);
            let flags = if self.direct_lazy { FopenFlags::FOPEN_DIRECT_IO } else { FopenFlags::empty() };
            return reply.opened(FileHandle(fh), flags);
        }
        // Opening for writing (without truncation) needs the content first.
        let engine = self.engine.clone();
        let ino = it.ino;
        let fh = self.new_fh();
        let slot = Arc::new(Handle {
            ino,
            write: true,
            background: false,
            file: Mutex::new(None),
            prefix: Mutex::new(None),
            wrote: AtomicBool::new(false),
        });
        self.handles.lock().unwrap().insert(fh, slot.clone());
        self.engine.note_open(ino, true);
        let handles = self.handles.clone();
        self.rt.spawn(async move {
            let failed = |e: i32| {
                handles.lock().unwrap().remove(&fh);
                engine.note_close(ino, true);
                errno(e)
            };
            match engine.hydrate(ino).await {
                Ok(()) => match engine.content.open_rw(ino) {
                    Ok(f) => {
                        *slot.file.lock().unwrap() = Some(Arc::new(f));
                        reply.opened(FileHandle(fh), FopenFlags::empty());
                    }
                    Err(e) => reply.error(failed(e.raw_os_error().unwrap_or(libc::EIO))),
                },
                Err(e) => reply.error(failed(e.errno())),
            }
        });
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some(h) = self.handle(fh) else { return reply.error(Errno::EBADF) };
        let file = h.file.lock().unwrap().clone();
        if let Some(f) = file {
            return read_at(&f, offset, size, reply);
        }
        // Lazy handle on an online-only file.
        let engine = self.engine.clone();
        let total = store::get(&engine.store.lock(), h.ino).ok().flatten().map(|i| i.size).unwrap_or(0);
        if offset >= total {
            return reply.data(&[]);
        }
        let end = (offset + size as u64).min(total);
        let ino = h.ino;
        if let Some(p) = h.prefix.lock().unwrap().clone()
            && end <= p.len() as u64
        {
            return reply.data(&p[offset as usize..end as usize]);
        }
        let use_prefix = total > SMALL_FILE && end <= PREFIX_WINDOW;
        let background = h.background;
        self.rt.spawn(async move {
            if use_prefix || background {
                if background && !use_prefix {
                    return reply.error(Errno::EIO);
                }
                match engine.read_prefix(ino, PREFIX_WINDOW.min(total)).await {
                    Ok(bytes) => {
                        let bytes = Arc::new(bytes);
                        *h.prefix.lock().unwrap() = Some(bytes.clone());
                        let e = (end as usize).min(bytes.len());
                        let s = (offset as usize).min(e);
                        reply.data(&bytes[s..e]);
                    }
                    Err(e) => reply.error(errno(e.errno())),
                }
                return;
            }
            // Large files stream: only the blocks this read needs are awaited.
            let streamed = if total >= crate::stream::STREAM_MIN {
                match engine.stream_for(ino) {
                    Ok(Some(stream)) => Some(engine.stream_read(&stream, offset, size).await),
                    Ok(None) => None,
                    Err(e) => Some(Err(e)),
                }
            } else {
                None
            };
            let result = match streamed {
                Some(Ok(data)) => return reply.data(&data),
                Some(Err(e)) => Err(e),
                None => engine.hydrate(ino).await,
            };
            match result {
                Ok(()) => match engine.content.open_read(ino) {
                    Ok(f) => {
                        let f = Arc::new(f);
                        *h.file.lock().unwrap() = Some(f.clone());
                        read_at(&f, offset, size, reply);
                    }
                    Err(e) => reply.error(errno(e.raw_os_error().unwrap_or(libc::EIO))),
                },
                Err(e) => {
                    if e.errno() == libc::ENETUNREACH {
                        let name =
                            store::get(&engine.store.lock(), ino).ok().flatten().map(|i| i.name).unwrap_or_default();
                        engine.notice(
                            NoticeKind::OfflineOpen,
                            "Can't open this file while you're offline",
                            &format!("\"{name}\" is available online only. Connect to the internet to open it."),
                            Some(engine.local_path(ino)),
                        );
                    }
                    reply.error(errno(e.errno()))
                }
            }
        });
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let Some(h) = self.handle(fh) else { return reply.error(Errno::EBADF) };
        let Some(file) = h.file.lock().unwrap().clone() else { return reply.error(Errno::EBADF) };
        let end = offset + data.len() as u64;
        let cur = self.engine.size_override(h.ino).unwrap_or(0);
        if end > cur && !self.free_space_ok() {
            return reply.error(Errno::ENOSPC);
        }
        if let Err(e) = file.write_all_at(data, offset) {
            return reply.error(errno(e.raw_os_error().unwrap_or(libc::EIO)));
        }
        self.mark_written(&h);
        let len = file.metadata().map(|m| m.len()).unwrap_or(end);
        self.engine.note_write(h.ino, Some(end), None);
        self.engine.set_open_size(h.ino, len);
        reply.written(data.len() as u32);
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let h = self.handles.lock().unwrap().remove(&fh.0);
        if let Some(h) = h {
            if h.wrote.load(Ordering::SeqCst) {
                if let Some(f) = h.file.lock().unwrap().as_ref() {
                    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                    let c = self.engine.store.lock();
                    let mtime = store::get(&c, h.ino).ok().flatten().map(|i| i.mtime_ns).unwrap_or(0);
                    let _ = store::set_size_mtime(&c, h.ino, len, mtime.max(now_ns() - 1).max(now_ns()));
                }
                self.engine.note_write(h.ino, None, None);
            }
            self.engine.note_close(h.ino, h.write);
            if h.wrote.load(Ordering::SeqCst) {
                self.engine.local_changed();
            }
        }
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, datasync: bool, reply: ReplyEmpty) {
        let Some(h) = self.handle(fh) else { return reply.error(Errno::EBADF) };
        let f = h.file.lock().unwrap().clone();
        match f {
            Some(f) => {
                let r = if datasync { f.sync_data() } else { f.sync_all() };
                match r {
                    Ok(()) => reply.ok(),
                    Err(e) => reply.error(errno(e.raw_os_error().unwrap_or(libc::EIO))),
                }
            }
            None => reply.ok(),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let it = match self.live(ino.0) {
            Ok(i) => i,
            Err(e) => return reply.error(e),
        };
        if !it.is_dir {
            return reply.error(Errno::ENOTDIR);
        }
        let children = match store::children(&self.engine.store.lock(), it.ino) {
            Ok(c) => c,
            Err(e) => return reply.error(store_errno(&e)),
        };
        self.engine.queue_thumbnails(&children);
        let mut entries = vec![
            (it.ino, FileType::Directory, ".".to_string()),
            (it.parent_ino.unwrap_or(ROOT_INO), FileType::Directory, "..".to_string()),
        ];
        for c in children.into_iter().filter(visible) {
            entries.push((c.ino, if c.is_dir { FileType::Directory } else { FileType::RegularFile }, c.name));
        }
        for l in self.engine.links_in(it.ino) {
            entries.push((l.ino, FileType::Symlink, l.name));
        }
        let fh = self.new_fh();
        self.dirs.lock().unwrap().insert(fh, Arc::new(DirSnapshot { entries }));
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn readdir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let Some(snap) = self.dirs.lock().unwrap().get(&fh.0).cloned() else { return reply.error(Errno::EBADF) };
        for (i, (ino, kind, name)) in snap.entries.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(*ino), (i + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectoryPlus) {
        let Some(snap) = self.dirs.lock().unwrap().get(&fh.0).cloned() else { return reply.error(Errno::EBADF) };
        for (i, (ino, _kind, name)) in snap.entries.iter().enumerate().skip(offset as usize) {
            let attr = if *ino >= LINK_INO_BASE {
                let Some(l) = self.engine.link_by_ino(*ino) else { continue };
                link_attr(self.uid, self.gid, &l)
            } else {
                let Ok(Some(it)) = store::get(&self.engine.store.lock(), *ino) else { continue };
                self.attr(&it)
            };
            if reply.add(INodeNo(*ino), (i + 1) as u64, name, &TTL, &attr, Generation(0)) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        self.dirs.lock().unwrap().remove(&fh.0);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        const BS: u64 = 4096;
        let (total, free) = match self.engine.quota() {
            Some(q) if q.total.unwrap_or(0) > 0 => {
                let total = q.total.unwrap_or(0).max(0) as u64;
                let free = q
                    .remaining
                    .map(|r| r.max(0) as u64)
                    .unwrap_or_else(|| total.saturating_sub(q.used.unwrap_or(0).max(0) as u64));
                (total, free)
            }
            _ => {
                let free = self.engine.content.free_space().unwrap_or(0);
                (free.max(1), free)
            }
        };
        reply.statfs(total / BS, free / BS, free / BS, 1_000_000, 1_000_000, BS as u32, 255, BS as u32);
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        if name.to_str() != Some(xattr::PIN) {
            return reply.error(Errno::ENOTSUP);
        }
        let pin = match std::str::from_utf8(value).unwrap_or("").trim() {
            "pinned" | "always" | "1" => PinState::AlwaysKeep,
            "unpinned" | "online" | "free" | "0" => PinState::OnlineOnly,
            "inherit" | "" => PinState::Inherit,
            _ => return reply.error(Errno::EINVAL),
        };
        match self.engine.set_pin_state(ino.0, pin) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(e.errno())),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        if ino.0 >= LINK_INO_BASE {
            return reply.error(Errno::NO_XATTR);
        }
        let it = match self.live(ino.0) {
            Ok(i) => i,
            Err(e) => return reply.error(e),
        };
        let value: Option<String> = match name.to_str() {
            Some(n) if n == xattr::STATUS => Some(self.engine.item_status(&it).as_str().to_string()),
            Some(n) if n == xattr::PIN => Some(
                store::effective_pin(&self.engine.store.lock(), it.ino)
                    .unwrap_or(PinState::Inherit)
                    .as_str()
                    .to_string(),
            ),
            Some(n) if n == xattr::ID => it.item_id.clone(),
            Some(n) if n == xattr::WEB_URL => it.web_url.clone(),
            Some(n) if n == xattr::SHARED => Some(if it.shared { "1" } else { "0" }.to_string()),
            Some(n) if n == xattr::ERROR => it.error_msg.clone(),
            _ => None,
        };
        let Some(v) = value else { return reply.error(Errno::NO_XATTR) };
        reply_xattr(v.as_bytes(), size, reply);
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        if ino.0 >= LINK_INO_BASE {
            return reply_xattr(&[], size, reply);
        }
        if let Err(e) = self.live(ino.0) {
            return reply.error(e);
        }
        let mut buf = Vec::new();
        for n in xattr::ALL {
            buf.extend_from_slice(n.as_bytes());
            buf.push(0);
        }
        reply_xattr(&buf, size, reply);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::ENOTSUP);
    }

    fn access(&self, _req: &Request, ino: INodeNo, _mask: fuser::AccessFlags, reply: ReplyEmpty) {
        if self.engine.link_by_ino(ino.0).is_some() {
            return reply.ok();
        }
        match self.live(ino.0) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let name = match self.new_name(name) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        let it = match self.create_item(parent.0, name, false, mode & !umask) {
            Ok(i) => i,
            Err(e) => return reply.error(e),
        };
        let write = flags & libc::O_ACCMODE != libc::O_RDONLY;
        let file = match self.open_content(it.ino, true) {
            Ok(f) => f,
            Err(e) => return reply.error(errno(e.raw_os_error().unwrap_or(libc::EIO))),
        };
        let h = Handle {
            ino: it.ino,
            write,
            background: false,
            file: Mutex::new(Some(Arc::new(file))),
            prefix: Mutex::new(None),
            // A new file is dirty already (it must be uploaded even if empty).
            wrote: AtomicBool::new(true),
        };
        self.engine.note_write(it.ino, None, Some(0));
        let fh = self.add_handle(h);
        reply.created(&TTL, &self.attr(&it), Generation(0), FileHandle(fh), FopenFlags::empty());
    }

    fn fallocate(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        if mode != 0 {
            return reply.error(Errno::ENOTSUP);
        }
        let Some(h) = self.handle(fh) else { return reply.error(Errno::EBADF) };
        let Some(f) = h.file.lock().unwrap().clone() else { return reply.error(Errno::EBADF) };
        use std::os::fd::AsRawFd;
        // SAFETY: valid fd owned by `f`.
        let rc = unsafe { libc::posix_fallocate(f.as_raw_fd(), offset as i64, length as i64) };
        if rc != 0 {
            return reply.error(errno(rc));
        }
        self.mark_written(&h);
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        self.engine.note_write(h.ino, Some(len), None);
        reply.ok();
    }
}

fn read_at(f: &File, offset: u64, size: u32, reply: ReplyData) {
    let mut buf = vec![0u8; size as usize];
    let mut done = 0usize;
    while done < buf.len() {
        match f.read_at(&mut buf[done..], offset + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return reply.error(errno(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }
    reply.data(&buf[..done]);
}

fn reply_xattr(v: &[u8], size: u32, reply: ReplyXattr) {
    if size == 0 {
        reply.size(v.len() as u32);
    } else if (size as usize) < v.len() {
        reply.error(Errno::ERANGE);
    } else {
        reply.data(v);
    }
}

impl OneDriveFs {
    fn do_rename(&self, parent: u64, name: &str, newparent: u64, newname: &str, noreplace: bool) -> Result<(), Errno> {
        let mut conn = self.engine.store.lock();
        let tx = conn.transaction().map_err(|_| Errno::EIO)?;
        let se = |e: StoreError| store_errno(&e);
        let src = store::child(&tx, parent, name).map_err(se)?.ok_or(Errno::ENOENT)?;
        let dest_parent = store::get_live(&tx, newparent).map_err(se)?.ok_or(Errno::ENOENT)?;
        if !dest_parent.is_dir {
            return Err(Errno::ENOTDIR);
        }
        if src.is_dir {
            // A folder can't move into itself.
            let mut cur = Some(newparent);
            while let Some(c) = cur {
                if c == src.ino {
                    return Err(Errno::EINVAL);
                }
                cur = store::get(&tx, c).map_err(se)?.and_then(|i| i.parent_ino);
            }
        }
        let dst = store::child(&tx, newparent, newname).map_err(se)?;
        if let Some(f) = store::child_folded(&tx, newparent, newname).map_err(se)? {
            // OneDrive is case-insensitive: refuse a clash with a different item.
            if f.ino != src.ino && dst.as_ref().map(|d| d.ino) != Some(f.ino) {
                return Err(Errno::EEXIST);
            }
        }
        let (lo_new, issue) = self.engine.classify_name(&dest_parent, newname);
        let mut removed_content: Vec<u64> = Vec::new();

        if let Some(d) = dst.as_ref().filter(|d| d.ino != src.ino) {
            if noreplace {
                return Err(Errno::EEXIST);
            }
            match (src.is_dir, d.is_dir) {
                (false, true) => return Err(Errno::EISDIR),
                (true, false) => return Err(Errno::ENOTDIR),
                (true, true) if store::count_children(&tx, d.ino).map_err(se)? > 0 => return Err(Errno::ENOTEMPTY),
                _ => {}
            }
            let atomic_save = !src.is_dir
                && src.item_id.is_none()
                && d.item_id.is_some()
                && d.local_only == LocalOnly::No
                && lo_new == LocalOnly::No;
            if atomic_save {
                // The new content becomes a new version of the replaced item.
                store::remove_row(&tx, d.ino).map_err(se)?;
                tx.execute(
                    "UPDATE items SET item_id = ?2, etag = ?3, ctag = ?4, remote_hash = ?5, remote_size = ?6,
                            remote_mtime_ns = ?7, remote_parent_id = ?8, remote_name = ?9, web_url = ?10,
                            shared = ?11, pin = ?12, dirty = 1 WHERE ino = ?1",
                    rusqlite::params![
                        src.ino as i64,
                        d.item_id,
                        d.etag,
                        d.ctag,
                        d.remote_hash,
                        d.remote_size.map(|v| v as i64),
                        d.remote_mtime_ns,
                        d.remote_parent_id,
                        d.remote_name,
                        d.web_url,
                        d.shared as i64,
                        store::pin_to_i64(d.pin),
                    ],
                )
                .map_err(|_| Errno::EIO)?;
                store::log_decision(
                    &tx,
                    "replace-content",
                    Some(src.ino),
                    d.item_id.as_deref(),
                    newname,
                    "saved over an existing file (atomic save)",
                )
                .map_err(se)?;
            } else if d.item_id.is_some() && d.local_only != LocalOnly::Ignored {
                store::tombstone(&tx, d.ino, now_ms()).map_err(se)?;
            } else {
                store::remove_row(&tx, d.ino).map_err(se)?;
            }
            removed_content.push(d.ino);
        }

        let old_parent = src.parent_ino.unwrap_or(ROOT_INO);
        let old_name = src.name.clone();
        store::set_name_parent(&tx, src.ino, newparent, newname).map_err(se)?;

        // Moving into or out of local-only areas (trash, ignored names).
        let effective_new = if lo_new == LocalOnly::Invalid { LocalOnly::Invalid } else { lo_new };
        if effective_new != src.local_only {
            let subtree = store::descendants(&tx, src.ino).map_err(se)?;
            if src.item_id.is_some() && effective_new == LocalOnly::Ignored {
                // Leaving the synced area (e.g. "Move to Trash"): delete it online
                // through a tombstone that keeps the server identity.
                let remote = store::RemoteState {
                    item_id: src.item_id.clone().unwrap_or_default(),
                    etag: src.etag.clone(),
                    ctag: src.ctag.clone(),
                    hash: src.remote_hash.clone(),
                    size: src.remote_size,
                    mtime_ns: src.remote_mtime_ns,
                    parent_id: src.remote_parent_id.clone(),
                    name: src.remote_name.clone(),
                    web_url: src.web_url.clone(),
                    shared: src.shared,
                };
                // Keep each file's server id as its "origin" so content that was
                // never downloaded can still be fetched before the delete goes out.
                for d in std::iter::once(src.ino).chain(subtree.iter().copied()) {
                    store::detach_to_origin(&tx, d).map_err(se)?;
                }
                let tomb = store::insert(
                    &tx,
                    &NewItem {
                        parent_ino: old_parent,
                        name: old_name.clone(),
                        is_dir: src.is_dir,
                        ..Default::default()
                    },
                )
                .map_err(se)?;
                store::tombstone(&tx, tomb, now_ms()).map_err(se)?;
                self.engine.note_local_delete();
                store::set_remote(&tx, tomb, &remote).map_err(se)?;
            }
            let child_lo = match effective_new {
                LocalOnly::No => LocalOnly::No,
                _ => LocalOnly::Ignored,
            };
            store::set_local_only(&tx, src.ino, effective_new).map_err(se)?;
            for d in &subtree {
                store::set_local_only(&tx, *d, child_lo).map_err(se)?;
            }
            if effective_new == LocalOnly::No {
                store::clear_error(&tx, src.ino).map_err(se)?;
            }
        }
        if let Some(issue) = issue {
            store::set_error(&tx, src.ino, issue.code(), &issue.to_string(), now_ms(), true).map_err(se)?;
        } else if src.local_only == LocalOnly::Invalid && effective_new == LocalOnly::No {
            store::clear_error(&tx, src.ino).map_err(se)?;
        }
        tx.commit().map_err(|_| Errno::EIO)?;
        drop(conn);
        for ino in removed_content {
            let _ = self.engine.content.remove(ino);
        }
        if !src.is_dir {
            self.engine.content.tag(src.ino, newname, src.dirty || src.item_id.is_none());
        }
        Ok(())
    }
}

/// Mount options used for every account.
pub fn mount_config() -> fuser::Config {
    use fuser::MountOption;
    let mut cfg = fuser::Config::default();
    cfg.mount_options = vec![
        MountOption::FSName("linux-onedrive".into()),
        MountOption::Subtype("onedrive".into()),
        MountOption::DefaultPermissions,
        MountOption::NoSuid,
        MountOption::NoDev,
        MountOption::RW,
    ];
    cfg.n_threads = Some(8);
    cfg
}

/// Mount the filesystem for an engine (in a background thread).
pub fn mount(engine: Arc<Engine>, rt: tokio::runtime::Handle) -> std::io::Result<fuser::BackgroundSession> {
    let mp = engine.mount_point();
    let fs = OneDriveFs::new(engine.clone(), rt);
    let session = fuser::Session::new(fs, &mp, &mount_config())?;
    engine.set_notifier(Some(session.notifier()));
    session.spawn()
}
