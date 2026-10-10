//! Remote → local: fetch delta pages, stage them, then apply the whole round
//! in tree order inside one transaction.
//!
//! Safety rules (research doc 04 §10.1, §12 P0):
//! * local removals come only from explicit `deleted` records, or from a
//!   complete, error-free full enumeration (mark & sweep after a resync);
//! * a sweep that would remove more than half of the known items is refused;
//! * dirty or never-uploaded local data is never removed: a remote delete of
//!   a modified file turns it into a new upload;
//! * content conflicts of modified files are resolved by the uploader's
//!   pre-check (keep both), never by overwriting local changes here.

use crate::engine::{Engine, EngineError, EngineEvent, EngineResult, Inval, NoticeKind, now_ms};
use crate::shortcuts::{self, Shortcut};
use crate::store::{self, Item, LocalOnly, NewItem, ROOT_INO, RemoteState};
use od_core::types::{ActivityKind, PinState};
use od_graph::{DriveItem, GraphError};
use rusqlite::{Connection, params};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Side effects collected while applying, executed after the commit.
#[derive(Default)]
struct ApplyCtx {
    invals: Vec<Inval>,
    /// Content to move to the content trash (clean copies of remote-deleted files).
    trash: Vec<(u64, String, String)>,
    /// Content to drop (stale clean copies that will be re-downloaded on demand).
    evict: Vec<u64>,
    changed: Vec<u64>,
    placeholders: Vec<u64>,
    remote_deletes: u32,
    kept_changes: Vec<String>,
    applied: u32,
    /// Shortcuts to shared folders (loaded at the start of a round).
    shortcuts: Vec<Shortcut>,
    shortcuts_seen: HashSet<String>,
    shortcuts_dirty: bool,
}

enum Applied {
    Done,
    Deferred,
}

const PLACEHOLDER_PREFIX: &str = ".lod-swap-";

/// Stored instead of a delta link (and as the listing mode) when the state
/// was built by listing folders.
const LISTING_LINK: &str = "walk";

/// The server doesn't offer delta at this place (as opposed to the folder
/// being gone or forbidden, or a passing failure).
fn delta_unsupported(e: &GraphError) -> bool {
    matches!(e.status(), Some(400 | 405 | 501)) || e.has_code("notSupported")
}

impl Engine {
    pub(crate) async fn pull_loop(self: Arc<Self>) {
        let mut failures: u32 = 0;
        loop {
            // With real-time notifications, polling is only a safety net; a
            // full listing is expensive and runs on its own, longer interval.
            let base = if self.uses_listing() {
                self.cfg().listing_interval.max(self.cfg().poll_interval)
            } else if self.push_connected() {
                self.cfg().poll_interval.max(Duration::from_secs(300))
            } else {
                self.cfg().poll_interval
            };
            let wait = if failures > 0 {
                (Duration::from_secs(15) * failures.min(20)).min(base.max(Duration::from_secs(15)))
            } else {
                base
            };
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.pull_wake.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
            if self.is_paused() || self.is_auth_required() {
                continue;
            }
            match self.pull_once().await {
                Ok(_) => {
                    failures = 0;
                    self.set_offline(false);
                }
                Err(e) => {
                    failures += 1;
                    self.handle_account_error(&e, "checking for changes");
                }
            }
        }
    }

    /// Classify an error as account-wide state (offline, auth) or log it.
    pub(crate) fn handle_account_error(&self, e: &EngineError, what: &str) {
        match e {
            EngineError::Graph(g) if g.is_offline() => self.set_offline(true),
            EngineError::Graph(GraphError::AuthRequired(m)) => {
                tracing::warn!("sign-in required: {m}");
                self.set_auth_required(true);
            }
            other => tracing::warn!("error while {what}: {other}"),
        }
    }

    /// Fetch and apply one delta round. Returns whether anything changed.
    pub async fn pull_once(self: &Arc<Self>) -> EngineResult<bool> {
        self.set_pulling(true);
        let r = self.pull_inner().await;
        self.set_pulling(false);
        r
    }

