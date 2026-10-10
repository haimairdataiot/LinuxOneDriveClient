//! SQLite metadata store: one row per file or folder, holding both the local
//! state (what the FUSE mount shows) and the last state known to match the
//! server. The sync engine is *state based*: it derives the work to do from
//! the difference between the two, so it needs no operation log, coalesces
//! repeated edits automatically and re-derives pending work after a crash.
//!
//! Inode numbers are the table's `AUTOINCREMENT` primary key: stable for the
//! life of the database and never reused, so the kernel never sees a recycled
//! inode with different content.

use od_core::names;
use od_core::types::PinState;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

pub const ROOT_INO: u64 = 1;
const SCHEMA_VERSION: i64 = 1;

/// Why an item is kept on this device only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalOnly {
    /// Normal item: synced.
    No = 0,
    /// Matches an ignore pattern (lock/temp/trash files): silently local.
    Ignored = 1,
    /// OneDrive would reject it (name, path length, size): reported as a sync issue.
    Invalid = 2,
    /// Inside a folder hidden by "Choose folders" (not shown, not synced).
    Excluded = 3,
}

impl LocalOnly {
    fn from_i64(v: i64) -> Self {
        match v {
            1 => LocalOnly::Ignored,
            2 => LocalOnly::Invalid,
            3 => LocalOnly::Excluded,
            _ => LocalOnly::No,
        }
    }
}

fn pin_from_i64(v: i64) -> PinState {
    match v {
        1 => PinState::AlwaysKeep,
        2 => PinState::OnlineOnly,
        _ => PinState::Inherit,
    }
}

pub fn pin_to_i64(p: PinState) -> i64 {
    match p {
        PinState::Inherit => 0,
        PinState::AlwaysKeep => 1,
        PinState::OnlineOnly => 2,
    }
}

/// One file or folder.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub ino: u64,
    /// Server id; `None` until the item has been created on the server.
    pub item_id: Option<String>,
    /// Local parent (None only for the root).
    pub parent_ino: Option<u64>,
    pub name: String,
    pub is_dir: bool,
    /// Local size (what `stat` reports).
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub mode: u32,

    // Last state known to be identical on the server.
    pub etag: Option<String>,
    pub ctag: Option<String>,
    pub remote_hash: Option<String>,
    pub remote_size: Option<u64>,
    pub remote_mtime_ns: Option<i64>,
    pub remote_parent_id: Option<String>,
    pub remote_name: Option<String>,
    pub web_url: Option<String>,
    pub shared: bool,

    pub pin: PinState,
    /// Content is present in the content cache.
    pub hydrated: bool,
    /// Hash of the cached content when it was last known to match the server.
    pub content_hash: Option<String>,
    /// Local content differs from the server (needs upload).
    pub dirty: bool,
    /// Deleted locally; the server delete is still pending. Invisible in the mount.
    pub deleted: bool,
    pub deleted_at: i64,
    pub local_only: LocalOnly,

    pub error_code: Option<String>,
    pub error_msg: Option<String>,
    pub attempts: u32,
    /// Unix ms before which the item is not retried (error backoff).
    pub retry_at: i64,

    /// Resumable upload session, and the hash of the snapshot it uploads.
    pub upload_url: Option<String>,
    pub upload_offset: u64,
    pub upload_hash: Option<String>,

    /// Server id this local-only item was moved from (e.g. into the Trash):
    /// its content is still downloadable until the server delete happens.
    pub origin_id: Option<String>,

    /// Unix seconds of the last open (for automatic "free up space").
    pub last_access: i64,
}

impl Item {
    /// Cached content is clean but older than the server version.
    pub fn is_stale(&self) -> bool {
        self.hydrated && !self.dirty && self.remote_hash.is_some() && self.content_hash != self.remote_hash
    }

    /// Local name/parent no longer match the server: a move/rename is pending.
    pub fn needs_move(&self, parent_item_id: Option<&str>) -> bool {
        self.item_id.is_some()
            && (self.remote_name.as_deref() != Some(self.name.as_str())
                || self.remote_parent_id.as_deref() != parent_item_id)
    }
}

const COLUMNS: &str = "ino, item_id, parent_ino, name, is_dir, size, mtime_ns, ctime_ns, mode, \
    etag, ctag, remote_hash, remote_size, remote_mtime_ns, remote_parent_id, remote_name, web_url, shared, \
    pin, hydrated, content_hash, dirty, deleted, deleted_at, local_only, \
    error_code, error_msg, attempts, retry_at, upload_url, upload_offset, upload_hash, origin_id, last_access";

