//! Per-account engine: owns the store, the content cache and the Graph
//! client, runs the sync loops, and is shared with the FUSE filesystem.

use crate::content::ContentStore;
use crate::store::{self, Item, LocalOnly, ROOT_INO, Store, StoreError};
use od_core::dbus::{ActivityEntry, Progress};
use od_core::names::{self, IgnoreSet, NameIssue};
use od_core::types::{ActivityKind, DriveType, ItemStatus, PinState, SyncState};
use od_graph::GraphClient;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, broadcast, watch};
use tokio_util::sync::CancellationToken;

/// Static configuration of one account's engine.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub account_id: String,
    pub account_dir: PathBuf,
    pub mount_point: PathBuf,
    pub drive_id: String,
    pub drive_type: DriveType,
    pub ignore: IgnoreSet,
    /// Remote top-level folder names hidden from this device ("Choose folders").
    pub excluded_folders: Vec<String>,
    pub mass_delete_threshold: u32,
    pub poll_interval: Duration,
    pub max_parallel_uploads: usize,
    pub max_parallel_downloads: usize,
    pub block_background_hydration: bool,
    pub download_all: bool,
    pub auto_free_up_days: u32,
    pub hostname: String,
    /// A file must be closed and unmodified this long before it is uploaded.
    pub upload_quiet: Duration,
    /// Free space always left on the disk holding the cache (downloads fail below it).
    pub disk_reserve: u64,
    /// Below this much free space, writes that grow files fail with ENOSPC.
    pub critical_free: u64,
    /// Run a full remote listing (with deletion sweep) this often as a safety net.
    pub full_reconcile_every: Duration,
    /// Write OneDrive's previews of online-only media into the thumbnail cache.
    pub server_thumbnails: bool,
    /// Use the real-time change notification channel.
    pub push_notifications: bool,
    /// The freedesktop thumbnail cache (`~/.cache/thumbnails`).
    pub thumbnail_root: PathBuf,
    /// Always detect remote changes by listing every folder instead of
    /// delta. Normally left off: the engine switches to listing by itself
    /// when the server refuses delta on a shared folder.
    pub walk_listing: bool,
    /// Listing is expensive (one request per folder): how long to wait
    /// between listings.
    pub listing_interval: Duration,
    /// Track the drive's storage quota (the user's own drive only).
    pub track_quota: bool,
    pub uid: u32,
    pub gid: u32,
}

impl EngineConfig {
    pub fn new(
        account_id: &str,
        account_dir: &Path,
        mount_point: &Path,
        drive_id: &str,
        drive_type: DriveType,
    ) -> Self {
        // SAFETY: getuid/getgid cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Self {
            account_id: account_id.into(),
            account_dir: account_dir.into(),
            mount_point: mount_point.into(),
            drive_id: drive_id.into(),
            drive_type,
            ignore: IgnoreSet::default(),
            excluded_folders: Vec::new(),
            mass_delete_threshold: 200,
            poll_interval: Duration::from_secs(60),
            max_parallel_uploads: 4,
            max_parallel_downloads: 4,
            block_background_hydration: true,
            download_all: false,
            auto_free_up_days: 0,
            hostname: od_core::hostname(),
            upload_quiet: Duration::from_secs(3),
            disk_reserve: 1024 * 1024 * 1024,
            critical_free: 512 * 1024 * 1024,
            full_reconcile_every: Duration::from_secs(24 * 3600),
            server_thumbnails: true,
            push_notifications: true,
            thumbnail_root: crate::thumbs::cache_root(),
            walk_listing: false,
            listing_interval: Duration::from_secs(30 * 60),
            track_quota: true,
            uid,
            gid,
        }
    }
}

/// Something the daemon/GUI should know about.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    StateChanged,
    Activity(ActivityEntry),
    /// Local paths whose status emblem may have changed.
    ItemsChanged(Vec<PathBuf>),
    ProgressChanged,
    MassDeleteHeld(u32),
    /// A user-facing notice (the daemon decides whether to show a notification).
    Notice {
        kind: NoticeKind,
        title: String,
        body: String,
        path: Option<PathBuf>,
    },
    AuthRequired,
    QuotaChanged,
    /// The engine stopped and needs attention (identity mismatch, dead mount, …).
    Fatal(String),
    /// The user deleted synced items on this device (once per session; used
    /// for the "Deleted files are removed everywhere" reminder).
    LocalDelete,
    /// Shortcuts to shared folders were added, removed or renamed.
    ShortcutsChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Conflict,
    OfflineOpen,
    DiskFull,
    QuotaFull,
    SyncError,
    RemoteMassDelete,
    Recovered,
}