    async fn pull_inner(self: &Arc<Self>) -> EngineResult<bool> {
        self.refresh_quota_if_due().await;
        if self.uses_listing() {
            return self.pull_walk().await;
        }
        let (mut link, mut full) = {
            let c = self.store.lock();
            let force_full = store::kv_get(&c, "force_full")?.is_some();
            // "walk" marks a state built by listing: delta starts from scratch.
            let delta_link = store::kv_get(&c, "delta_link")?.filter(|l| l != LISTING_LINK);
            match (store::kv_get(&c, "delta_next")?, delta_link) {
                (Some(n), _) => (Some(n), store::kv_get(&c, "delta_full")?.as_deref() == Some("1")),
                // Safety-net reconcile: enumerate everything; the old link stays
                // valid until the full round has been applied.
                (None, Some(_)) if force_full => {
                    store::kv_del(&c, "force_full")?;
                    c.execute("DELETE FROM delta_stage", []).map_err(store::StoreError::from)?;
                    (None, true)
                }
                (None, Some(d)) => (Some(d), false),
                (None, None) => (None, true),
            }
        };
        loop {
            let page = match self.graph.delta_page(link.as_deref()).await {
                Ok(p) => p,
                Err(GraphError::ResyncRequired { code, restart_link }) => {
                    tracing::warn!("delta token rejected ({code}); starting a full resync");
                    let c = self.store.lock();
                    c.execute("DELETE FROM delta_stage", [])?;
                    store::kv_del(&c, "delta_link")?;
                    match &restart_link {
                        Some(l) => store::kv_set(&c, "delta_next", l)?,
                        None => store::kv_del(&c, "delta_next")?,
                    }
                    store::kv_set(&c, "delta_full", "1")?;
                    store::log_decision(&c, "resync", None, None, "", &code)?;
                    link = restart_link;
                    full = true;
                    continue;
                }
                // Some services only offer delta on a drive's root (older
                // SharePoint, national clouds): list the shared folder instead.
                Err(e) if link.is_none() && self.graph.scoped_root().is_some() && delta_unsupported(&e) => {
                    tracing::info!("change tracking isn't offered for this shared folder ({e}); listing it instead");
                    store::kv_set(&self.store.lock(), "listing_mode", LISTING_LINK)?;
                    return self.pull_walk().await;
                }
                Err(e) => return Err(e.into()),
            };
            {
                let mut conn = self.store.lock();
                let tx = conn.transaction().map_err(store::StoreError::from)?;
                let mut seq: i64 = store::kv_get(&tx, "stage_seq")?.and_then(|v| v.parse().ok()).unwrap_or(0);
                for item in &page.value {
                    if item.id.is_empty() {
                        continue;
                    }
                    seq += 1;
                    tx.execute(
                        "INSERT INTO delta_stage(item_id, seq, json) VALUES (?1, ?2, ?3)
                         ON CONFLICT(item_id) DO UPDATE SET seq = ?2, json = ?3",
                        params![item.id, seq, serde_json::to_string(item).unwrap_or_default()],
                    )
                    .map_err(store::StoreError::from)?;
                }
                store::kv_set(&tx, "stage_seq", &seq.to_string())?;
                if let Some(next) = &page.next_link {
                    // Persist progress so a long first enumeration survives a restart.
                    store::kv_set(&tx, "delta_next", next)?;
                    store::kv_set(&tx, "delta_full", if full { "1" } else { "0" })?;
                }
                tx.commit().map_err(store::StoreError::from)?;
            }
            match (page.next_link, page.delta_link) {
                (Some(next), _) => link = Some(next),
                (None, Some(delta)) => {
                    let me = self.clone();
                    let changed = tokio::task::spawn_blocking(move || me.apply_staged(full, &delta))
                        .await
                        .map_err(|e| EngineError::Other(e.to_string()))??;
                    return Ok(changed);
                }
                (None, None) => return Err(EngineError::Other("delta page without continuation".into())),
            }
        }
    }

