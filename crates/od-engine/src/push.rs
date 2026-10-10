//! Local → remote reconciler, background hydration and maintenance.
//!
//! The reconciler derives work from the store (state-based): tombstones →
//! remote deletes; rows without a server id → creates; rows whose name or
//! parent differ from the last-synced server state → moves; dirty rows →
//! uploads. Remote deletes only ever come from tombstones, i.e. from a user
//! deleting something through the mount.

use crate::engine::{Engine, EngineError, EngineEvent, EngineResult, Inval, now_ms};
use crate::store::{self, Item, ROOT_INO, RemoteState};
use od_core::types::{ActivityKind, PinState};
use od_graph::{ConflictBehavior, GraphError, ItemPatch};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::task::JoinSet;

impl Engine {
    /// Start the background loops. They stop when the engine is cancelled.
    pub fn start(self: &Arc<Self>) {
        let pin_root = self.cfg().download_all;
        {
            let c = self.store.lock();
            let current = store::get(&c, ROOT_INO).ok().flatten().map(|r| r.pin);
            if pin_root && current != Some(PinState::AlwaysKeep) {
                let _ = store::set_pin(&c, ROOT_INO, PinState::AlwaysKeep);
            } else if !pin_root && current == Some(PinState::AlwaysKeep) {
                let _ = store::set_pin(&c, ROOT_INO, PinState::Inherit);
            }
        }
        tokio::spawn(self.clone().pull_loop());
        tokio::spawn(self.clone().push_loop());
        tokio::spawn(self.clone().hydrate_loop());
        tokio::spawn(self.clone().maintenance_loop());
        tokio::spawn(self.clone().thumbnail_loop());
        tokio::spawn(self.clone().push_channel_loop());
        self.pull_wake.notify_one();
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }

    async fn push_loop(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.push_wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
            if self.is_paused() || self.is_auth_required() || self.fatal_error().is_some() {
                continue;
            }
            // Let a burst of local changes settle.
            tokio::time::sleep(Duration::from_millis(300)).await;
            if let Err(e) = self.push_pass().await {
                self.handle_account_error(&e, "uploading changes");
            }
        }
    }

    /// One reconciliation pass; repeats while progress is made.
    pub async fn push_pass(self: &Arc<Self>) -> EngineResult<()> {
        for _round in 0..50 {
            let mut progress = self.push_deletes().await?;
            let work = store::pending_push(&self.store.lock(), now_ms(), 500)?;
            if work.is_empty() && !progress {
                break;
            }
            let mut uploads = Vec::new();
            for item in work {
                let parent = item.parent_ino.and_then(|p| store::get(&self.store.lock(), p).ok().flatten());
                let parent_id = parent.as_ref().and_then(|p| p.item_id.clone());
                if parent_id.is_none() || parent.as_ref().is_some_and(|p| p.deleted) {
                    continue; // parent not created online yet (or being deleted)
                }
                let r = if item.item_id.is_none() && item.is_dir {
                    self.push_create_folder(&item, parent_id.as_deref().unwrap()).await
                } else if item.item_id.is_some() && item.needs_move(parent_id.as_deref()) {
                    self.push_move(&item, parent_id.as_deref().unwrap()).await
                } else if !item.is_dir && (item.dirty || item.item_id.is_none()) {
                    if !self.is_busy(item.ino) {
                        uploads.push(item.ino);
                    }
                    continue;
                } else {
                    Ok(false)
                };
                match r {
                    Ok(p) => progress |= p,
                    Err(e) => self.record_item_error(item.ino, &e)?,
                }
            }
            if !uploads.is_empty() {
                progress |= self.run_uploads(uploads).await?;
            }
            if !progress {
                break;
            }
        }
        self.emit(EngineEvent::StateChanged);
        Ok(())
    }

    /// Account-wide problems stop the pass; per-item problems back off.
    fn record_item_error(&self, ino: u64, e: &EngineError) -> EngineResult<()> {
        if let EngineError::Graph(g) = e {
            if g.is_offline() || matches!(g, GraphError::AuthRequired(_)) {
                self.handle_account_error(e, "syncing");
                return Err(EngineError::Other("account unavailable".into()));
            }
            if g.status() == Some(429) || g.status() == Some(503) {
                // Throttling is account-wide, not the item's fault.
                return Ok(());
            }
        }
        let (code, msg) = match e {
            EngineError::Graph(g) => (g.short_code(), g.user_message()),
            other => ("error".to_string(), other.to_string()),
        };
        let c = self.store.lock();
        store::set_error(&c, ino, &code, &msg, now_ms(), false)?;
        tracing::warn!(ino, %code, "sync error: {msg}");
        Ok(())
    }

    async fn run_uploads(self: &Arc<Self>, inos: Vec<u64>) -> EngineResult<bool> {
        let limit = self.cfg().max_parallel_uploads.max(1);
        let sem = Arc::new(tokio::sync::Semaphore::new(limit));
        let mut set = JoinSet::new();
        for ino in inos {
            let me = self.clone();
            let permit = sem.clone().acquire_owned().await.map_err(|e| EngineError::Other(e.to_string()))?;
            set.spawn(async move {
                let r = me.upload_one(ino).await;
                drop(permit);
                (ino, r)
            });
        }
        let mut progress = false;
        let mut account_err = None;
        while let Some(res) = set.join_next().await {
            let Ok((ino, r)) = res else { continue };
            match r {
                Ok(()) => progress = true,
                Err(e) => {
                    if let Err(acct) = self.record_item_error(ino, &e) {
                        account_err = Some(acct);
                    }
                }
            }
        }
        match account_err {
            Some(e) => Err(e),
            None => Ok(progress),
        }
    }

    async fn push_create_folder(&self, item: &Item, parent_id: &str) -> EngineResult<bool> {
        let created = match self.graph.create_folder(parent_id, &item.name, ConflictBehavior::Fail).await {
            Ok(it) => it,
            Err(e) if e.is_name_conflict() => {
                let existing = self.graph.child_by_name(parent_id, &item.name).await?;
                if !existing.is_folder() {
                    // A file online has this name: let the next pull rename ours.
                    self.pull_wake.notify_one();
                    return Ok(false);
                }
                existing
            }
            Err(e) => return Err(e.into()),
        };
        let c = self.store.lock();
        store::set_remote(&c, item.ino, &RemoteState::from_item(&created))?;
        store::clear_error(&c, item.ino)?;
        Ok(true)
    }

    async fn push_move(&self, item: &Item, parent_id: &str) -> EngineResult<bool> {
        let id = item.item_id.clone().unwrap_or_default();
        let patch = ItemPatch {
            name: (item.remote_name.as_deref() != Some(item.name.as_str())).then(|| item.name.clone()),
            parent_id: (item.remote_parent_id.as_deref() != Some(parent_id)).then(|| parent_id.to_string()),
            ..Default::default()
        };
        let mut etag = item.etag.clone();
        for _ in 0..2 {
            match self.graph.update_item(&id, &patch, etag.as_deref()).await {
                Ok(updated) => {
                    let c = self.store.lock();
                    store::set_remote_meta(
                        &c,
                        item.ino,
                        updated.e_tag.as_deref(),
                        updated.parent_id(),
                        updated.name.as_deref(),
                    )?;
                    store::clear_error(&c, item.ino)?;
                    let path = store::rel_path(&c, item.ino).unwrap_or_default();
                    drop(c);
                    let kind = if patch.parent_id.is_some() { ActivityKind::Moved } else { ActivityKind::Renamed };
                    self.activity(kind, Some(item.ino), &path, 0, "");
                    return Ok(true);
                }
                Err(e) if e.is_precondition_failed() => {
                    // eTag moved on (often spuriously). Retry with the fresh eTag
                    // unless the content changed, which the pull/upload handle.
                    let remote = self.graph.item(&id).await?;
                    if crate::pull::content_changed(item, &RemoteState::from_item(&remote)) {
                        self.pull_wake.notify_one();
                        return Ok(false);
                    }
                    etag = remote.e_tag.clone();
                }
                Err(e) if e.is_not_found() => {
                    self.pull_wake.notify_one();
                    return Ok(false);
                }
                Err(e) if e.is_name_conflict() => {
                    // Probably a held delete still occupies the name; retry later.
                    return Err(e.into());
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(false)
    }

    /// Propagate local deletes (tombstones), honouring the mass-delete hold.
    async fn push_deletes(self: &Arc<Self>) -> EngineResult<bool> {
        // Files moved out of the synced area (e.g. to the Trash) must be fully
        // downloaded before their server copies are deleted.
        let origins = store::pending_origin_downloads(&self.store.lock())?;
        for ino in origins {
            if let Err(e) = self.hydrate(ino).await {
                tracing::warn!(ino, "can't download a file before deleting it online yet: {e}");
                return Ok(false);
            }
        }
        let tombs = store::pending_deletes(&self.store.lock())?;
        if tombs.is_empty() {
            if self.held_deletes.swap(0, Ordering::SeqCst) != 0 {
                self.emit(EngineEvent::StateChanged);
            }
            self.mass_delete_approved.store(false, Ordering::SeqCst);
            return Ok(false);
        }
        let synced: Vec<&Item> = tombs.iter().filter(|t| t.item_id.is_some()).collect();
        let count = synced.len() as u32;
        let known = store::synced_count(&self.store.lock())?.max(1) as u32;
        let threshold = self.cfg().mass_delete_threshold;
        let mass = threshold > 0 && (count >= threshold || (count >= 20 && count * 2 >= known));
        if mass && !self.mass_delete_approved.load(Ordering::SeqCst) {
            if self.held_deletes.swap(count, Ordering::SeqCst) != count {
                self.emit(EngineEvent::MassDeleteHeld(count));
                self.emit(EngineEvent::StateChanged);
            }
            // Never-uploaded items can go right away; synced ones wait for the user.
            let c = self.store.lock();
            for t in tombs.iter().filter(|t| t.item_id.is_none()) {
                store::remove_row(&c, t.ino)?;
            }
            return Ok(false);
        }

        let tomb_set: HashSet<u64> = tombs.iter().map(|t| t.ino).collect();
        let mut progress = false;
        for t in &tombs {
            if t.item_id.is_none() {
                store::remove_row(&self.store.lock(), t.ino)?;
                progress = true;
                continue;
            }
            // Only the top-most deleted item is deleted online; descendants go with it.
            if t.parent_ino.is_some_and(|p| tomb_set.contains(&p)) {
                continue;
            }
            match self.push_delete(t).await {
                Ok(p) => progress |= p,
                Err(e) => self.record_item_error(t.ino, &e)?,
            }
        }
        Ok(progress)
    }

    async fn push_delete(self: &Arc<Self>, t: &Item) -> EngineResult<bool> {
        let id = t.item_id.clone().unwrap_or_default();
        let path = store::rel_path(&self.store.lock(), t.ino).unwrap_or_else(|_| t.name.clone());
        if t.is_dir {
            // Only delete a folder online if everything in it is known to us and
            // deleted locally: never delete content we have never seen.
            if let Some(unknown) = self.unknown_remote_descendant(&id).await? {
                tracing::warn!(%path, %unknown, "folder has new content online; restoring instead of deleting");
                self.restore_tombstone(t, "folder has content online that this device hasn't seen")?;
                self.pull_wake.notify_one();
                return Ok(true);
            }
        }
        let mut etag = t.etag.clone();
        for _ in 0..2 {
            match self.graph.delete_item(&id, etag.as_deref()).await {
                Ok(()) => {
                    {
                        let c = self.store.lock();
                        for d in store::descendants(&c, t.ino)? {
                            store::remove_row(&c, d)?;
                        }
                        store::remove_row(&c, t.ino)?;
                        store::log_decision(
                            &c,
                            "delete-online",
                            Some(t.ino),
                            Some(&id),
                            &path,
                            "deleted on this device",
                        )?;
                    }
                    self.activity(ActivityKind::Deleted, None, &path, 0, "");
                    return Ok(true);
                }
                Err(e) if e.is_precondition_failed() => {
                    let remote = self.graph.item(&id).await?;
                    if !t.is_dir && crate::pull::content_changed(t, &RemoteState::from_item(&remote)) {
                        // Changed online after our delete: keep the newer version.
                        self.restore_tombstone(t, "changed online after the local delete")?;
                        self.pull_wake.notify_one();
                        return Ok(true);
                    }
                    etag = remote.e_tag.clone();
                }
                Err(e) if e.is_access_denied() => {
                    self.restore_tombstone(t, "no permission to delete online")?;
                    let c = self.store.lock();
                    store::set_error(
                        &c,
                        t.ino,
                        "accessDenied",
                        "You don't have permission to delete this item online, so it was restored.",
                        now_ms(),
                        true,
                    )?;
                    return Ok(true);
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(false)
    }

    /// First remote descendant of a folder that is not a local tombstone.
    async fn unknown_remote_descendant(&self, folder_id: &str) -> EngineResult<Option<String>> {
        let mut stack = vec![folder_id.to_string()];
        let mut visited = 0;
        while let Some(id) = stack.pop() {
            for child in self.graph.children(&id).await? {
                visited += 1;
                let known = store::by_item_id(&self.store.lock(), &child.id)?;
                match known {
                    Some(k) if k.deleted || self.under_tombstone(&k)? => {}
                    _ => return Ok(Some(child.name.unwrap_or(child.id))),
                }
                if child.is_folder() {
                    stack.push(child.id);
                }
                if visited > 100_000 {
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }

    fn under_tombstone(&self, it: &Item) -> EngineResult<bool> {
        let c = self.store.lock();
        let mut cur = it.parent_ino;
        while let Some(p) = cur {
            let Some(row) = store::get(&c, p)? else { return Ok(false) };
            if row.deleted {
                return Ok(true);
            }
            cur = row.parent_ino;
        }
        Ok(false)
    }

    /// Bring a locally deleted item (and its deleted descendants) back.
    fn restore_tombstone(&self, t: &Item, reason: &str) -> EngineResult<()> {
        let mut invals = Vec::new();
        {
            let c = self.store.lock();
            let mut todo = vec![t.ino];
            todo.extend(store::descendants(&c, t.ino)?);
            for ino in todo {
                let Some(row) = store::get(&c, ino)? else { continue };
                if !row.deleted {
                    continue;
                }
                let parent = row.parent_ino.unwrap_or(ROOT_INO);
                let name = crate::engine::unique_name(&c, parent, &row.name, &self.cfg().hostname)?;
                store::untombstone(&c, ino)?;
                if name != row.name {
                    store::set_name_parent(&c, ino, parent, &name)?;
                }
                store::set_hydrated(&c, ino, false, None)?;
                invals.push(Inval::Entry { parent, name });
            }
            let path = store::rel_path(&c, t.ino).unwrap_or_default();
            store::log_decision(&c, "restore", Some(t.ino), t.item_id.as_deref(), &path, reason)?;
        }
        self.invalidate(invals);
        self.items_changed(&[t.ino]);
        Ok(())
    }

    /// Answer the mass-delete prompt.
    pub fn resolve_mass_delete(self: &Arc<Self>, delete_online: bool) -> EngineResult<()> {
        if delete_online {
            self.mass_delete_approved.store(true, Ordering::SeqCst);
            {
                let c = self.store.lock();
                store::log_decision(&c, "mass-delete-approved", None, None, "", "user confirmed")?;
            }
        } else {
            let tombs = store::pending_deletes(&self.store.lock())?;
            let tops: Vec<Item> = {
                let set: HashSet<u64> = tombs.iter().map(|t| t.ino).collect();
                tombs.into_iter().filter(|t| !t.parent_ino.is_some_and(|p| set.contains(&p))).collect()
            };
            for t in &tops {
                self.restore_tombstone(t, "user chose Restore files")?;
            }
        }
        self.held_deletes.store(0, Ordering::SeqCst);
        self.emit(EngineEvent::StateChanged);
        self.push_wake.notify_one();
        Ok(())
    }

    // ---------------------------------------------------------------- pins

    /// "Always keep on this device" (AlwaysKeep), "Free up space" (OnlineOnly)
    /// or neither (Inherit), for a file or a whole folder.
    pub fn set_pin_state(self: &Arc<Self>, ino: u64, pin: PinState) -> EngineResult<()> {
        let mut evicted = Vec::new();
        let mut changed = vec![ino];
        {
            let c = self.store.lock();
            store::set_pin(&c, ino, pin)?;
            let desc = store::descendants(&c, ino)?;
            for d in &desc {
                store::set_pin(&c, *d, PinState::Inherit)?;
            }
            changed.extend(desc.iter().copied());
            if pin == PinState::OnlineOnly {
                for f in std::iter::once(ino).chain(desc) {
                    let Some(it) = store::get_live(&c, f)? else { continue };
                    // Dehydration guard: only clean, closed, verified content.
                    if !it.is_dir
                        && it.hydrated
                        && !it.dirty
                        && it.item_id.is_some()
                        && it.content_hash.is_some()
                        && it.content_hash == it.remote_hash
                        && !self.is_open(f)
                        && !self.is_busy(f)
                    {
                        store::set_hydrated(&c, f, false, None)?;
                        evicted.push(f);
                    }
                }
            }
        }
        for f in &evicted {
            let _ = self.content.remove(*f);
        }
        self.invalidate(evicted.iter().map(|i| Inval::Inode { ino: *i }).collect());
        if !evicted.is_empty() {
            let rel = store::rel_path(&self.store.lock(), ino).unwrap_or_default();
            self.activity(ActivityKind::FreedUp, Some(ino), &rel, 0, "");
        }
        if pin == PinState::AlwaysKeep {
            self.hydrate_wake.notify_one();
        }
        self.items_changed(&changed);
        Ok(())
    }

    // ---------------------------------------------------------------- background

    async fn hydrate_loop(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.hydrate_wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(60)) => {}
            }
            if self.is_paused() || self.is_offline() || self.is_auth_required() {
                continue;
            }
            let todo = {
                let c = self.store.lock();
                let mut v = store::stale_items(&c, 100).unwrap_or_default();
                v.extend(store::hydration_candidates(&c, 200).unwrap_or_default());
                v.dedup();
                v
            };
            if todo.is_empty() {
                continue;
            }
            let sem = Arc::new(tokio::sync::Semaphore::new(self.cfg().max_parallel_downloads.max(1)));
            let mut set = JoinSet::new();
            for ino in todo {
                let me = self.clone();
                let Ok(permit) = sem.clone().acquire_owned().await else { break };
                set.spawn(async move {
                    let r = me.hydrate(ino).await;
                    drop(permit);
                    (ino, r)
                });
            }
            let mut again = false;
            while let Some(Ok((ino, r))) = set.join_next().await {
                match r {
                    Ok(()) => again = true,
                    Err(EngineError::NoSpace) => break,
                    Err(e) => {
                        let _ = self.record_item_error(ino, &e);
                    }
                }
            }
            if again {
                self.hydrate_wake.notify_one();
            }
        }
    }

    async fn maintenance_loop(self: Arc<Self>) {
        let mut tick: u64 = 0;
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(60)) => {}
            }
            tick += 1;
            self.gc_open();
            if tick % 60 == 1 {
                self.backup_db();
                let _ = store::prune_decision_log(&self.store.lock(), chrono::Utc::now().timestamp() - 30 * 86_400);
                let purged = self.content.purge_trash(Duration::from_secs(7 * 86_400));
                if purged > 0 {
                    tracing::info!(purged, "purged expired copies of files deleted online");
                }
            }
            if tick.is_multiple_of(30) {
                self.auto_free_up();
            }
            if tick % 60 == 5 && self.is_idle() {
                self.periodic_checks().await;
            }
        }
    }

    /// Nothing transferring or open for writing: safe moment for heavy checks.
    fn is_idle(&self) -> bool {
        self.progress().files_pending() == 0 && !self.is_paused()
    }

    /// Safety nets (research doc 04 §12 P1/P2): weekly full integrity check,
    /// daily DB↔content consistency check and daily full remote reconcile.
    async fn periodic_checks(self: &Arc<Self>) {
        let now = chrono::Utc::now().timestamp();
        let due = |key: &str, every: i64| -> bool {
            let c = self.store.lock();
            let last: i64 = store::kv_get(&c, key).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0);
            if last == 0 {
                // First run: start the clock instead of checking immediately.
                let _ = store::kv_set(&c, key, &now.to_string());
                return false;
            }
            now - last >= every
        };
        let mark = |key: &str| {
            let _ = store::kv_set(&self.store.lock(), key, &now.to_string());
        };

        if due("integrity_at", 7 * 86_400) {
            mark("integrity_at");
            let me = self.clone();
            let r = tokio::task::spawn_blocking(move || me.store.integrity_check()).await;
            if let Ok(Err(e)) = r {
                let dir = self.cfg().account_dir;
                let _ = std::fs::write(dir.join(crate::engine::REPAIR_MARKER), e.to_string());
                self.set_fatal(format!("The sync database is damaged ({e}). Restart Linux OneDrive to repair it."));
                return;
            }
        }
        if due("content_check_at", 86_400) {
            mark("content_check_at");
            self.runtime_content_check();
        }
        let every = self.cfg().full_reconcile_every.as_secs() as i64;
        if every > 0 && due("reconcile_at", every) {
            mark("reconcile_at");
            if let Err(e) = self.full_reconcile().await {
                tracing::warn!("full reconcile failed: {e}");
            }
        }
    }

    /// Clean rows whose cached content vanished become online-only again
    /// (never touches dirty items or files in use).
    fn runtime_content_check(&self) {
        let missing: Vec<u64> = {
            let c = self.store.lock();
            let Ok(mut st) = c.prepare("SELECT ino FROM items WHERE hydrated = 1 AND dirty = 0 AND is_dir = 0 AND deleted = 0 AND item_id IS NOT NULL") else {
                return;
            };
            st.query_map([], |r| r.get::<_, i64>(0))
                .map(|rows| rows.filter_map(|r| r.ok()).map(|v| v as u64).collect())
                .unwrap_or_default()
        };
        let mut fixed = 0;
        for ino in missing {
            if !self.content.exists(ino) && !self.is_open(ino) {
                let c = self.store.lock();
                let _ = store::set_hydrated(&c, ino, false, None);
                let _ = store::log_decision(
                    &c,
                    "content-missing",
                    Some(ino),
                    None,
                    "",
                    "cached content vanished; now online-only",
                );
                fixed += 1;
            }
        }
        if fixed > 0 {
            tracing::warn!(fixed, "cached copies were missing; those files are online-only again");
        }
    }

    /// Re-list the whole drive and reconcile (catches anything delta missed).
    pub async fn full_reconcile(self: &Arc<Self>) -> EngineResult<()> {
        {
            let c = self.store.lock();
            if store::kv_get(&c, "delta_next")?.is_some() {
                return Ok(()); // an enumeration is already in progress
            }
            store::kv_set(&c, "force_full", "1")?;
        }
        self.pull_once().await.map(|_| ())
    }

    /// Storage Sense equivalent: dehydrate unpinned files not opened for N days.
    fn auto_free_up(self: &Arc<Self>) {
        let days = self.cfg().auto_free_up_days;
        if days == 0 {
            return;
        }
        let before = chrono::Utc::now().timestamp() - days as i64 * 86_400;
        let mut evicted = Vec::new();
        {
            let c = self.store.lock();
            for ino in store::eviction_candidates(&c, before, 500).unwrap_or_default() {
                if store::effective_pin(&c, ino).ok() == Some(PinState::AlwaysKeep) || self.is_open(ino) {
                    continue;
                }
                let Ok(Some(it)) = store::get(&c, ino) else { continue };
                if it.content_hash.is_some()
                    && it.content_hash == it.remote_hash
                    && store::set_hydrated(&c, ino, false, None).is_ok()
                {
                    evicted.push(ino);
                }
            }
        }
        for ino in &evicted {
            let _ = self.content.remove(*ino);
        }
        if !evicted.is_empty() {
            tracing::info!(count = evicted.len(), "freed up space for files not opened recently");
            self.invalidate(evicted.iter().map(|i| Inval::Inode { ino: *i }).collect());
            self.items_changed(&evicted);
        }
    }
}