fn from_row(r: &Row<'_>) -> rusqlite::Result<Item> {
    Ok(Item {
        ino: r.get::<_, i64>(0)? as u64,
        item_id: r.get(1)?,
        parent_ino: r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
        name: r.get(3)?,
        is_dir: r.get::<_, i64>(4)? != 0,
        size: r.get::<_, i64>(5)? as u64,
        mtime_ns: r.get(6)?,
        ctime_ns: r.get(7)?,
        mode: r.get::<_, i64>(8)? as u32,
        etag: r.get(9)?,
        ctag: r.get(10)?,
        remote_hash: r.get(11)?,
        remote_size: r.get::<_, Option<i64>>(12)?.map(|v| v as u64),
        remote_mtime_ns: r.get(13)?,
        remote_parent_id: r.get(14)?,
        remote_name: r.get(15)?,
        web_url: r.get(16)?,
        shared: r.get::<_, i64>(17)? != 0,
        pin: pin_from_i64(r.get(18)?),
        hydrated: r.get::<_, i64>(19)? != 0,
        content_hash: r.get(20)?,
        dirty: r.get::<_, i64>(21)? != 0,
        deleted: r.get::<_, i64>(22)? != 0,
        deleted_at: r.get(23)?,
        local_only: LocalOnly::from_i64(r.get(24)?),
        error_code: r.get(25)?,
        error_msg: r.get(26)?,
        attempts: r.get::<_, i64>(27)? as u32,
        retry_at: r.get(28)?,
        upload_url: r.get(29)?,
        upload_offset: r.get::<_, i64>(30)? as u64,
        upload_hash: r.get(31)?,
        origin_id: r.get(32)?,
        last_access: r.get(33)?,
    })
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS items (
    ino              INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id          TEXT UNIQUE,
    parent_ino       INTEGER,
    name             TEXT NOT NULL,
    name_fold        TEXT NOT NULL,
    is_dir           INTEGER NOT NULL,
    size             INTEGER NOT NULL DEFAULT 0,
    mtime_ns         INTEGER NOT NULL DEFAULT 0,
    ctime_ns         INTEGER NOT NULL DEFAULT 0,
    mode             INTEGER NOT NULL DEFAULT 0,
    etag             TEXT,
    ctag             TEXT,
    remote_hash      TEXT,
    remote_size      INTEGER,
    remote_mtime_ns  INTEGER,
    remote_parent_id TEXT,
    remote_name      TEXT,
    web_url          TEXT,
    shared           INTEGER NOT NULL DEFAULT 0,
    pin              INTEGER NOT NULL DEFAULT 0,
    hydrated         INTEGER NOT NULL DEFAULT 0,
    content_hash     TEXT,
    dirty            INTEGER NOT NULL DEFAULT 0,
    deleted          INTEGER NOT NULL DEFAULT 0,
    deleted_at       INTEGER NOT NULL DEFAULT 0,
    local_only       INTEGER NOT NULL DEFAULT 0,
    error_code       TEXT,
    error_msg        TEXT,
    attempts         INTEGER NOT NULL DEFAULT 0,
    retry_at         INTEGER NOT NULL DEFAULT 0,
    upload_url       TEXT,
    upload_offset    INTEGER NOT NULL DEFAULT 0,
    upload_hash      TEXT,
    origin_id        TEXT,
    last_access      INTEGER NOT NULL DEFAULT 0
);
-- OneDrive is case-insensitive: one live name per folder, compared case-folded.
CREATE UNIQUE INDEX IF NOT EXISTS items_live_child ON items(parent_ino, name_fold) WHERE deleted = 0;
CREATE INDEX IF NOT EXISTS items_parent ON items(parent_ino);
CREATE INDEX IF NOT EXISTS items_work ON items(dirty, deleted, item_id);

-- Delta pages staged until the whole round has arrived (applied atomically).
CREATE TABLE IF NOT EXISTS delta_stage (
    item_id TEXT PRIMARY KEY,
    seq     INTEGER NOT NULL,
    json    TEXT NOT NULL
);

-- Every destructive decision (delete, overwrite, dehydrate, conflict) with its
-- provenance, kept 30 days for diagnosis (research doc 04 §10.6).
CREATE TABLE IF NOT EXISTS decision_log (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    time    INTEGER NOT NULL,
    action  TEXT NOT NULL,
    ino     INTEGER,
    item_id TEXT,
    path    TEXT NOT NULL,
    reason  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS kv (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS activity (
    id     INTEGER PRIMARY KEY AUTOINCREMENT,
    time   INTEGER NOT NULL,
    kind   TEXT NOT NULL,
    ino    INTEGER,
    name   TEXT NOT NULL,
    path   TEXT NOT NULL,
    folder TEXT NOT NULL,
    size   INTEGER NOT NULL DEFAULT 0,
    detail TEXT NOT NULL DEFAULT ''
);
"#;

/// Thread-safe handle to the account database.
pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("database is corrupt: {0}")]
    Corrupt(String),
    #[error("an item with this name already exists")]
    Exists,
    #[error("no such item")]
    NotFound,
}

pub type StoreResult<T> = Result<T, StoreError>;

impl Store {
    /// Open (creating if needed) and verify the database.
    pub fn open(path: &Path) -> StoreResult<Store> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> StoreResult<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> StoreResult<Store> {
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // FULL: a committed local change (and therefore its pending upload)
        // survives power loss.
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(StoreError::Corrupt(check));
        }
        conn.execute_batch(SCHEMA)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        let store = Store { conn: Mutex::new(conn) };
        store.ensure_root()?;
        Ok(store)
    }

    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `f` inside one transaction.
    pub fn tx<T>(&self, f: impl FnOnce(&rusqlite::Transaction<'_>) -> StoreResult<T>) -> StoreResult<T> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// Full `PRAGMA integrity_check` (slow; run when idle).
    pub fn integrity_check(&self) -> StoreResult<()> {
        let conn = self.lock();
        let mut st = conn.prepare("PRAGMA integrity_check")?;
        let msgs: Vec<String> = st.query_map([], |r| r.get(0))?.filter_map(|r| r.ok()).collect();
        if msgs.len() == 1 && msgs[0] == "ok" { Ok(()) } else { Err(StoreError::Corrupt(msgs.join("; "))) }
    }

    /// Online backup to `dest` (consistent snapshot while in use).
    pub fn backup_to(&self, dest: &Path) -> StoreResult<()> {
        let tmp = dest.with_extension("tmp");
        let _ = std::fs::remove_file(&tmp);
        {
            let conn = self.lock();
            conn.execute("VACUUM INTO ?1", params![tmp.to_string_lossy()])?;
        }
        std::fs::rename(&tmp, dest).map_err(|e| StoreError::Corrupt(format!("backup rename: {e}")))?;
        Ok(())
    }

    fn ensure_root(&self) -> StoreResult<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR IGNORE INTO items (ino, parent_ino, name, name_fold, is_dir, mode, pin)
             VALUES (?1, NULL, '', '', 1, 493, 0)",
            params![ROOT_INO as i64],
        )?;
        Ok(())
    }
}