/// Pause reasons (bitmask).
pub mod pause {
    pub const USER: u32 = 1;
    pub const METERED: u32 = 2;
    pub const POWER_SAVER: u32 = 4;
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Graph(#[from] od_graph::GraphError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not enough free disk space")]
    NoSpace,
    #[error("you're offline")]
    Offline,
    #[error("blocked: {0}")]
    Blocked(&'static str),
    /// An error relayed from a shared operation (e.g. a download several
    /// opens were waiting for), keeping the errno for the application.
    #[error("{msg}")]
    Shared { errno: i32, msg: String },
    #[error("{0}")]
    Other(String),
}

impl EngineError {
    /// errno to report to the application that triggered the operation.
    pub fn errno(&self) -> i32 {
        match self {
            EngineError::NoSpace => libc::ENOSPC,
            EngineError::Offline => libc::ENETUNREACH,
            EngineError::Blocked(_) => libc::EACCES,
            EngineError::Store(StoreError::NotFound) => libc::ENOENT,
            EngineError::Store(StoreError::Exists) => libc::EEXIST,
            EngineError::Graph(g) if g.is_offline() => libc::ENETUNREACH,
            EngineError::Graph(g) if g.is_not_found() => libc::ENOENT,
            EngineError::Graph(g) if g.is_access_denied() => libc::EACCES,
            EngineError::Io(e) => e.raw_os_error().unwrap_or(libc::EIO),
            EngineError::Shared { errno, .. } => *errno,
            _ => libc::EIO,
        }
    }
}

pub type EngineResult<T> = Result<T, EngineError>;

impl From<rusqlite::Error> for EngineError {
    fn from(e: rusqlite::Error) -> Self {
        EngineError::Store(StoreError::Sql(e))
    }
}

/// In-memory state of a file with open handles (or recent writes).
#[derive(Debug, Default)]
struct OpenFile {
    readers: u32,
    writers: u32,
    /// Incremented on every write/truncate; an upload only marks the item
    /// clean if the generation it snapshotted is still current.
    write_gen: u64,
    last_write: Option<Instant>,
    /// Size while being written (authoritative over the DB until close).
    size: Option<u64>,
}

/// Kernel cache invalidations, sent after the DB lock is released.
#[derive(Debug, Clone)]
pub enum Inval {
    Entry { parent: u64, name: String },
    Inode { ino: u64 },
    Delete { parent: u64, child: u64, name: String },
}

#[derive(Debug, Default)]
struct Transfer {
    name: String,
    done: u64,
    total: u64,
    upload: bool,
}

pub struct Engine {
    cfg: RwLock<EngineConfig>,
    pub store: Store,
    pub content: ContentStore,
    pub graph: GraphClient,
    events: broadcast::Sender<EngineEvent>,
    pub(crate) push_wake: Notify,
    pub(crate) pull_wake: Notify,
    pub(crate) hydrate_wake: Notify,
    paused: AtomicU32,
    offline: AtomicBool,
    auth_required: AtomicBool,
    fatal: Mutex<Option<String>>,
    pulling: AtomicBool,
    open: Mutex<HashMap<u64, OpenFile>>,
    pub(crate) uploading: Mutex<HashSet<u64>>,
    pub(crate) hydrations: Mutex<HashMap<u64, watch::Receiver<Option<Result<(), (i32, String)>>>>>,
    pub(crate) streams: Mutex<HashMap<u64, Arc<crate::stream::Stream>>>,
    pub(crate) thumbs: crate::thumbs::ThumbQueue,
    pub(crate) push_connected: AtomicBool,
    transfers: Mutex<HashMap<u64, Transfer>>,
    notifier: Mutex<Option<fuser::Notifier>>,
    pub(crate) held_deletes: AtomicU32,
    delete_noted: AtomicBool,
    pub(crate) mass_delete_approved: AtomicBool,
    quota: Mutex<Option<od_graph::model::Quota>>,
    pub(crate) cancel: CancellationToken,
    last_activity_ping: AtomicU64,
    /// Started at construction; lets the delta loop poll faster after local activity.
    started: Instant,
    /// "Sync now": the next listing round runs even if one ran recently.
    pub(crate) refresh_requested: AtomicBool,
    pub(crate) links: Mutex<crate::shortcuts::Links>,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn now_ns() -> i64 {
    chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
}

impl Engine {
    /// Open the account's state (recovering from a damaged database if
    /// needed) and run the startup consistency check.
    pub fn new(cfg: EngineConfig, graph: GraphClient) -> EngineResult<Arc<Engine>> {
        od_core::paths::ensure_private_dir(&cfg.account_dir)?;
        let content = ContentStore::open(&cfg.account_dir)?;
        let (store, recovered_from) = open_store_with_recovery(&cfg.account_dir)?;
        let (events, _) = broadcast::channel(1024);
        let engine = Arc::new(Engine {
            cfg: RwLock::new(cfg),
            store,
            content,
            graph,
            events,
            push_wake: Notify::new(),
            pull_wake: Notify::new(),
            hydrate_wake: Notify::new(),
            paused: AtomicU32::new(0),
            offline: AtomicBool::new(false),
            auth_required: AtomicBool::new(false),
            fatal: Mutex::new(None),
            pulling: AtomicBool::new(false),
            open: Mutex::new(HashMap::new()),
            uploading: Mutex::new(HashSet::new()),
            hydrations: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
            thumbs: crate::thumbs::ThumbQueue::default(),
            push_connected: AtomicBool::new(false),
            transfers: Mutex::new(HashMap::new()),
            notifier: Mutex::new(None),
            held_deletes: AtomicU32::new(0),
            delete_noted: AtomicBool::new(false),
            mass_delete_approved: AtomicBool::new(false),
            quota: Mutex::new(None),
            cancel: CancellationToken::new(),
            last_activity_ping: AtomicU64::new(0),
            started: Instant::now(),
            refresh_requested: AtomicBool::new(false),
            links: Mutex::new(Default::default()),
        });
        engine.check_placement()?;
        engine.fsck()?;
        engine.ensure_special_files()?;
        if let Some(from) = recovered_from {
            engine.notice(
                NoticeKind::Recovered,
                "Sync database was repaired",
                &format!("The damaged database was kept as {}. Your files are being re-checked.", from.display()),
                None,
            );
        }
        Ok(engine)
    }

    pub fn cfg(&self) -> EngineConfig {
        self.cfg.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn update_cfg(&self, f: impl FnOnce(&mut EngineConfig)) {
        let mut c = self.cfg.write().unwrap_or_else(|e| e.into_inner());
        f(&mut c);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    pub(crate) fn emit(&self, ev: EngineEvent) {
        let _ = self.events.send(ev);
    }

    pub fn mount_point(&self) -> PathBuf {
        self.cfg.read().unwrap_or_else(|e| e.into_inner()).mount_point.clone()
    }

    // ------------------------------------------------------------ state

    pub fn set_paused(&self, reason: u32, on: bool) {
        if on {
            self.paused.fetch_or(reason, Ordering::SeqCst);
        } else {
            self.paused.fetch_and(!reason, Ordering::SeqCst);
            self.push_wake.notify_one();
            self.pull_wake.notify_one();
        }
        self.emit(EngineEvent::StateChanged);
    }

    pub fn pause_reasons(&self) -> u32 {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn is_paused(&self) -> bool {
        self.pause_reasons() != 0
    }

    pub fn is_offline(&self) -> bool {
        self.offline.load(Ordering::SeqCst)
    }

    pub(crate) fn set_offline(&self, offline: bool) {
        if self.offline.swap(offline, Ordering::SeqCst) != offline {
            if !offline {
                // Back online: retry everything that failed meanwhile.
                let _ = store::reset_backoff(&self.store.lock());
                self.push_wake.notify_one();
                self.hydrate_wake.notify_one();
            }
            self.emit(EngineEvent::StateChanged);
        }
    }

    pub(crate) fn set_auth_required(&self, v: bool) {
        if self.auth_required.swap(v, Ordering::SeqCst) != v {
            if v {
                self.emit(EngineEvent::AuthRequired);
            }
            self.emit(EngineEvent::StateChanged);
        }
    }

    /// Real-time notifications are connected (polling can be slower).
    pub fn push_connected(&self) -> bool {
        self.push_connected.load(Ordering::SeqCst)
    }

    pub fn is_auth_required(&self) -> bool {
        self.auth_required.load(Ordering::SeqCst)
    }

    pub(crate) fn set_fatal(&self, msg: String) {
        tracing::error!(account = %self.cfg().account_id, "engine stopped: {msg}");
        *self.fatal.lock().unwrap() = Some(msg.clone());
        self.cancel.cancel();
        self.emit(EngineEvent::Fatal(msg));
        self.emit(EngineEvent::StateChanged);
    }

    pub fn fatal_error(&self) -> Option<String> {
        self.fatal.lock().unwrap().clone()
    }

    pub(crate) fn set_pulling(&self, v: bool) {
        self.pulling.store(v, Ordering::SeqCst);
        self.emit(EngineEvent::StateChanged);
    }

    /// Aggregate state for the tray icon and status card.
    pub fn sync_state(&self) -> SyncState {
        if self.fatal.lock().unwrap().is_some() {
            return SyncState::Error;
        }
        if self.is_auth_required() {
            return SyncState::SignInRequired;
        }
        if self.is_paused() {
            return SyncState::Paused;
        }
        if self.is_offline() {
            return SyncState::Offline;
        }
        let p = self.progress();
        if self.pulling.load(Ordering::SeqCst) || p.files_pending() > 0 || !self.transfers.lock().unwrap().is_empty() {
            return SyncState::Syncing;
        }
        SyncState::Idle
    }

    /// One-line status text for the activity panel.
    pub fn status_text(&self) -> String {
        if let Some(f) = self.fatal_error() {
            return f;
        }
        match self.sync_state() {
            SyncState::SignInRequired => "Sign in to keep syncing".into(),
            SyncState::Paused => {
                let r = self.pause_reasons();
                if r & pause::USER != 0 {
                    "Syncing is paused".into()
                } else if r & pause::METERED != 0 {
                    "Paused because you're on a metered network".into()
                } else {
                    "Paused to save power".into()
                }
            }
            SyncState::Offline => "You're offline".into(),
            SyncState::Syncing => {
                let p = self.progress();
                let n = p.files_pending();
                if n == 0 {
                    "Looking for changes".into()
                } else if p.bytes_total > 0 {
                    format!(
                        "Syncing {n} file{} · {}",
                        if n == 1 { "" } else { "s" },
                        od_core::fmt::progress(p.bytes_done, p.bytes_total)
                    )
                } else {
                    format!("Syncing {n} file{}", if n == 1 { "" } else { "s" })
                }
            }
            _ => {
                let held = self.held_deletes.load(Ordering::SeqCst);
                if held > 0 {
                    format!("{held} deletions are waiting for your confirmation")
                } else {
                    "Your files are synced".into()
                }
            }
        }
    }

    pub fn progress(&self) -> Progress {
        let (uploads_pending, _) = store::pending_upload_stats(&self.store.lock()).unwrap_or((0, 0));
        let t = self.transfers.lock().unwrap();
        let downloads = t.values().filter(|x| !x.upload).count() as u32;
        let (done, total) = t.values().fold((0, 0), |(d, tot), x| (d + x.done, tot + x.total));
        let current = t.values().next().map(|x| x.name.clone()).unwrap_or_default();
        Progress { uploads_pending, downloads_pending: downloads, bytes_done: done, bytes_total: total, current }
    }

    pub(crate) fn transfer_begin(&self, ino: u64, name: &str, total: u64, upload: bool) {
        self.transfers.lock().unwrap().insert(ino, Transfer { name: name.into(), done: 0, total, upload });
        self.emit(EngineEvent::ProgressChanged);
    }

    pub(crate) fn transfer_progress(&self, ino: u64, done: u64) {
        if let Some(t) = self.transfers.lock().unwrap().get_mut(&ino) {
            t.done = done;
        }
        // Rate-limit progress events to ~4/s.
        let now = self.started.elapsed().as_millis() as u64;
        if now.saturating_sub(self.last_activity_ping.load(Ordering::Relaxed)) > 250 {
            self.last_activity_ping.store(now, Ordering::Relaxed);
            self.emit(EngineEvent::ProgressChanged);
        }
    }

    pub(crate) fn transfer_end(&self, ino: u64) {
        self.transfers.lock().unwrap().remove(&ino);
        self.emit(EngineEvent::ProgressChanged);
        self.emit(EngineEvent::StateChanged);
    }

    /// Deletes waiting for the user's confirmation (mass-delete hold).
    pub fn held_deletes_count(&self) -> u32 {
        self.held_deletes.load(Ordering::SeqCst)
    }

    pub fn quota(&self) -> Option<od_graph::model::Quota> {
        self.quota.lock().unwrap().clone()
    }

    pub(crate) fn set_quota(&self, q: Option<od_graph::model::Quota>) {
        *self.quota.lock().unwrap() = q;
        self.emit(EngineEvent::QuotaChanged);
    }

    // ------------------------------------------------------------ open files

    pub(crate) fn note_open(&self, ino: u64, write: bool) {
        let mut m = self.open.lock().unwrap();
        let e = m.entry(ino).or_default();
        if write {
            e.writers += 1;
        } else {
            e.readers += 1;
        }
    }

    pub(crate) fn note_close(&self, ino: u64, write: bool) {
        let mut m = self.open.lock().unwrap();
        if let Some(e) = m.get_mut(&ino) {
            if write {
                e.writers = e.writers.saturating_sub(1);
            } else {
                e.readers = e.readers.saturating_sub(1);
            }
            if e.writers == 0 {
                e.size = None;
            }
        }
    }

    /// Record a write; returns the new generation.
    pub(crate) fn note_write(&self, ino: u64, end: Option<u64>, truncate_to: Option<u64>) -> u64 {
        let mut m = self.open.lock().unwrap();
        let e = m.entry(ino).or_default();
        e.write_gen += 1;
        e.last_write = Some(Instant::now());
        if let Some(t) = truncate_to {
            e.size = Some(t);
        }
        if let Some(end) = end {
            e.size = Some(e.size.unwrap_or(0).max(end));
        }
        e.write_gen
    }

    pub(crate) fn set_open_size(&self, ino: u64, size: u64) {
        let mut m = self.open.lock().unwrap();
        m.entry(ino).or_default().size = Some(size);
    }

    pub(crate) fn generation(&self, ino: u64) -> u64 {
        self.open.lock().unwrap().get(&ino).map(|e| e.write_gen).unwrap_or(0)
    }

    pub(crate) fn size_override(&self, ino: u64) -> Option<u64> {
        self.open.lock().unwrap().get(&ino).and_then(|e| e.size)
    }

    /// Open for writing, or written to within the quiet period.
    pub(crate) fn is_busy(&self, ino: u64) -> bool {
        let quiet = self.cfg().upload_quiet;
        let m = self.open.lock().unwrap();
        m.get(&ino)
            .is_some_and(|e: &OpenFile| e.writers > 0 || e.last_write.is_some_and(|t: Instant| t.elapsed() < quiet))
    }

    pub(crate) fn is_open(&self, ino: u64) -> bool {
        self.open.lock().unwrap().get(&ino).is_some_and(|e| e.writers > 0 || e.readers > 0)
    }

    /// Forget bookkeeping of closed, clean files.
    pub(crate) fn gc_open(&self) {
        let mut m = self.open.lock().unwrap();
        m.retain(|_, e: &mut OpenFile| {
            e.writers > 0
                || e.readers > 0
                || e.last_write.is_some_and(|t: Instant| t.elapsed() < Duration::from_secs(600))
        });
    }

    // ------------------------------------------------------------ kernel cache

    pub fn set_notifier(&self, n: Option<fuser::Notifier>) {
        *self.notifier.lock().unwrap() = n;
    }

    /// Send kernel cache invalidations from a separate thread. Never call this
    /// while holding the store lock (the kernel may wait on a FUSE request
    /// that is itself waiting for the lock).
    pub(crate) fn invalidate(&self, invals: Vec<Inval>) {
        if invals.is_empty() {
            return;
        }
        let Some(n) = self.notifier.lock().unwrap().clone() else { return };
        std::thread::spawn(move || {
            for i in invals {
                let r = match &i {
                    Inval::Entry { parent, name } => n.inval_entry(fuser::INodeNo(*parent), name.as_ref()),
                    Inval::Inode { ino } => n.inval_inode(fuser::INodeNo(*ino), 0, 0),
                    Inval::Delete { parent, child, name } => {
                        n.delete(fuser::INodeNo(*parent), fuser::INodeNo(*child), name.as_ref())
                    }
                };
                if let Err(e) = r {
                    // ENOENT just means the kernel had nothing cached.
                    if e.raw_os_error() != Some(libc::ENOENT) {
                        tracing::debug!("invalidation {i:?} failed: {e}");
                    }
                }
            }
        });
    }

    // ------------------------------------------------------------ helpers

    /// Absolute local path of an item.
    pub fn local_path(&self, ino: u64) -> PathBuf {
        let rel = store::rel_path(&self.store.lock(), ino).unwrap_or_default();
        if rel.is_empty() { self.mount_point() } else { self.mount_point().join(rel) }
    }

    /// Classify a name created locally inside `parent`.
    pub fn classify_name(&self, parent: &Item, name: &str) -> (LocalOnly, Option<NameIssue>) {
        if parent.local_only != LocalOnly::No {
            return (LocalOnly::Ignored, None);
        }
        let cfg = self.cfg.read().unwrap_or_else(|e| e.into_inner());
        if cfg.ignore.is_ignored(name) {
            return (LocalOnly::Ignored, None);
        }
        if let Err(issue) = names::validate_name(name, cfg.drive_type) {
            return (LocalOnly::Invalid, Some(issue));
        }
        (LocalOnly::No, None)
    }

    /// Status shown as emblem/xattr for an item.
    pub fn item_status(&self, item: &Item) -> ItemStatus {
        if item.local_only == LocalOnly::Ignored || item.local_only == LocalOnly::Excluded {
            return ItemStatus::Excluded;
        }
        if item.error_code.is_some() || item.local_only == LocalOnly::Invalid {
            return ItemStatus::Error;
        }
        if item.dirty
            || (item.item_id.is_none() && item.ino != ROOT_INO)
            || self.transfers.lock().unwrap().contains_key(&item.ino)
            || self.uploading.lock().unwrap().contains(&item.ino)
        {
            return ItemStatus::Syncing;
        }
        let pin = store::effective_pin(&self.store.lock(), item.ino).unwrap_or(PinState::Inherit);
        if item.is_dir {
            return if pin == PinState::AlwaysKeep { ItemStatus::Pinned } else { ItemStatus::Local };
        }
        if !item.hydrated {
            ItemStatus::OnlineOnly
        } else if pin == PinState::AlwaysKeep {
            ItemStatus::Pinned
        } else {
            ItemStatus::Local
        }
    }

    pub(crate) fn notice(&self, kind: NoticeKind, title: &str, body: &str, path: Option<PathBuf>) {
        self.emit(EngineEvent::Notice { kind, title: title.into(), body: body.into(), path });
    }

    /// Add an entry to the activity feed and broadcast it.
    pub(crate) fn activity(&self, kind: ActivityKind, ino: Option<u64>, rel: &str, size: u64, detail: &str) {
        let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
        let folder_rel = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let folder = if folder_rel.is_empty() {
            "OneDrive".to_string()
        } else {
            folder_rel.rsplit('/').next().unwrap_or(folder_rel).to_string()
        };
        let path = self.mount_point().join(rel);
        let id = store::add_activity(
            &self.store.lock(),
            kind.as_str(),
            ino,
            &name,
            &path.to_string_lossy(),
            &folder,
            size,
            detail,
        )
        .unwrap_or(0);
        self.emit(EngineEvent::Activity(ActivityEntry {
            id,
            time: chrono::Utc::now().timestamp(),
            kind,
            name,
            path: path.to_string_lossy().into_owned(),
            folder,
            size,
            detail: detail.into(),
            percent: -1,
        }));
    }

    pub(crate) fn items_changed(&self, inos: &[u64]) {
        if inos.is_empty() {
            return;
        }
        let paths = inos.iter().map(|i| self.local_path(*i)).collect();
        self.emit(EngineEvent::ItemsChanged(paths));
    }

    /// A synced item was deleted on this device.
    pub(crate) fn note_local_delete(&self) {
        if !self.delete_noted.swap(true, Ordering::SeqCst) {
            self.emit(EngineEvent::LocalDelete);
        }
    }

    /// Wake the uploader (called by the FUSE layer after local changes).
    pub fn local_changed(&self) {
        self.push_wake.notify_one();
    }

    /// Ask for an immediate remote change check.
    pub fn refresh(&self) {
        self.refresh_requested.store(true, Ordering::SeqCst);
        self.pull_wake.notify_one();
    }

    // ------------------------------------------------------------ startup checks

    /// The content directory must still be on the device recorded at setup
    /// (an unmounted data volume must not look like "everything deleted").
    fn check_placement(&self) -> EngineResult<()> {
        use std::os::unix::fs::MetadataExt;
        let dev = std::fs::metadata(self.content.root())?.dev().to_string();
        let c = self.store.lock();
        match store::kv_get(&c, "content_dev")? {
            Some(d) if d != dev => {
                return Err(EngineError::Other(format!(
                    "the sync data folder {} moved to a different disk; refusing to sync",
                    self.content.root().display()
                )));
            }
            Some(_) => {}
            None => store::kv_set(&c, "content_dev", &dev)?,
        }
        Ok(())
    }

    /// Reconcile the database with the content directory after a crash.
    /// Never deletes data that might be unsynced: orphaned dirty content is
    /// salvaged into "Recovered files".
    fn fsck(&self) -> EngineResult<()> {
        let mut salvage: Vec<(u64, String)> = Vec::new();
        {
            let c = self.store.lock();
            let on_disk: HashSet<u64> = self.content.all_inodes().into_iter().collect();
            // Never hand out an inode number that still has a content file
            // (after a database rebuild the counter would start over).
            if let Some(max) = on_disk.iter().max() {
                c.execute(
                    "INSERT INTO sqlite_sequence(name, seq) SELECT 'items', ?1
                     WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'items')",
                    [*max as i64],
                )?;
                c.execute("UPDATE sqlite_sequence SET seq = MAX(seq, ?1) WHERE name = 'items'", [*max as i64])?;
            }
            for ino in &on_disk {
                match store::get(&c, *ino)? {
                    None => {
                        let (name, dirty) = self.content.read_tag(*ino);
                        let empty = self.content.len(*ino).map(|l| l == 0).unwrap_or(false);
                        if dirty && !(empty && name.is_empty()) {
                            salvage.push((*ino, name));
                        } else {
                            store::log_decision(
                                &c,
                                "remove-orphan-content",
                                Some(*ino),
                                None,
                                "",
                                "clean content without item",
                            )?;
                            self.content.remove(*ino)?;
                        }
                    }
                    Some(it) if it.deleted || it.is_dir => {
                        self.content.remove(*ino)?;
                    }
                    Some(it) if !it.hydrated => {
                        // Crash between installing a download and committing it.
                        let ok = match (&it.remote_hash, od_core::quickxor::hash_file(&self.content.path(*ino))) {
                            (Some(r), Ok(h)) => *r == h,
                            _ => false,
                        };
                        if ok {
                            store::set_hydrated(&c, *ino, true, it.remote_hash.as_deref())?;
                        } else {
                            self.content.remove(*ino)?;
                        }
                    }
                    Some(_) => {}
                }
            }
            // Hydrated rows whose content vanished.
            let mut st = c.prepare("SELECT ino FROM items WHERE hydrated = 1 AND is_dir = 0 AND deleted = 0")?;
            let rows: Vec<u64> =
                st.query_map([], |r| r.get::<_, i64>(0))?.filter_map(|r| r.ok()).map(|v| v as u64).collect();
            drop(st);
            for ino in rows {
                if !on_disk.contains(&ino) {
                    let it = store::get(&c, ino)?.ok_or(StoreError::NotFound)?;
                    let path = store::rel_path(&c, ino).unwrap_or_default();
                    if it.dirty || it.item_id.is_none() {
                        store::log_decision(
                            &c,
                            "lost-content",
                            Some(ino),
                            it.item_id.as_deref(),
                            &path,
                            "content file missing at startup",
                        )?;
                        store::set_error(
                            &c,
                            ino,
                            "content-missing",
                            "The local copy of this file was lost; the online version is kept.",
                            now_ms(),
                            true,
                        )?;
                    }
                    if it.item_id.is_none() {
                        store::remove_row(&c, ino)?;
                    } else {
                        c.execute("UPDATE items SET hydrated = 0, dirty = 0 WHERE ino = ?1", [ino as i64])?;
                    }
                }
            }
            // Rows whose parent vanished: re-home them in the root.
            let orphans: Vec<(u64, String)> = c
                .prepare(
                    "SELECT i.ino, i.name FROM items i LEFT JOIN items p ON p.ino = i.parent_ino
                     WHERE i.ino <> 1 AND p.ino IS NULL",
                )?
                .query_map([], |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            for (ino, name) in orphans {
                let new = unique_name(&c, ROOT_INO, &name, &self.cfg().hostname)?;
                store::set_name_parent(&c, ino, ROOT_INO, &new)?;
                store::log_decision(&c, "rehome-orphan", Some(ino), None, &new, "parent row missing")?;
            }
        }
        if !salvage.is_empty() {
            self.salvage(salvage)?;
        }
        Ok(())
    }

    /// Move orphaned unsynced content into "Recovered files (<date>)" as new local files.
    fn salvage(&self, items: Vec<(u64, String)>) -> EngineResult<()> {
        // Move every orphan aside first: new rows get fresh inode numbers that
        // could otherwise coincide with another orphan's content file.
        let mut staged = Vec::new();
        for (old_ino, name) in &items {
            let tmp = self.content.partial_path(*old_ino);
            std::fs::rename(self.content.path(*old_ino), &tmp)?;
            staged.push((*old_ino, name.clone(), tmp));
        }
        let folder_name = format!("Recovered files ({})", chrono::Local::now().format("%Y-%m-%d"));
        let c = self.store.lock();
        let folder = match store::child(&c, ROOT_INO, &folder_name)? {
            Some(f) => f.ino,
            None => store::insert(
                &c,
                &store::NewItem {
                    parent_ino: ROOT_INO,
                    name: folder_name.clone(),
                    is_dir: true,
                    mode: 0o755,
                    mtime_ns: now_ns(),
                    ctime_ns: now_ns(),
                    ..Default::default()
                },
            )?,
        };
        for (old_ino, name, tmp) in &staged {
            let name = if name.is_empty() { format!("file-{old_ino}") } else { name.clone() };
            let name = unique_name(&c, folder, &name, &self.cfg().hostname)?;
            let size = std::fs::metadata(tmp).map(|m| m.len()).unwrap_or(0);
            let ino = store::insert(
                &c,
                &store::NewItem {
                    parent_ino: folder,
                    name: name.clone(),
                    size,
                    mode: 0o644,
                    mtime_ns: now_ns(),
                    ctime_ns: now_ns(),
                    hydrated: true,
                    dirty: true,
                    ..Default::default()
                },
            )?;
            self.content.install(ino, tmp)?;
            self.content.tag(ino, &name, true);
            store::log_decision(
                &c,
                "salvage",
                Some(ino),
                None,
                &format!("{folder_name}/{name}"),
                "unsynced content without database entry",
            )?;
        }
        drop(c);
        self.notice(
            NoticeKind::Recovered,
            "Some files were recovered",
            &format!("{} file(s) with unsynced changes were saved to \"{folder_name}\".", items.len()),
            Some(self.mount_point().join(&folder_name)),
        );
        Ok(())
    }

    /// Local-only helper files exposed in the mount root (never synced).
    fn ensure_special_files(&self) -> EngineResult<()> {
        let c = self.store.lock();
        // LocalSearch indexes $HOME recursively and would download every file
        // to extract text; this marker keeps it out (research doc 03 §6.6).
        if store::child_folded(&c, ROOT_INO, ".trackerignore")?.is_none() {
            let ino = store::insert(
                &c,
                &store::NewItem {
                    parent_ino: ROOT_INO,
                    name: ".trackerignore".into(),
                    mode: 0o444,
                    hydrated: true,
                    local_only: Some(LocalOnly::Ignored),
                    mtime_ns: now_ns(),
                    ctime_ns: now_ns(),
                    ..Default::default()
                },
            )?;
            self.content.create_empty(ino)?;
            self.content.tag(ino, ".trackerignore", false);
        }
        Ok(())
    }

    /// Take a database backup right away (used before risky operations and by tests).
    pub fn backup_db_now(&self) {
        self.backup_db();
    }

    /// Hourly online backup of the database (keeps three generations).
    pub(crate) fn backup_db(&self) {
        let dir = &self.cfg().account_dir;
        let b1 = dir.join("state.db.bak.1");
        let _ = std::fs::rename(dir.join("state.db.bak.2"), dir.join("state.db.bak.3"));
        let _ = std::fs::rename(&b1, dir.join("state.db.bak.2"));
        if let Err(e) = self.store.backup_to(&b1) {
            tracing::warn!("database backup failed: {e}");
        }
    }
}

/// A name not used (case-insensitively) in `parent`: `name`, then conflict-style variants.
pub(crate) fn unique_name(c: &rusqlite::Connection, parent: u64, name: &str, host: &str) -> EngineResult<String> {
    if store::child_folded(c, parent, name)?.is_none() {
        return Ok(name.to_string());
    }
    for n in 1..1000 {
        let candidate = names::conflict_name(name, host, n);
        if store::child_folded(c, parent, &candidate)?.is_none() {
            return Ok(candidate);
        }
    }
    Err(EngineError::Other("no free name".into()))
}

/// Open the database; on corruption, move it aside (never delete it) and fall
/// back to the newest usable backup, or start fresh.
/// Marker written when a runtime integrity check failed: the next start
/// treats the database as corrupt and recovers it.
pub(crate) const REPAIR_MARKER: &str = "state.db.needs-repair";

fn open_store_with_recovery(dir: &Path) -> EngineResult<(Store, Option<PathBuf>)> {
    let db = dir.join("state.db");
    let marker = dir.join(REPAIR_MARKER);
    if marker.exists() {
        tracing::error!("the database was flagged as damaged; recovering");
        let _ = std::fs::remove_file(&marker);
    } else {
        match Store::open(&db) {
            Ok(s) => return Ok((s, None)),
            Err(e) => tracing::error!("cannot open {}: {e}; recovering", db.display()),
        }
    }
    if !db.exists() {
        return Ok((Store::open(&db)?, None));
    }
    let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let aside = dir.join(format!("state.db.corrupt-{stamp}"));
    std::fs::rename(&db, &aside)?;
    for ext in ["-wal", "-shm"] {
        let p = dir.join(format!("state.db{ext}"));
        if p.exists() {
            let _ = std::fs::rename(&p, dir.join(format!("state.db.corrupt-{stamp}{ext}")));
        }
    }
    for n in 1..=3 {
        let bak = dir.join(format!("state.db.bak.{n}"));
        if bak.exists() {
            std::fs::copy(&bak, &db)?;
            match Store::open(&db) {
                Ok(s) => {
                    tracing::warn!("restored database from {}", bak.display());
                    return Ok((s, Some(aside)));
                }
                Err(e) => {
                    tracing::error!("backup {} unusable: {e}", bak.display());
                    let _ = std::fs::remove_file(&db);
                }
            }
        }
    }
    Ok((Store::open(&db)?, Some(aside)))
}