    /// One round without delta: list every folder, then apply the listing
    /// as a full round (items missing from it are swept, with the same
    /// anomaly brake as a resync). Nothing is applied unless the whole
    /// listing succeeded.
    async fn pull_walk(self: &Arc<Self>) -> EngineResult<bool> {
        {
            // One request per folder: don't repeat it for every wake-up.
            let c = self.store.lock();
            let forced =
                store::kv_get(&c, "force_full")?.is_some() | self.refresh_requested.swap(false, Ordering::SeqCst);
            let last: i64 = store::kv_get(&c, "listed_at")?.and_then(|v| v.parse().ok()).unwrap_or(0);
            let every = self.cfg().listing_interval.as_millis() as i64;
            if !forced && last > 0 && now_ms() - last < every {
                return Ok(false);
            }
            store::kv_del(&c, "force_full")?;
        }
        let items = self.graph.walk().await?;
        {
            let mut conn = self.store.lock();
            let tx = conn.transaction().map_err(store::StoreError::from)?;
            tx.execute("DELETE FROM delta_stage", []).map_err(store::StoreError::from)?;
            for (seq, item) in items.iter().enumerate() {
                tx.execute(
                    "INSERT INTO delta_stage(item_id, seq, json) VALUES (?1, ?2, ?3)
                     ON CONFLICT(item_id) DO UPDATE SET seq = ?2, json = ?3",
                    params![item.id, seq as i64 + 1, serde_json::to_string(item).unwrap_or_default()],
                )
                .map_err(store::StoreError::from)?;
            }
            tx.commit().map_err(store::StoreError::from)?;
        }
        let me = self.clone();
        let changed = tokio::task::spawn_blocking(move || me.apply_staged(true, LISTING_LINK))
            .await
            .map_err(|e| EngineError::Other(e.to_string()))??;
        store::kv_set(&self.store.lock(), "listed_at", &now_ms().to_string())?;
        Ok(changed)
    }

    /// Remote changes come from full listings instead of delta: forced by
    /// configuration, or because the server refused delta on this folder.
    pub(crate) fn uses_listing(&self) -> bool {
        self.cfg().walk_listing
            || store::kv_get(&self.store.lock(), "listing_mode").ok().flatten().as_deref() == Some(LISTING_LINK)
    }