// ------------------------------------------------------------------ queries
// Free functions over a `Connection` so they compose inside transactions.

pub fn get(c: &Connection, ino: u64) -> StoreResult<Option<Item>> {
    Ok(c.prepare_cached(&format!("SELECT {COLUMNS} FROM items WHERE ino = ?1"))?
        .query_row(params![ino as i64], from_row)
        .optional()?)
}

pub fn get_live(c: &Connection, ino: u64) -> StoreResult<Option<Item>> {
    Ok(get(c, ino)?.filter(|i| !i.deleted))
}

pub fn by_item_id(c: &Connection, item_id: &str) -> StoreResult<Option<Item>> {
    Ok(c.prepare_cached(&format!("SELECT {COLUMNS} FROM items WHERE item_id = ?1"))?
        .query_row(params![item_id], from_row)
        .optional()?)
}

/// Live child by exact name.
pub fn child(c: &Connection, parent: u64, name: &str) -> StoreResult<Option<Item>> {
    Ok(c.prepare_cached(&format!("SELECT {COLUMNS} FROM items WHERE parent_ino = ?1 AND name = ?2 AND deleted = 0"))?
        .query_row(params![parent as i64, name], from_row)
        .optional()?)
}

/// Live child whose name equals `name` case-insensitively.
pub fn child_folded(c: &Connection, parent: u64, name: &str) -> StoreResult<Option<Item>> {
    Ok(c.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM items WHERE parent_ino = ?1 AND name_fold = ?2 AND deleted = 0"
    ))?
    .query_row(params![parent as i64, names::fold(name)], from_row)
    .optional()?)
}

/// Live children sorted by name.
pub fn children(c: &Connection, parent: u64) -> StoreResult<Vec<Item>> {
    let mut st = c.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM items WHERE parent_ino = ?1 AND deleted = 0 AND ino <> ?1 ORDER BY name"
    ))?;
    let rows = st.query_map(params![parent as i64], from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// All rows (live and tombstones) directly under `parent`.
pub fn children_all(c: &Connection, parent: u64) -> StoreResult<Vec<Item>> {
    let mut st = c.prepare_cached(&format!("SELECT {COLUMNS} FROM items WHERE parent_ino = ?1 AND ino <> ?1"))?;
    let rows = st.query_map(params![parent as i64], from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn count_children(c: &Connection, parent: u64) -> StoreResult<u64> {
    let n: i64 = c
        .prepare_cached("SELECT COUNT(*) FROM items WHERE parent_ino = ?1 AND deleted = 0 AND ino <> ?1")?
        .query_row(params![parent as i64], |r| r.get(0))?;
    Ok(n as u64)
}

/// Every descendant ino (depth-first, parents before children), excluding `ino`.
pub fn descendants(c: &Connection, ino: u64) -> StoreResult<Vec<u64>> {
    let mut out = Vec::new();
    let mut stack = vec![ino];
    let mut st = c.prepare_cached("SELECT ino FROM items WHERE parent_ino = ?1 AND ino <> ?1")?;
    while let Some(cur) = stack.pop() {
        let kids: Vec<u64> = st
            .query_map(params![cur as i64], |r| r.get::<_, i64>(0))?
            .map(|r| r.map(|v| v as u64))
            .collect::<Result<_, _>>()?;
        for k in kids {
            out.push(k);
            stack.push(k);
        }
    }
    Ok(out)
}

/// Path relative to the mount root, e.g. `Documents/report.docx` ("" for the root).
pub fn rel_path(c: &Connection, ino: u64) -> StoreResult<String> {
    let mut parts = Vec::new();
    let mut cur = ino;
    let mut guard = 0;
    while cur != ROOT_INO {
        let (name, parent): (String, Option<i64>) = c
            .prepare_cached("SELECT name, parent_ino FROM items WHERE ino = ?1")?
            .query_row(params![cur as i64], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?
            .ok_or(StoreError::NotFound)?;
        parts.push(name);
        cur = parent.map(|p| p as u64).unwrap_or(ROOT_INO);
        guard += 1;
        if guard > 10_000 {
            return Err(StoreError::Corrupt("parent cycle".into()));
        }
    }
    parts.reverse();
    Ok(parts.join("/"))
}

/// Resolve a path relative to the mount root to a live item.
pub fn resolve(c: &Connection, rel: &str) -> StoreResult<Option<Item>> {
    let mut cur = match get(c, ROOT_INO)? {
        Some(r) => r,
        None => return Ok(None),
    };
    for seg in rel.split('/').filter(|s| !s.is_empty()) {
        match child(c, cur.ino, seg)? {
            Some(i) => cur = i,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

/// Effective pin state: the nearest explicit pin among the item and its ancestors.
pub fn effective_pin(c: &Connection, ino: u64) -> StoreResult<PinState> {
    let mut cur = Some(ino);
    let mut st = c.prepare_cached("SELECT pin, parent_ino FROM items WHERE ino = ?1")?;
    let mut guard = 0;
    while let Some(i) = cur {
        let row: Option<(i64, Option<i64>)> =
            st.query_row(params![i as i64], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((pin, parent)) = row else { break };
        let pin = pin_from_i64(pin);
        if pin != PinState::Inherit {
            return Ok(pin);
        }
        cur = parent.map(|p| p as u64);
        guard += 1;
        if guard > 10_000 {
            break;
        }
    }
    Ok(PinState::Inherit)
}

/// Fields for a new row.
#[derive(Debug, Clone, Default)]
pub struct NewItem {
    pub item_id: Option<String>,
    pub parent_ino: u64,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub mode: u32,
    pub hydrated: bool,
    pub dirty: bool,
    pub local_only: Option<LocalOnly>,
}

/// Insert a new live item. Fails with `Exists` on a case-insensitive name clash.
pub fn insert(c: &Connection, n: &NewItem) -> StoreResult<u64> {
    if child_folded(c, n.parent_ino, &n.name)?.is_some() {
        return Err(StoreError::Exists);
    }
    c.prepare_cached(
        "INSERT INTO items (item_id, parent_ino, name, name_fold, is_dir, size, mtime_ns, ctime_ns, mode,
                            hydrated, dirty, local_only)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?
    .execute(params![
        n.item_id,
        n.parent_ino as i64,
        n.name,
        names::fold(&n.name),
        n.is_dir as i64,
        n.size as i64,
        n.mtime_ns,
        n.ctime_ns,
        n.mode as i64,
        n.hydrated as i64,
        n.dirty as i64,
        n.local_only.unwrap_or(LocalOnly::No) as i64,
    ])?;
    Ok(c.last_insert_rowid() as u64)
}

/// Move/rename a row locally (the caller has checked for clashes).
pub fn set_name_parent(c: &Connection, ino: u64, parent: u64, name: &str) -> StoreResult<()> {
    c.prepare_cached("UPDATE items SET parent_ino = ?2, name = ?3, name_fold = ?4 WHERE ino = ?1")?
        .execute(params![ino as i64, parent as i64, name, names::fold(name)])?;
    Ok(())
}

pub fn set_local_only(c: &Connection, ino: u64, v: LocalOnly) -> StoreResult<()> {
    c.prepare_cached("UPDATE items SET local_only = ?2 WHERE ino = ?1")?.execute(params![ino as i64, v as i64])?;
    Ok(())
}

pub fn set_pin(c: &Connection, ino: u64, pin: PinState) -> StoreResult<()> {
    c.prepare_cached("UPDATE items SET pin = ?2 WHERE ino = ?1")?.execute(params![ino as i64, pin_to_i64(pin)])?;
    Ok(())
}

pub fn set_size_mtime(c: &Connection, ino: u64, size: u64, mtime_ns: i64) -> StoreResult<()> {
    c.prepare_cached("UPDATE items SET size = ?2, mtime_ns = ?3 WHERE ino = ?1")?.execute(params![
        ino as i64,
        size as i64,
        mtime_ns
    ])?;
    Ok(())
}

pub fn set_mtime(c: &Connection, ino: u64, mtime_ns: i64) -> StoreResult<()> {
    c.prepare_cached("UPDATE items SET mtime_ns = ?2 WHERE ino = ?1")?.execute(params![ino as i64, mtime_ns])?;
    Ok(())
}

pub fn set_mode(c: &Connection, ino: u64, mode: u32) -> StoreResult<()> {
    c.prepare_cached("UPDATE items SET mode = ?2 WHERE ino = ?1")?.execute(params![ino as i64, mode as i64])?;
    Ok(())
}

/// Local content changed: needs upload. Clears error backoff so it is retried.
pub fn mark_dirty(c: &Connection, ino: u64) -> StoreResult<()> {
    c.prepare_cached(
        "UPDATE items SET dirty = 1, hydrated = 1, attempts = 0, retry_at = 0,
                          error_code = NULL, error_msg = NULL WHERE ino = ?1",
    )?
    .execute(params![ino as i64])?;
    Ok(())
}

pub fn set_hydrated(c: &Connection, ino: u64, hydrated: bool, content_hash: Option<&str>) -> StoreResult<()> {
    c.prepare_cached("UPDATE items SET hydrated = ?2, content_hash = ?3 WHERE ino = ?1")?.execute(params![
        ino as i64,
        hydrated as i64,
        content_hash
    ])?;
    Ok(())
}

pub fn touch_access(c: &Connection, ino: u64, now_secs: i64) -> StoreResult<()> {
    c.prepare_cached("UPDATE items SET last_access = ?2 WHERE ino = ?1")?.execute(params![ino as i64, now_secs])?;
    Ok(())
}

/// Turn a live row into a tombstone (server delete pending).
pub fn tombstone(c: &Connection, ino: u64, now_ms: i64) -> StoreResult<()> {
    c.prepare_cached(
        "UPDATE items SET deleted = 1, deleted_at = ?2, hydrated = 0, dirty = 0,
                          upload_url = NULL, upload_offset = 0, upload_hash = NULL WHERE ino = ?1",
    )?
    .execute(params![ino as i64, now_ms])?;
    Ok(())
}

pub fn untombstone(c: &Connection, ino: u64) -> StoreResult<()> {
    c.prepare_cached("UPDATE items SET deleted = 0, deleted_at = 0 WHERE ino = ?1")?.execute(params![ino as i64])?;
    Ok(())
}

pub fn remove_row(c: &Connection, ino: u64) -> StoreResult<()> {
    if ino == ROOT_INO {
        return Ok(());
    }
    c.prepare_cached("DELETE FROM items WHERE ino = ?1")?.execute(params![ino as i64])?;
    Ok(())
}

/// Server state after a successful create/upload/move/download.
#[derive(Debug, Clone, Default)]
pub struct RemoteState {
    pub item_id: String,
    pub etag: Option<String>,
    pub ctag: Option<String>,
    pub hash: Option<String>,
    pub size: Option<u64>,
    pub mtime_ns: Option<i64>,
    pub parent_id: Option<String>,
    pub name: Option<String>,
    pub web_url: Option<String>,
    pub shared: bool,
}

impl RemoteState {
    pub fn from_item(it: &od_graph::DriveItem) -> RemoteState {
        RemoteState {
            item_id: it.id.clone(),
            etag: it.e_tag.clone(),
            ctag: it.c_tag.clone(),
            hash: it.quick_xor_hash().map(str::to_string),
            size: it.size.map(|s| s.max(0) as u64),
            mtime_ns: it.modified().map(|t| t.timestamp_nanos_opt().unwrap_or(0)),
            parent_id: it.parent_id().map(str::to_string),
            name: it.name.clone(),
            web_url: it.web_url.clone(),
            shared: it.shared.is_some(),
        }
    }
}

/// Record the server's view of an item (does not touch local name/content flags).
pub fn set_remote(c: &Connection, ino: u64, r: &RemoteState) -> StoreResult<()> {
    c.prepare_cached(
        "UPDATE items SET item_id = ?2, etag = ?3, ctag = COALESCE(?4, ctag), remote_hash = COALESCE(?5, remote_hash),
                remote_size = COALESCE(?6, remote_size), remote_mtime_ns = COALESCE(?7, remote_mtime_ns),
                remote_parent_id = COALESCE(?8, remote_parent_id), remote_name = COALESCE(?9, remote_name),
                web_url = COALESCE(?10, web_url), shared = ?11
         WHERE ino = ?1",
    )?
    .execute(params![
        ino as i64,
        r.item_id,
        r.etag,
        r.ctag,
        r.hash,
        r.size.map(|s| s as i64),
        r.mtime_ns,
        r.parent_id,
        r.name,
        r.web_url,
        r.shared as i64,
    ])?;
    Ok(())
}

/// Record only the server-side name/parent/eTag (after a move), keeping the
/// content baseline (cTag/hash/size) that conflict detection relies on.
pub fn set_remote_meta(
    c: &Connection,
    ino: u64,
    etag: Option<&str>,
    parent_id: Option<&str>,
    name: Option<&str>,
) -> StoreResult<()> {
    c.prepare_cached(
        "UPDATE items SET etag = COALESCE(?2, etag), remote_parent_id = COALESCE(?3, remote_parent_id),
                remote_name = COALESCE(?4, remote_name) WHERE ino = ?1",
    )?
    .execute(params![ino as i64, etag, parent_id, name])?;
    Ok(())
}

/// Clean cached files whose server version moved on (to re-download).
pub fn stale_items(c: &Connection, limit: usize) -> StoreResult<Vec<u64>> {
    let mut st = c.prepare_cached(
        "SELECT ino FROM items WHERE is_dir = 0 AND deleted = 0 AND hydrated = 1 AND dirty = 0
           AND item_id IS NOT NULL AND remote_hash IS NOT NULL
           AND (content_hash IS NULL OR content_hash <> remote_hash) AND retry_at <= ?1 LIMIT ?2",
    )?;
    let rows = st.query_map(params![chrono::Utc::now().timestamp_millis(), limit as i64], |r| r.get::<_, i64>(0))?;
    Ok(rows.map(|r| r.map(|v| v as u64)).collect::<Result<_, _>>()?)
}

/// Clean, unpinned cached files not opened since `before` (unix seconds).
pub fn eviction_candidates(c: &Connection, before: i64, limit: usize) -> StoreResult<Vec<u64>> {
    let mut st = c.prepare_cached(
        "SELECT ino FROM items WHERE is_dir = 0 AND deleted = 0 AND hydrated = 1 AND dirty = 0
           AND item_id IS NOT NULL AND local_only = 0 AND last_access < ?1 LIMIT ?2",
    )?;
    let rows = st.query_map(params![before, limit as i64], |r| r.get::<_, i64>(0))?;
    Ok(rows.map(|r| r.map(|v| v as u64)).collect::<Result<_, _>>()?)
}

/// Number of synced items (for the mass-delete ratio).
pub fn synced_count(c: &Connection) -> StoreResult<u64> {
    let n: i64 = c.query_row("SELECT COUNT(*) FROM items WHERE item_id IS NOT NULL AND ino <> 1", [], |r| r.get(0))?;
    Ok(n as u64)
}

/// Detach an item from the server for a move into a local-only area,
/// remembering where its content can still be downloaded from.
pub fn detach_to_origin(c: &Connection, ino: u64) -> StoreResult<()> {
    c.prepare_cached(
        "UPDATE items SET origin_id = item_id, item_id = NULL, etag = NULL, ctag = NULL,
                remote_parent_id = NULL, remote_name = NULL, web_url = NULL, shared = 0,
                upload_url = NULL, upload_offset = 0, upload_hash = NULL WHERE ino = ?1",
    )?
    .execute(params![ino as i64])?;
    Ok(())
}

/// Local-only files whose content must be downloaded (from `origin_id`)
/// before their server copy may be deleted.
pub fn pending_origin_downloads(c: &Connection) -> StoreResult<Vec<u64>> {
    let mut st = c.prepare_cached(
        "SELECT ino FROM items WHERE origin_id IS NOT NULL AND hydrated = 0 AND is_dir = 0 AND deleted = 0",
    )?;
    let rows = st.query_map([], |r| r.get::<_, i64>(0))?;
    Ok(rows.map(|r| r.map(|v| v as u64)).collect::<Result<_, _>>()?)
}

/// Forget the server identity (the item will be uploaded as new).
pub fn clear_remote(c: &Connection, ino: u64) -> StoreResult<()> {
    c.prepare_cached(
        "UPDATE items SET item_id = NULL, etag = NULL, ctag = NULL, remote_hash = NULL, remote_size = NULL,
                remote_mtime_ns = NULL, remote_parent_id = NULL, remote_name = NULL, web_url = NULL,
                shared = 0, upload_url = NULL, upload_offset = 0, upload_hash = NULL WHERE ino = ?1",
    )?
    .execute(params![ino as i64])?;
    Ok(())
}

pub fn mark_clean(c: &Connection, ino: u64, content_hash: Option<&str>) -> StoreResult<()> {
    c.prepare_cached(
        "UPDATE items SET dirty = 0, content_hash = ?2, attempts = 0, retry_at = 0,
                error_code = NULL, error_msg = NULL, upload_url = NULL, upload_offset = 0, upload_hash = NULL
         WHERE ino = ?1",
    )?
    .execute(params![ino as i64, content_hash])?;
    Ok(())
}

/// Backoff after `attempts` consecutive failures: 25 s × 5ⁿ, capped at 24 h
/// (Nextcloud's blacklist schedule; research doc 04 §6).
pub fn backoff_ms(attempts: u32) -> i64 {
    let n = attempts.saturating_sub(1).min(6);
    (25_000i64 * 5i64.pow(n)).min(86_400_000)
}

/// Record a failure with exponential backoff. `permanent` errors are not
/// retried until the item changes (any change clears the error).
pub fn set_error(c: &Connection, ino: u64, code: &str, msg: &str, now_ms: i64, permanent: bool) -> StoreResult<u32> {
    let attempts: i64 = c
        .prepare_cached("SELECT attempts FROM items WHERE ino = ?1")?
        .query_row(params![ino as i64], |r| r.get(0))
        .optional()?
        .unwrap_or(0)
        + 1;
    let delay_ms = if permanent {
        i64::MAX / 4
    } else if code == "accessDenied" {
        backoff_ms(attempts as u32).min(3_600_000)
    } else {
        backoff_ms(attempts as u32)
    };
    c.prepare_cached("UPDATE items SET error_code = ?2, error_msg = ?3, attempts = ?4, retry_at = ?5 WHERE ino = ?1")?
        .execute(params![ino as i64, code, msg, attempts, now_ms.saturating_add(delay_ms)])?;
    Ok(attempts as u32)
}

pub fn clear_error(c: &Connection, ino: u64) -> StoreResult<()> {
    c.prepare_cached(
        "UPDATE items SET error_code = NULL, error_msg = NULL, attempts = 0, retry_at = 0 WHERE ino = ?1",
    )?
    .execute(params![ino as i64])?;
    Ok(())
}

/// Clear backoff of every failed item ("Retry" in Sync issues / network back).
pub fn reset_backoff(c: &Connection) -> StoreResult<()> {
    c.execute("UPDATE items SET retry_at = 0 WHERE retry_at > 0 AND retry_at < ?1", params![i64::MAX / 8])?;
    Ok(())
}

pub fn set_upload_session(
    c: &Connection,
    ino: u64,
    url: Option<&str>,
    offset: u64,
    snapshot_hash: Option<&str>,
) -> StoreResult<()> {
    c.prepare_cached("UPDATE items SET upload_url = ?2, upload_offset = ?3, upload_hash = ?4 WHERE ino = ?1")?
        .execute(params![ino as i64, url, offset as i64, snapshot_hash])?;
    Ok(())
}

// ------------------------------------------------------------------ decision log

pub fn log_decision(
    c: &Connection,
    action: &str,
    ino: Option<u64>,
    item_id: Option<&str>,
    path: &str,
    reason: &str,
) -> StoreResult<()> {
    c.prepare_cached(
        "INSERT INTO decision_log (time, action, ino, item_id, path, reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![chrono::Utc::now().timestamp(), action, ino.map(|v| v as i64), item_id, path, reason])?;
    Ok(())
}

pub fn prune_decision_log(c: &Connection, older_than_secs: i64) -> StoreResult<usize> {
    Ok(c.execute("DELETE FROM decision_log WHERE time < ?1", params![older_than_secs])?)
}

pub fn recent_decisions(c: &Connection, limit: u32) -> StoreResult<Vec<(i64, String, String, String)>> {
    let mut st = c.prepare_cached("SELECT time, action, path, reason FROM decision_log ORDER BY id DESC LIMIT ?1")?;
    let rows = st.query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Items with problems for the Sync issues list.
pub fn issues(c: &Connection) -> StoreResult<Vec<Item>> {
    let mut st = c.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM items WHERE error_code IS NOT NULL ORDER BY name LIMIT 1000"
    ))?;
    let rows = st.query_map([], from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn issue_count(c: &Connection) -> StoreResult<u32> {
    let n: i64 = c.query_row("SELECT COUNT(*) FROM items WHERE error_code IS NOT NULL", [], |r| r.get(0))?;
    Ok(n as u32)
}

// ------------------------------------------------------------------ work queries

/// Live items that need to be pushed (created, uploaded or moved) and are not in backoff.
pub fn pending_push(c: &Connection, now_ms: i64, limit: usize) -> StoreResult<Vec<Item>> {
    let mut st = c.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM items i
         WHERE deleted = 0 AND local_only = 0 AND ino <> {ROOT_INO} AND retry_at <= ?1
           AND (item_id IS NULL OR dirty = 1
                OR remote_name IS NOT name
                OR remote_parent_id IS NOT (SELECT p.item_id FROM items p WHERE p.ino = i.parent_ino))
         ORDER BY is_dir DESC, ino
         LIMIT ?2"
    ))?;
    let rows = st.query_map(params![now_ms, limit as i64], from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Tombstones whose server delete is still pending.
pub fn pending_deletes(c: &Connection) -> StoreResult<Vec<Item>> {
    let mut st = c.prepare_cached(&format!("SELECT {COLUMNS} FROM items WHERE deleted = 1 ORDER BY ino"))?;
    let rows = st.query_map([], from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Pinned-but-not-downloaded files (by effective pin), up to `limit`.
pub fn hydration_candidates(c: &Connection, limit: usize) -> StoreResult<Vec<u64>> {
    let mut st = c.prepare_cached(
        "SELECT ino FROM items WHERE is_dir = 0 AND deleted = 0 AND hydrated = 0 AND item_id IS NOT NULL
           AND retry_at <= ?1 ORDER BY ino",
    )?;
    let now = chrono::Utc::now().timestamp_millis();
    let all: Vec<u64> =
        st.query_map(params![now], |r| r.get::<_, i64>(0))?.map(|r| r.map(|v| v as u64)).collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for ino in all {
        if effective_pin(c, ino)? == PinState::AlwaysKeep {
            out.push(ino);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

/// Totals for the status card: (files needing upload, bytes).
pub fn pending_upload_stats(c: &Connection) -> StoreResult<(u32, u64)> {
    let (n, b): (i64, i64) = c.query_row(
        "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM items
         WHERE deleted = 0 AND local_only = 0 AND is_dir = 0 AND (dirty = 1 OR item_id IS NULL)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((n as u32, b as u64))
}

/// Bytes of cached content (for the storage page).
pub fn cached_bytes(c: &Connection) -> StoreResult<u64> {
    let b: i64 = c.query_row(
        "SELECT COALESCE(SUM(size), 0) FROM items WHERE hydrated = 1 AND deleted = 0 AND is_dir = 0",
        [],
        |r| r.get(0),
    )?;
    Ok(b as u64)
}

// ------------------------------------------------------------------ kv

pub fn kv_get(c: &Connection, key: &str) -> StoreResult<Option<String>> {
    Ok(c.prepare_cached("SELECT value FROM kv WHERE key = ?1")?.query_row(params![key], |r| r.get(0)).optional()?)
}

pub fn kv_set(c: &Connection, key: &str, value: &str) -> StoreResult<()> {
    c.prepare_cached("INSERT INTO kv(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2")?
        .execute(params![key, value])?;
    Ok(())
}

pub fn kv_del(c: &Connection, key: &str) -> StoreResult<()> {
    c.prepare_cached("DELETE FROM kv WHERE key = ?1")?.execute(params![key])?;
    Ok(())
}

// ------------------------------------------------------------------ activity

#[derive(Debug, Clone)]
pub struct ActivityRow {
    pub id: i64,
    pub time: i64,
    pub kind: String,
    pub ino: Option<u64>,
    pub name: String,
    pub path: String,
    pub folder: String,
    pub size: u64,
    pub detail: String,
}

pub fn add_activity(
    c: &Connection,
    kind: &str,
    ino: Option<u64>,
    name: &str,
    path: &str,
    folder: &str,
    size: u64,
    detail: &str,
) -> StoreResult<i64> {
    let now = chrono::Utc::now().timestamp();
    c.prepare_cached(
        "INSERT INTO activity (time, kind, ino, name, path, folder, size, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?
    .execute(params![now, kind, ino.map(|v| v as i64), name, path, folder, size as i64, detail])?;
    let id = c.last_insert_rowid();
    // Keep the feed bounded.
    if id % 100 == 0 {
        c.execute("DELETE FROM activity WHERE id <= ?1", params![id - 1000])?;
    }
    Ok(id)
}

pub fn recent_activity(c: &Connection, limit: u32) -> StoreResult<Vec<ActivityRow>> {
    let mut st = c.prepare_cached(
        "SELECT id, time, kind, ino, name, path, folder, size, detail FROM activity ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = st.query_map(params![limit as i64], |r| {
        Ok(ActivityRow {
            id: r.get(0)?,
            time: r.get(1)?,
            kind: r.get(2)?,
            ino: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
            name: r.get(4)?,
            path: r.get(5)?,
            folder: r.get(6)?,
            size: r.get::<_, i64>(7)? as u64,
            detail: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(parent: u64, name: &str) -> NewItem {
        NewItem { parent_ino: parent, name: name.into(), mode: 0o644, ..Default::default() }
    }

    #[test]
    fn insert_lookup_and_case_folding() {
        let s = Store::open_in_memory().unwrap();
        let c = s.lock();
        assert!(get(&c, ROOT_INO).unwrap().unwrap().is_dir);
        let docs = insert(&c, &NewItem { is_dir: true, ..file(ROOT_INO, "Docs") }).unwrap();
        let a = insert(&c, &file(docs, "Report.docx")).unwrap();
        assert!(a > docs);
        assert!(matches!(insert(&c, &file(docs, "report.DOCX")), Err(StoreError::Exists)));
        assert_eq!(child(&c, docs, "Report.docx").unwrap().unwrap().ino, a);
        assert!(child(&c, docs, "report.docx").unwrap().is_none());
        assert_eq!(child_folded(&c, docs, "REPORT.docx").unwrap().unwrap().ino, a);
        assert_eq!(rel_path(&c, a).unwrap(), "Docs/Report.docx");
        assert_eq!(resolve(&c, "/Docs/Report.docx").unwrap().unwrap().ino, a);
        assert_eq!(children(&c, ROOT_INO).unwrap().len(), 1);

        // Tombstones free the name for a new live item.
        tombstone(&c, a, 1).unwrap();
        assert!(child(&c, docs, "Report.docx").unwrap().is_none());
        let b = insert(&c, &file(docs, "report.docx")).unwrap();
        assert_ne!(a, b);
        assert_eq!(pending_deletes(&c).unwrap().len(), 1);
    }

    #[test]
    fn pins_inherit() {
        let s = Store::open_in_memory().unwrap();
        let c = s.lock();
        let d = insert(&c, &NewItem { is_dir: true, ..file(ROOT_INO, "D") }).unwrap();
        let e = insert(&c, &NewItem { is_dir: true, ..file(d, "E") }).unwrap();
        let f = insert(&c, &file(e, "f")).unwrap();
        assert_eq!(effective_pin(&c, f).unwrap(), PinState::Inherit);
        set_pin(&c, d, PinState::AlwaysKeep).unwrap();
        assert_eq!(effective_pin(&c, f).unwrap(), PinState::AlwaysKeep);
        set_pin(&c, e, PinState::OnlineOnly).unwrap();
        assert_eq!(effective_pin(&c, f).unwrap(), PinState::OnlineOnly);
        assert_eq!(descendants(&c, d).unwrap(), vec![e, f]);
    }

    #[test]
    fn pending_work_detection() {
        let s = Store::open_in_memory().unwrap();
        let c = s.lock();
        set_remote(&c, ROOT_INO, &RemoteState { item_id: "ROOT".into(), ..Default::default() }).unwrap();
        let f = insert(&c, &file(ROOT_INO, "new.txt")).unwrap();
        // New item: needs create.
        assert_eq!(pending_push(&c, 0, 10).unwrap()[0].ino, f);
        // Synced item: nothing to do.
        set_remote(
            &c,
            f,
            &RemoteState {
                item_id: "X1".into(),
                parent_id: Some("ROOT".into()),
                name: Some("new.txt".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(pending_push(&c, 0, 10).unwrap().is_empty());
        // Renamed locally: needs move.
        set_name_parent(&c, f, ROOT_INO, "renamed.txt").unwrap();
        assert_eq!(pending_push(&c, 0, 10).unwrap().len(), 1);
        set_remote(
            &c,
            f,
            &RemoteState { item_id: "X1".into(), name: Some("renamed.txt".into()), ..Default::default() },
        )
        .unwrap();
        assert!(pending_push(&c, 0, 10).unwrap().is_empty());
        // Dirty: needs upload; errors back off.
        mark_dirty(&c, f).unwrap();
        assert_eq!(pending_push(&c, 0, 10).unwrap().len(), 1);
        set_error(&c, f, "x", "y", 1000, false).unwrap();
        assert!(pending_push(&c, 1000, 10).unwrap().is_empty());
        assert_eq!(pending_push(&c, 1000 + 25_000, 10).unwrap().len(), 1);
        assert_eq!(issue_count(&c).unwrap(), 1);
        // Local-only items are never pushed.
        let l = insert(&c, &NewItem { local_only: Some(LocalOnly::Ignored), ..file(ROOT_INO, ".~lock.a#") }).unwrap();
        assert!(pending_push(&c, i64::MAX / 2, 10).unwrap().iter().all(|i| i.ino != l));
    }

    #[test]
    fn backoff_schedule() {
        assert_eq!(backoff_ms(1), 25_000);
        assert_eq!(backoff_ms(2), 125_000);
        assert_eq!(backoff_ms(4), 3_125_000);
        assert_eq!(backoff_ms(20), 86_400_000);
    }

    #[test]
    fn backup_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("a.db")).unwrap();
        insert(&s.lock(), &file(ROOT_INO, "x")).unwrap();
        s.backup_to(&dir.path().join("a.db.bak")).unwrap();
        let b = Store::open(&dir.path().join("a.db.bak")).unwrap();
        assert!(child(&b.lock(), ROOT_INO, "x").unwrap().is_some());
    }

    #[test]
    fn activity_and_kv() {
        let s = Store::open_in_memory().unwrap();
        let c = s.lock();
        kv_set(&c, "delta", "a").unwrap();
        kv_set(&c, "delta", "b").unwrap();
        assert_eq!(kv_get(&c, "delta").unwrap().as_deref(), Some("b"));
        for i in 0..5 {
            add_activity(&c, "uploaded", None, &format!("f{i}"), "/p", "Docs", 1, "").unwrap();
        }
        let r = recent_activity(&c, 3).unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].name, "f4");
    }

    #[test]
    fn on_disk_reopen_and_corruption_detection() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.db");
        {
            let s = Store::open(&p).unwrap();
            insert(&s.lock(), &file(ROOT_INO, "keep")).unwrap();
        }
        let s = Store::open(&p).unwrap();
        assert!(child(&s.lock(), ROOT_INO, "keep").unwrap().is_some());
        drop(s);
        // Scribble over the file: open must fail cleanly (caller quarantines + rebuilds).
        std::fs::write(&p, vec![0x5au8; 8192]).unwrap();
        let _ = std::fs::remove_file(dir.path().join("state.db-wal"));
        assert!(Store::open(&p).is_err());
    }
}