    async fn refresh_quota_if_due(&self) {
        if !self.cfg().track_quota {
            return;
        }
        let due = {
            let c = self.store.lock();
            let last: i64 = store::kv_get(&c, "quota_at").ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0);
            now_ms() - last > 300_000
        };
        if !due {
            return;
        }
        if let Ok(d) = self.graph.drive().await {
            let c = self.store.lock();
            let _ = store::kv_set(&c, "quota_at", &now_ms().to_string());
            if let Some(q) = &d.quota {
                let _ = store::kv_set(
                    &c,
                    "quota",
                    &serde_json::json!({"total": q.total, "used": q.used, "state": q.state}).to_string(),
                );
            }
            drop(c);
            self.set_quota(d.quota);
        }
    }

    /// Apply every staged item (parents before children), then persist the
    /// delta link, all in one transaction.
    fn apply_staged(self: &Arc<Self>, full: bool, delta_link: &str) -> EngineResult<bool> {
        let mut ctx = ApplyCtx::default();
        {
            let mut conn = self.store.lock();
            let tx = conn.transaction().map_err(store::StoreError::from)?;
            ctx.shortcuts = shortcuts::load(&tx);
            let staged: Vec<(String, String)> = {
                let mut st = tx
                    .prepare("SELECT item_id, json FROM delta_stage ORDER BY seq")
                    .map_err(store::StoreError::from)?;
                st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map_err(store::StoreError::from)?
                    .collect::<Result<_, _>>()
                    .map_err(store::StoreError::from)?
            };
            let mut deferred: Vec<DriveItem> = Vec::new();
            for (_, json) in &staged {
                let Ok(item) = serde_json::from_str::<DriveItem>(json) else { continue };
                if let Applied::Deferred = self.apply_one(&tx, &item, &mut ctx)? {
                    deferred.push(item);
                }
            }
            // Retry items whose parent arrived later in the round.
            loop {
                let before = deferred.len();
                let mut still = Vec::new();
                for item in deferred {
                    if let Applied::Deferred = self.apply_one(&tx, &item, &mut ctx)? {
                        still.push(item);
                    }
                }
                deferred = still;
                if deferred.is_empty() || deferred.len() == before {
                    break;
                }
            }
            for item in &deferred {
                tracing::warn!(id = %item.id, name = ?item.name, "skipping item whose parent is unknown");
            }
            self.resolve_placeholders(&tx, &mut ctx)?;
            if full {
                self.sweep(&tx, &mut ctx)?;
                // Shortcuts missing from a complete listing are gone.
                let before = ctx.shortcuts.len();
                let seen = std::mem::take(&mut ctx.shortcuts_seen);
                ctx.shortcuts.retain(|s| seen.contains(&s.id));
                ctx.shortcuts_dirty |= ctx.shortcuts.len() != before;
            }
            if ctx.shortcuts_dirty {
                shortcuts::save(&tx, &ctx.shortcuts)?;
            }
            tx.execute("DELETE FROM delta_stage", []).map_err(store::StoreError::from)?;
            store::kv_set(&tx, "delta_link", delta_link)?;
            store::kv_del(&tx, "delta_next")?;
            store::kv_del(&tx, "delta_full")?;
            store::kv_set(&tx, "stage_seq", "0")?;
            tx.commit().map_err(store::StoreError::from)?;
        }

        // ---- after commit: content side effects, kernel caches, events
        for (ino, name, rel) in &ctx.trash {
            if let Err(e) = self.content.trash(*ino, name, rel) {
                tracing::warn!("could not move content of {name} to trash: {e}");
            }
        }
        for ino in &ctx.evict {
            let _ = self.content.remove(*ino);
        }
        let any = ctx.applied > 0 || !ctx.invals.is_empty();
        self.invalidate(std::mem::take(&mut ctx.invals));
        if ctx.shortcuts_dirty {
            self.rebuild_links();
            self.emit(EngineEvent::ShortcutsChanged);
        }
        self.items_changed(&ctx.changed);
        for name in &ctx.kept_changes {
            self.activity(
                ActivityKind::Conflict,
                None,
                name,
                0,
                "Deleted online; your changes were kept and will be uploaded again",
            );
        }
        let threshold = self.cfg().mass_delete_threshold.max(1);
        if ctx.remote_deletes >= threshold {
            self.notice(
                NoticeKind::RemoteMassDelete,
                "Files were deleted from your OneDrive",
                &format!(
                    "{} items were deleted online and removed from this device. They're in the OneDrive recycle bin, and copies of downloaded files are kept for 7 days.",
                    ctx.remote_deletes
                ),
                None,
            );
        }
        if any {
            self.push_wake.notify_one();
            self.hydrate_wake.notify_one();
            self.emit(EngineEvent::StateChanged);
        }
        Ok(any)
    }

    fn apply_one(&self, c: &Connection, it: &DriveItem, ctx: &mut ApplyCtx) -> EngineResult<Applied> {
        // A shortcut's engine syncs a folder of another drive: that folder is its root.
        if it.is_root() || self.graph.scoped_root() == Some(it.id.as_str()) {
            return self.apply_root(c, it).map(|_| Applied::Done);
        }
        if it.is_deleted() && ctx.shortcuts.iter().any(|s| s.id == it.id) {
            ctx.shortcuts.retain(|s| s.id != it.id);
            ctx.shortcuts_dirty = true;
            return Ok(Applied::Done);
        }
        // Shortcuts to shared folders: content is synced by their own engine.
        if it.remote_item.is_some() && !it.is_deleted() {
            if let Some(s) = Shortcut::from_item(it) {
                ctx.shortcuts_seen.insert(s.id.clone());
                match ctx.shortcuts.iter_mut().find(|x| x.id == s.id) {
                    Some(x) if *x == s => {}
                    Some(x) => {
                        *x = s;
                        ctx.shortcuts_dirty = true;
                    }
                    None => {
                        ctx.shortcuts.push(s);
                        ctx.shortcuts_dirty = true;
                    }
                }
            }
            return Ok(Applied::Done);
        }
        // OneNote notebooks (packages) are not regular files; malware is never synced.
        if it.is_package() || it.malware.is_some() {
            return Ok(Applied::Done);
        }
        let existing = store::by_item_id(c, &it.id)?;
        if it.is_deleted() {
            if let Some(row) = existing {
                self.remote_deleted(c, &row, ctx, "deleted online")?;
            }
            return Ok(Applied::Done);
        }
        let Some(pid) = it.parent_id() else { return Ok(Applied::Done) };
        let Some(parent) = store::by_item_id(c, pid)? else { return Ok(Applied::Deferred) };
        let name = it.name.clone().unwrap_or_default();
        if name.is_empty() || name.contains('/') || name.contains('\0') {
            return Ok(Applied::Done);
        }
        ctx.applied += 1;
        let remote = RemoteState::from_item(it);
        match existing {
            Some(row) => self.remote_updated(c, row, parent, &name, it, remote, ctx)?,
            None => self.remote_created(c, parent, &name, it, remote, ctx)?,
        }
        Ok(Applied::Done)
    }

    fn apply_root(&self, c: &Connection, it: &DriveItem) -> EngineResult<()> {
        match store::kv_get(c, "root_item_id")? {
            Some(known) if known != it.id => {
                let msg = "Your OneDrive's root folder changed on the server. Syncing is stopped to protect your files; unlink and sign in again.".to_string();
                self.set_fatal(msg.clone());
                return Err(EngineError::Other(msg));
            }
            Some(_) => {}
            None => store::kv_set(c, "root_item_id", &it.id)?,
        }
        let mut r = RemoteState::from_item(it);
        r.name = Some(String::new());
        r.parent_id = None;
        store::set_remote(c, ROOT_INO, &r)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn remote_created(
        &self,
        c: &Connection,
        parent: Item,
        name: &str,
        it: &DriveItem,
        remote: RemoteState,
        ctx: &mut ApplyCtx,
    ) -> EngineResult<()> {
        let parent = self.resurrect_if_tombstoned(c, parent, ctx)?;
        if let Some(local) = store::child_folded(c, parent.ino, name)? {
            if local.item_id.is_none() && local.local_only == LocalOnly::No {
                // Created on both sides before either synced.
                if local.is_dir && it.is_folder() {
                    store::set_remote(c, local.ino, &remote)?;
                    store::log_decision(
                        c,
                        "merge-folder",
                        Some(local.ino),
                        Some(&it.id),
                        name,
                        "same folder created on both sides",
                    )?;
                    return Ok(());
                }
                if !local.is_dir && it.is_file() && !self.is_busy(local.ino) {
                    let same = match (&remote.hash, od_core::quickxor::hash_file(&self.content.path(local.ino))) {
                        (Some(r), Ok(h)) => *r == h,
                        _ => false,
                    };
                    if same {
                        store::set_remote(c, local.ino, &remote)?;
                        store::mark_clean(c, local.ino, remote.hash.as_deref())?;
                        return Ok(());
                    }
                }
                // Different content: the local copy gets a conflict name and is uploaded under it.
                let new_name = crate::engine::unique_name(c, parent.ino, name, &self.cfg().hostname)?;
                store::set_name_parent(c, local.ino, parent.ino, &new_name)?;
                store::log_decision(
                    c,
                    "conflict-rename",
                    Some(local.ino),
                    None,
                    &new_name,
                    "a different item with this name was added online",
                )?;
                ctx.invals.push(Inval::Entry { parent: parent.ino, name: local.name.clone() });
                ctx.invals.push(Inval::Entry { parent: parent.ino, name: new_name });
            } else if local.item_id.as_deref() != Some(it.id.as_str()) {
                // Another synced item still holds the name (e.g. a swap of two names);
                // its own update in this round gives it its final name.
                let tmp = format!("{PLACEHOLDER_PREFIX}{}", local.ino);
                store::set_name_parent(c, local.ino, parent.ino, &tmp)?;
                ctx.placeholders.push(local.ino);
                ctx.invals.push(Inval::Entry { parent: parent.ino, name: local.name.clone() });
            }
        }
        let excluded = parent.ino == ROOT_INO
            && it.is_folder()
            && self.cfg().excluded_folders.iter().any(|f| f.eq_ignore_ascii_case(name));
        let ino = store::insert(
            c,
            &NewItem {
                item_id: Some(it.id.clone()),
                parent_ino: parent.ino,
                name: name.to_string(),
                is_dir: it.is_folder(),
                size: remote.size.unwrap_or(0),
                mtime_ns: remote.mtime_ns.unwrap_or(0),
                ctime_ns: it.created().and_then(|t| t.timestamp_nanos_opt()).unwrap_or(0),
                mode: if it.is_folder() { 0o755 } else { 0o644 },
                hydrated: false,
                dirty: false,
                local_only: Some(if excluded { LocalOnly::Excluded } else { parent_local_only(&parent) }),
            },
        )?;
        store::set_remote(c, ino, &remote)?;
        ctx.invals.push(Inval::Entry { parent: parent.ino, name: name.to_string() });
        ctx.changed.push(ino);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn remote_updated(
        &self,
        c: &Connection,
        row: Item,
        parent: Item,
        name: &str,
        it: &DriveItem,
        remote: RemoteState,
        ctx: &mut ApplyCtx,
    ) -> EngineResult<()> {
        let content_changed = !row.is_dir && content_changed(&row, &remote);
        if row.deleted {
            if content_changed {
                // Changed online after we deleted it locally: the newer version wins
                // over the (not yet propagated) delete.
                let parent = self.resurrect_if_tombstoned(c, parent, ctx)?;
                let free = crate::engine::unique_name(c, parent.ino, name, &self.cfg().hostname)?;
                store::untombstone(c, row.ino)?;
                store::set_name_parent(c, row.ino, parent.ino, &free)?;
                store::set_remote(c, row.ino, &remote)?;
                store::set_size_mtime(c, row.ino, remote.size.unwrap_or(0), remote.mtime_ns.unwrap_or(0))?;
                store::set_hydrated(c, row.ino, false, None)?;
                store::log_decision(
                    c,
                    "restore",
                    Some(row.ino),
                    Some(&it.id),
                    &free,
                    "changed online after a local delete",
                )?;
                ctx.invals.push(Inval::Entry { parent: parent.ino, name: free });
                ctx.changed.push(row.ino);
            } else {
                // Keep the tombstone; refresh the eTag so the delete's If-Match works.
                store::set_remote(c, row.ino, &remote)?;
            }
            return Ok(());
        }

        // ---- moves and renames
        let local_parent_item_id = row.parent_ino.and_then(|p| store::get(c, p).ok().flatten()).and_then(|p| p.item_id);
        let local_moved = row.needs_move(local_parent_item_id.as_deref());
        let at_destination = row.parent_ino == Some(parent.ino) && row.name == name;
        if !local_moved && !at_destination {
            let parent = self.resurrect_if_tombstoned(c, parent, ctx)?;
            if let Some(other) = store::child_folded(c, parent.ino, name)?
                && other.ino != row.ino
            {
                if other.item_id.is_none() {
                    let new_name = crate::engine::unique_name(c, parent.ino, name, &self.cfg().hostname)?;
                    store::set_name_parent(c, other.ino, parent.ino, &new_name)?;
                    ctx.invals.push(Inval::Entry { parent: parent.ino, name: new_name });
                } else {
                    let tmp = format!("{PLACEHOLDER_PREFIX}{}", other.ino);
                    store::set_name_parent(c, other.ino, parent.ino, &tmp)?;
                    ctx.placeholders.push(other.ino);
                }
                ctx.invals.push(Inval::Entry { parent: parent.ino, name: other.name.clone() });
            }
            let (old_parent, old_name) = (row.parent_ino.unwrap_or(ROOT_INO), row.name.clone());
            store::set_name_parent(c, row.ino, parent.ino, name)?;
            ctx.invals.push(Inval::Entry { parent: old_parent, name: old_name });
            ctx.invals.push(Inval::Entry { parent: parent.ino, name: name.to_string() });
            ctx.changed.push(row.ino);
            // Moving into or out of a hidden/ignored area changes visibility.
            let lo = parent_local_only(&parent);
            if lo != row.local_only && row.local_only != LocalOnly::Invalid {
                store::set_local_only(c, row.ino, lo)?;
            }
        }

        // ---- content
        let busy = self.is_busy(row.ino);
        if content_changed && (row.dirty || busy) {
            // Both sides changed. Record only the new name/eTag; the uploader's
            // pre-check sees the remote change and keeps both versions.
            let mut meta_only = remote.clone();
            meta_only.ctag = row.ctag.clone();
            meta_only.hash = row.remote_hash.clone();
            meta_only.size = row.remote_size;
            meta_only.mtime_ns = row.remote_mtime_ns;
            store::set_remote(c, row.ino, &meta_only)?;
            return Ok(());
        }
        store::set_remote(c, row.ino, &remote)?;
        if row.error_code.is_some() && row.etag != remote.etag {
            store::clear_error(c, row.ino)?;
        }
        if row.is_dir {
            return Ok(());
        }
        if content_changed && row.hydrated {
            let pinned = store::effective_pin(c, row.ino)? == PinState::AlwaysKeep;
            let recent = chrono::Utc::now().timestamp() - row.last_access < 7 * 86_400;
            if pinned || recent || self.is_open(row.ino) {
                // Stay hydrated with the old content until the new version is
                // downloaded (is_stale() → the hydrator refreshes it).
            } else {
                store::set_hydrated(c, row.ino, false, None)?;
                store::log_decision(
                    c,
                    "evict-stale",
                    Some(row.ino),
                    row.item_id.as_deref(),
                    &row.name,
                    "changed online; not used recently",
                )?;
                ctx.evict.push(row.ino);
            }
        }
        let size = remote.size.unwrap_or(row.size);
        let mtime = remote.mtime_ns.unwrap_or(row.mtime_ns);
        if !row.dirty && (size != row.size || mtime != row.mtime_ns || content_changed) {
            // While a stale copy is still served, keep its real size until refreshed.
            if !(content_changed && row.hydrated && !ctx.evict.contains(&row.ino)) {
                store::set_size_mtime(c, row.ino, size, mtime)?;
            }
            ctx.invals.push(Inval::Inode { ino: row.ino });
            ctx.changed.push(row.ino);
        }
        Ok(())
    }

    /// The item was deleted online (or vanished in a full resync).
    fn remote_deleted(&self, c: &Connection, row: &Item, ctx: &mut ApplyCtx, why: &str) -> EngineResult<()> {
        if row.ino == ROOT_INO {
            return Ok(());
        }
        let path = store::rel_path(c, row.ino).unwrap_or_else(|_| row.name.clone());
        if row.deleted {
            // Deleted on both sides.
            for d in store::descendants(c, row.ino)? {
                store::remove_row(c, d)?;
            }
            store::remove_row(c, row.ino)?;
            return Ok(());
        }
        ctx.remote_deletes += 1;
        if row.is_dir {
            // Keep every descendant holding unsynced data, and the folders above it.
            let desc = store::descendants(c, row.ino)?;
            let mut keep: HashSet<u64> = HashSet::new();
            for d in &desc {
                let Some(it) = store::get(c, *d)? else { continue };
                if !it.deleted
                    && (it.dirty || self.is_busy(it.ino) || (it.item_id.is_none() && it.local_only == LocalOnly::No))
                {
                    let mut cur = Some(it.ino);
                    while let Some(x) = cur {
                        if !keep.insert(x) || x == row.ino {
                            break;
                        }
                        cur = store::get(c, x)?.and_then(|i| i.parent_ino);
                    }
                }
            }
            for d in desc.iter().rev() {
                let Some(it) = store::get(c, *d)? else { continue };
                if keep.contains(d) {
                    store::clear_remote(c, *d)?;
                } else {
                    if it.hydrated && !it.is_dir {
                        let rel = store::rel_path(c, *d).unwrap_or_else(|_| it.name.clone());
                        ctx.trash.push((*d, it.name.clone(), rel));
                    }
                    store::remove_row(c, *d)?;
                }
            }
            if keep.contains(&row.ino) {
                store::clear_remote(c, row.ino)?;
                store::log_decision(
                    c,
                    "keep-folder",
                    Some(row.ino),
                    row.item_id.as_deref(),
                    &path,
                    "deleted online but contains unsynced files; re-uploading",
                )?;
                ctx.kept_changes.push(path);
                ctx.changed.push(row.ino);
                return Ok(());
            }
        } else if row.dirty || self.is_busy(row.ino) {
            store::clear_remote(c, row.ino)?;
            store::log_decision(
                c,
                "keep-file",
                Some(row.ino),
                row.item_id.as_deref(),
                &path,
                "deleted online but modified locally; re-uploading",
            )?;
            ctx.kept_changes.push(path);
            ctx.changed.push(row.ino);
            return Ok(());
        } else if row.hydrated {
            ctx.trash.push((row.ino, row.name.clone(), path.clone()));
        }
        store::remove_row(c, row.ino)?;
        store::log_decision(c, "remove-local", Some(row.ino), row.item_id.as_deref(), &path, why)?;
        let parent = row.parent_ino.unwrap_or(ROOT_INO);
        ctx.invals.push(Inval::Delete { parent, child: row.ino, name: row.name.clone() });
        Ok(())
    }

    /// Something changed online inside a folder we deleted locally: the folder
    /// (and its ancestors) come back rather than deleting new remote content.
    fn resurrect_if_tombstoned(&self, c: &Connection, parent: Item, ctx: &mut ApplyCtx) -> EngineResult<Item> {
        if !parent.deleted {
            return Ok(parent);
        }
        let mut chain = vec![parent.clone()];
        let mut cur = parent.parent_ino;
        while let Some(p) = cur {
            let Some(it) = store::get(c, p)? else { break };
            if !it.deleted {
                break;
            }
            cur = it.parent_ino;
            chain.push(it);
        }
        for it in chain.iter().rev() {
            let host = self.cfg().hostname;
            let pino = it.parent_ino.unwrap_or(ROOT_INO);
            let free = crate::engine::unique_name(c, pino, &it.name, &host)?;
            store::untombstone(c, it.ino)?;
            if free != it.name {
                store::set_name_parent(c, it.ino, pino, &free)?;
            }
            store::log_decision(
                c,
                "restore-folder",
                Some(it.ino),
                it.item_id.as_deref(),
                &free,
                "new content online inside a locally deleted folder",
            )?;
            ctx.invals.push(Inval::Entry { parent: pino, name: free });
        }
        Ok(store::get(c, parent.ino)?.unwrap_or(parent))
    }

    fn resolve_placeholders(&self, c: &Connection, ctx: &mut ApplyCtx) -> EngineResult<()> {
        for ino in std::mem::take(&mut ctx.placeholders) {
            let Some(it) = store::get(c, ino)? else { continue };
            if !it.name.starts_with(PLACEHOLDER_PREFIX) {
                continue;
            }
            let parent = it.parent_ino.unwrap_or(ROOT_INO);
            let wanted = it.remote_name.clone().unwrap_or_else(|| format!("item-{ino}"));
            let free = crate::engine::unique_name(c, parent, &wanted, &self.cfg().hostname)?;
            store::set_name_parent(c, ino, parent, &free)?;
            store::log_decision(
                c,
                "placeholder-fixup",
                Some(ino),
                it.item_id.as_deref(),
                &free,
                "name clash during remote rename",
            )?;
            ctx.invals.push(Inval::Entry { parent, name: free });
        }
        Ok(())
    }

    /// After a complete enumeration: items we know that the server no longer
    /// listed were deleted online.
    fn sweep(&self, c: &Connection, ctx: &mut ApplyCtx) -> EngineResult<()> {
        let staged: i64 =
            c.query_row("SELECT COUNT(*) FROM delta_stage", [], |r| r.get(0)).map_err(store::StoreError::from)?;
        let missing: Vec<u64> = {
            let mut st = c
                .prepare(
                    "SELECT ino FROM items WHERE item_id IS NOT NULL AND ino <> 1
                     AND item_id NOT IN (SELECT item_id FROM delta_stage)",
                )
                .map_err(store::StoreError::from)?;
            st.query_map([], |r| r.get::<_, i64>(0))
                .map_err(store::StoreError::from)?
                .filter_map(|r| r.ok())
                .map(|v| v as u64)
                .collect()
        };
        if missing.is_empty() {
            return Ok(());
        }
        let known: i64 = c
            .query_row("SELECT COUNT(*) FROM items WHERE item_id IS NOT NULL AND ino <> 1", [], |r| r.get(0))
            .map_err(store::StoreError::from)?;
        // Anomaly brake: an enumeration that "lost" most of the drive is more
        // likely a server/API problem than real deletes.
        if staged == 0 || (missing.len() as i64 * 2 > known && known >= 20) {
            store::log_decision(
                c,
                "sweep-suppressed",
                None,
                None,
                "",
                &format!("{} of {known} known items missing from a full listing of {staged}", missing.len()),
            )?;
            self.notice(
                NoticeKind::SyncError,
                "OneDrive returned an incomplete file list",
                "Nothing was removed from this device. Syncing will check again later.",
                None,
            );
            return Ok(());
        }
        let missing_set: HashSet<u64> = missing.iter().copied().collect();
        // Only process the top-most missing items; descendants go with them.
        let tops: Vec<u64> = missing
            .iter()
            .copied()
            .filter(|ino| {
                store::get(c, *ino).ok().flatten().and_then(|i| i.parent_ino).is_none_or(|p| !missing_set.contains(&p))
            })
            .collect();
        for ino in tops {
            if let Some(row) = store::get(c, ino)? {
                self.remote_deleted(c, &row, ctx, "missing from a full resync listing")?;
            }
        }
        Ok(())
    }
}

fn parent_local_only(parent: &Item) -> LocalOnly {
    match parent.local_only {
        LocalOnly::No => LocalOnly::No,
        LocalOnly::Excluded => LocalOnly::Excluded,
        LocalOnly::Ignored | LocalOnly::Invalid => LocalOnly::Ignored,
    }
}

/// Did the file's content change on the server since our last sync?
/// Hashes win when both sides have one; otherwise cTag; otherwise size.
pub(crate) fn content_changed(row: &Item, remote: &RemoteState) -> bool {
    if let (Some(a), Some(b)) = (&row.remote_hash, &remote.hash) {
        return a != b;
    }
    if let (Some(a), Some(b)) = (&row.ctag, &remote.ctag) {
        return a != b;
    }
    row.remote_size != remote.size
}
