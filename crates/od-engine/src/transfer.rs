//! Downloads (hydration) and uploads.
//!
//! Download pipeline: partial file on the same filesystem → stream with
//! QuickXorHash → verify size + hash → fsync → re-check in a transaction that
//! the item is unchanged and has no writers → atomic rename → commit.
//!
//! Upload pipeline: wait until the file is closed and quiet → open a
//! snapshot fd (writers that appear meanwhile get a copy-on-write detach in
//! the FUSE layer) → hash → remote pre-check (lost reply, conflict) →
//! resumable session → verify the server's hash → mark clean only if the
//! write generation did not change.

use crate::engine::{Engine, EngineError, EngineResult, Inval, NoticeKind, now_ms};
use crate::pull::content_changed;
use crate::store::{self, Item, LocalOnly, ROOT_INO, RemoteState};
use bytes::Bytes;
use od_core::types::{ActivityKind, DriveType, PinState};
use od_graph::client::UPLOAD_FRAGMENT;
use od_graph::{ConflictBehavior, FragmentResult, GraphError, UploadTarget};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use tokio::sync::watch;

impl Engine {
    /// Ensure the current server content of `ino` is cached. Concurrent
    /// callers for the same file share one download.
    pub async fn hydrate(self: &Arc<Self>, ino: u64) -> EngineResult<()> {
        // A streamed download already in progress completes the hydration.
        let stream = self.streams.lock().unwrap().get(&ino).cloned();
        if let Some(stream) = stream {
            return self.stream_wait(&stream).await;
        }
        let (tx, existing) = {
            let mut m = self.hydrations.lock().unwrap();
            match m.get(&ino) {
                Some(rx) => (None, Some(rx.clone())),
                None => {
                    let (tx, rx) = watch::channel(None);
                    m.insert(ino, rx);
                    (Some(tx), None)
                }
            }
        };
        if let Some(mut rx) = existing {
            loop {
                if let Some(r) = rx.borrow().clone() {
                    return r.map_err(|(errno, msg)| EngineError::Shared { errno, msg });
                }
                if rx.changed().await.is_err() {
                    return Err(EngineError::Other("download aborted".into()));
                }
            }
        }
        let tx = tx.expect("sender");
        let r = self.download_now(ino).await;
        let _ = tx.send(Some(r.as_ref().map(|_| ()).map_err(|e| (e.errno(), e.to_string()))));
        self.hydrations.lock().unwrap().remove(&ino);
        r
    }

    async fn download_now(self: &Arc<Self>, ino: u64) -> EngineResult<()> {
        let mut last_err: Option<GraphError> = None;
        for attempt in 0..3u32 {
            let row = store::get_live(&self.store.lock(), ino)?.ok_or(store::StoreError::NotFound)?;
            if row.is_dir || (row.hydrated && !row.is_stale()) || row.dirty {
                return Ok(());
            }
            let Some(item_id) = row.item_id.clone().or_else(|| row.origin_id.clone()) else {
                return if row.hydrated { Ok(()) } else { Err(store::StoreError::NotFound.into()) };
            };
            let size = row.remote_size.unwrap_or(row.size);
            let free = self.content.free_space().unwrap_or(u64::MAX);
            if free < size.saturating_add(self.cfg().disk_reserve) {
                self.notice(
                    NoticeKind::DiskFull,
                    "Your disk is almost full",
                    &format!("\"{}\" can't be downloaded. Free up some space on this device.", row.name),
                    None,
                );
                return Err(EngineError::NoSpace);
            }
            let partial = self.content.partial_path(ino);
            self.transfer_begin(ino, &row.name, size, false);
            let me = self.clone();
            let progress = move |done: u64| me.transfer_progress(ino, done);
            let res = self
                .graph
                .download_to(&item_id, &partial, Some(size), row.remote_hash.as_deref(), false, &self.cancel, &progress)
                .await;
            self.transfer_end(ino);
            let hash = match res {
                Ok(h) => h,
                Err(e) => {
                    let _ = std::fs::remove_file(&partial);
                    if matches!(e, GraphError::HashMismatch) {
                        tracing::warn!(ino, "downloaded content failed verification; retrying");
                        last_err = Some(e);
                        continue;
                    }
                    if e.is_offline() {
                        self.set_offline(true);
                        return Err(e.into());
                    }
                    if e.is_transient() {
                        tracing::warn!(ino, "download interrupted ({e}); retrying");
                        last_err = Some(e);
                        tokio::time::sleep(std::time::Duration::from_millis(400 * (attempt + 1) as u64)).await;
                        continue;
                    }
                    return Err(e.into());
                }
            };
            self.content.tag_path(&partial, &row.name, false);

            // Commit only if nothing changed while we downloaded.
            let installed = {
                let c = self.store.lock();
                let now = store::get_live(&c, ino)?;
                match now {
                    Some(n)
                        if n.remote_hash == row.remote_hash && n.ctag == row.ctag && !n.dirty && !self.is_busy(ino) =>
                    {
                        self.content.install(ino, &partial)?;
                        store::set_hydrated(&c, ino, true, Some(&hash))?;
                        if n.item_id.is_none() && n.origin_id.is_some() {
                            // A local-only copy is complete; its server copy may now go.
                            c.execute("UPDATE items SET origin_id = NULL WHERE ino = ?1", [ino as i64])?;
                        }
                        if row.hydrated {
                            // Refreshing a stale copy: now report the new size.
                            store::set_size_mtime(&c, ino, size, n.remote_mtime_ns.unwrap_or(n.mtime_ns))?;
                        }
                        true
                    }
                    Some(n) if n.dirty || self.is_busy(ino) => {
                        // Local writes won the race; never overwrite them.
                        let _ = std::fs::remove_file(&partial);
                        return Ok(());
                    }
                    Some(_) => {
                        let _ = std::fs::remove_file(&partial);
                        false // changed online meanwhile: download again
                    }
                    None => {
                        let _ = std::fs::remove_file(&partial);
                        return Err(store::StoreError::NotFound.into());
                    }
                }
            };
            if installed {
                self.clear_thumbnail_failure(ino);
                self.invalidate(vec![Inval::Inode { ino }]);
                self.items_changed(&[ino]);
                let rel = store::rel_path(&self.store.lock(), ino).unwrap_or_default();
                self.activity(ActivityKind::Downloaded, Some(ino), &rel, size, "");
                return Ok(());
            }
        }
        match last_err {
            Some(e) => Err(e.into()),
            None => Err(EngineError::Other("the file kept changing online while downloading".into())),
        }
    }

    /// Download the first `len` bytes of an online-only file (content sniffing,
    /// file managers) without hydrating it.
    pub async fn read_prefix(self: &Arc<Self>, ino: u64, len: u64) -> EngineResult<Vec<u8>> {
        let item_id =
            store::get_live(&self.store.lock(), ino)?.and_then(|r| r.item_id).ok_or(store::StoreError::NotFound)?;
        Ok(self.graph.download_range(&item_id, 0, len).await?)
    }

    // ---------------------------------------------------------------- upload

    /// Upload a new or modified file. Returns Ok(()) also when the upload was
    /// skipped for now (busy, waiting for the parent folder, conflict handled).
    pub(crate) async fn upload_one(self: &Arc<Self>, ino: u64) -> EngineResult<()> {
        let row = match store::get_live(&self.store.lock(), ino)? {
            Some(r) => r,
            None => return Ok(()),
        };
        if row.is_dir || row.local_only != LocalOnly::No || !row.hydrated || self.is_busy(ino) {
            return Ok(());
        }
        let Some(parent_id) =
            row.parent_ino.and_then(|p| store::get(&self.store.lock(), p).ok().flatten()).and_then(|p| p.item_id)
        else {
            return Ok(()); // parent folder not created online yet
        };

        // ---- snapshot
        let gen0 = self.generation(ino);
        let file = Arc::new(self.content.open_read(ino)?);
        let size = file.metadata()?.len();
        if size > od_core::names::MAX_FILE_SIZE {
            let c = self.store.lock();
            store::set_error(
                &c,
                ino,
                "too-large",
                "This file is larger than OneDrive's 250 GB limit.",
                now_ms(),
                true,
            )?;
            return Ok(());
        }
        self.uploading.lock().unwrap().insert(ino);
        let result = self.upload_snapshot(&row, &parent_id, file, size, gen0).await;
        self.uploading.lock().unwrap().remove(&ino);
        self.transfer_end(ino);
        result
    }

    async fn upload_snapshot(
        self: &Arc<Self>,
        row: &Item,
        parent_id: &str,
        file: Arc<std::fs::File>,
        size: u64,
        gen0: u64,
    ) -> EngineResult<()> {
        let ino = row.ino;
        let hash = {
            let f = file.clone();
            tokio::task::spawn_blocking(move || hash_fd(&f, size))
                .await
                .map_err(|e| EngineError::Other(e.to_string()))??
        };
        if self.generation(ino) != gen0 {
            return Ok(()); // written to while hashing; try again later
        }

        // ---- remote pre-check
        let target = match &row.item_id {
            Some(id) => match self.graph.item(id).await {
                Ok(remote_item) => {
                    let rs = RemoteState::from_item(&remote_item);
                    if rs.hash.as_deref() == Some(hash.as_str()) {
                        // Already there (identical content, or a reply we lost).
                        return self.commit_upload(ino, &rs, &hash, gen0, row, false);
                    }
                    if content_changed(row, &rs) {
                        return self.make_conflict_copy(ino, &rs);
                    }
                    UploadTarget::Existing { item_id: id.clone(), if_match: None }
                }
                Err(e) if e.is_not_found() => {
                    // Deleted online meanwhile: the next pull keeps our version as new.
                    self.pull_wake.notify_one();
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            },
            None => match self.graph.child_by_name(parent_id, &row.name).await {
                Ok(existing) => {
                    if existing.is_file() && existing.quick_xor_hash() == Some(hash.as_str()) {
                        let rs = RemoteState::from_item(&existing);
                        return self.commit_upload(ino, &rs, &hash, gen0, row, false);
                    }
                    // The name is taken online by something we don't know yet:
                    // let the next pull merge it (it renames our copy).
                    self.pull_wake.notify_one();
                    return Ok(());
                }
                Err(e) if e.is_not_found() => {
                    UploadTarget::New { parent_id: parent_id.to_string(), name: row.name.clone() }
                }
                Err(e) => return Err(e.into()),
            },
        };
        let conflict = match target {
            UploadTarget::New { .. } => ConflictBehavior::Fail,
            UploadTarget::Existing { .. } => ConflictBehavior::Replace,
        };

        // ---- transfer
        self.transfer_begin(ino, &row.name, size, true);
        let item = if size == 0 {
            self.graph.upload_small(&target, Bytes::new(), conflict).await.map(Some)
        } else {
            self.upload_session(row, &target, &file, size, &hash, gen0, conflict).await
        };
        let item = match item {
            Ok(Some(i)) => i,
            Ok(None) => return Ok(()), // aborted because the file changed
            Err(e) if e.is_name_conflict() => {
                self.pull_wake.notify_one();
                return Ok(());
            }
            Err(e) => {
                // The reply may have been lost after the server committed the
                // upload: if the server now has exactly our bytes, adopt them
                // instead of uploading again (Nextcloud #5106 lesson).
                if let Some(rs) = self.find_committed(row, parent_id, &hash).await {
                    tracing::info!(ino, "upload reply was lost but the server has the file; adopting it");
                    return self.commit_upload(ino, &rs, &hash, gen0, row, false);
                }
                return Err(e.into());
            }
        };

        // ---- verify
        let rs = RemoteState::from_item(&item);
        if let Some(server) = &rs.hash
            && *server != hash
        {
            if self.cfg().drive_type != DriveType::Personal && is_enriched_type(&row.name) {
                // SharePoint rewrites Office/PDF metadata on upload: adopt the
                // server's version as the new baseline instead of looping.
                return self.commit_upload(ino, &rs, &hash, gen0, row, true);
            }
            let c = self.store.lock();
            store::set_remote(&c, ino, &rs)?;
            store::set_error(
                &c,
                ino,
                "hash-mismatch",
                "The uploaded file didn't match; it will be uploaded again.",
                now_ms(),
                false,
            )?;
            return Ok(());
        }
        if rs.size.is_some_and(|s| s != size) {
            let c = self.store.lock();
            store::set_error(
                &c,
                ino,
                "size-mismatch",
                "The uploaded file size didn't match; it will be uploaded again.",
                now_ms(),
                false,
            )?;
            return Ok(());
        }
        self.commit_upload(ino, &rs, &hash, gen0, row, false)
    }

    /// Server state of the item if it already holds content with `hash`.
    async fn find_committed(&self, row: &Item, parent_id: &str, hash: &str) -> Option<RemoteState> {
        let found = match &row.item_id {
            Some(id) => self.graph.item(id).await.ok(),
            None => self.graph.child_by_name(parent_id, &row.name).await.ok(),
        }?;
        (found.is_file() && found.quick_xor_hash() == Some(hash)).then(|| RemoteState::from_item(&found))
    }

    #[allow(clippy::too_many_arguments)]
    async fn upload_session(
        self: &Arc<Self>,
        row: &Item,
        target: &UploadTarget,
        file: &Arc<std::fs::File>,
        size: u64,
        hash: &str,
        gen0: u64,
        conflict: ConflictBehavior,
    ) -> Result<Option<od_graph::DriveItem>, GraphError> {
        let ino = row.ino;
        // Resume a persisted session only for the very same snapshot.
        let mut session: Option<(String, u64)> = None;
        if let Some(url) = &row.upload_url {
            if row.upload_hash.as_deref() == Some(hash) {
                if let Ok(st) = self.graph.upload_session_status(url).await {
                    session = Some((url.clone(), st.next_offset().unwrap_or(0)));
                }
            } else {
                let _ = self.graph.cancel_upload_session(url).await;
            }
        }
        let (url, mut offset) = match session {
            Some(s) => s,
            None => {
                let mtime = clamp_time(row.mtime_ns);
                let ctime = clamp_time(row.ctime_ns);
                let s = self.graph.create_upload_session(target, mtime, ctime, conflict).await?;
                let _ = store::set_upload_session(&self.store.lock(), ino, Some(&s.upload_url), 0, Some(hash));
                (s.upload_url, 0)
            }
        };
        loop {
            if self.generation(ino) != gen0 {
                let _ = self.graph.cancel_upload_session(&url).await;
                let _ = store::set_upload_session(&self.store.lock(), ino, None, 0, None);
                return Ok(None);
            }
            let len = (UPLOAD_FRAGMENT as u64).min(size - offset) as usize;
            let chunk = {
                let f = file.clone();
                tokio::task::spawn_blocking(move || {
                    let mut buf = vec![0u8; len];
                    f.read_exact_at(&mut buf, offset).map(|_| buf)
                })
                .await
                .map_err(|e| GraphError::InvalidResponse(e.to_string()))??
            };
            match self.graph.upload_fragment(&url, Bytes::from(chunk), offset, size).await {
                Ok(FragmentResult::Continue(next)) => {
                    offset = next;
                    self.transfer_progress(ino, offset);
                    let _ = store::set_upload_session(&self.store.lock(), ino, Some(&url), offset, Some(hash));
                }
                Ok(FragmentResult::Complete(item)) => return Ok(Some(*item)),
                Err(e) if e.is_not_found() => {
                    // Session expired: start over next time.
                    let _ = store::set_upload_session(&self.store.lock(), ino, None, 0, None);
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Record a successful upload. The item becomes clean only if it was not
    /// written to since the snapshot was taken.
    fn commit_upload(
        &self,
        ino: u64,
        rs: &RemoteState,
        hash: &str,
        gen0: u64,
        row: &Item,
        adopt_server: bool,
    ) -> EngineResult<()> {
        let rel;
        let mut evict = false;
        {
            let c = self.store.lock();
            let Some(now) = store::get(&c, ino)? else { return Ok(()) };
            store::set_remote(&c, ino, rs)?;
            if now.deleted {
                return Ok(()); // the pending delete now targets the uploaded item
            }
            if self.generation(ino) == gen0 && now.dirty {
                if adopt_server {
                    store::mark_clean(&c, ino, None)?;
                    store::set_hydrated(&c, ino, false, None)?;
                    evict = true;
                } else {
                    store::mark_clean(&c, ino, Some(hash))?;
                    self.content.tag(ino, &now.name, false);
                    // "Free up space" was requested while the upload was pending.
                    if store::effective_pin(&c, ino)? == PinState::OnlineOnly && !self.is_open(ino) {
                        store::set_hydrated(&c, ino, false, None)?;
                        evict = true;
                    }
                }
            } else {
                store::set_upload_session(&c, ino, None, 0, None)?;
            }
            rel = store::rel_path(&c, ino).unwrap_or_else(|_| row.name.clone());
        }
        if evict {
            let _ = self.content.remove(ino);
            self.invalidate(vec![Inval::Inode { ino }]);
        }
        self.items_changed(&[ino]);
        self.activity(ActivityKind::Uploaded, Some(ino), &rel, rs.size.unwrap_or(0), "");
        Ok(())
    }

    /// Both sides changed: keep both. The local version moves to
    /// `name-<host>.ext` (uploaded as a new file); the server version keeps
    /// the original name.
    pub(crate) fn make_conflict_copy(&self, ino: u64, remote: &RemoteState) -> EngineResult<()> {
        let (rel_copy, parent, new_name, size);
        {
            let mut conn = self.store.lock();
            let tx = conn.transaction().map_err(store::StoreError::from)?;
            let Some(row) = store::get_live(&tx, ino)? else { return Ok(()) };
            if self.is_busy(ino) {
                return Ok(()); // resolve once the file is closed
            }
            parent = row.parent_ino.unwrap_or(ROOT_INO);
            new_name = crate::engine::unique_name(
                &tx,
                parent,
                &od_core::names::conflict_name(&row.name, &self.cfg().hostname, 1),
                &self.cfg().hostname,
            )?;
            size = self.content.len(ino).unwrap_or(row.size);
            let copy = store::insert(
                &tx,
                &store::NewItem {
                    parent_ino: parent,
                    name: new_name.clone(),
                    size,
                    mtime_ns: row.mtime_ns,
                    ctime_ns: row.ctime_ns,
                    mode: row.mode,
                    hydrated: true,
                    dirty: true,
                    ..Default::default()
                },
            )?;
            // Move the bytes before committing: a crash leaves an orphan
            // content file that the startup check salvages, never a loss.
            self.content.transfer(ino, copy)?;
            self.content.tag(copy, &new_name, true);
            store::set_remote(&tx, ino, remote)?;
            tx.execute(
                "UPDATE items SET dirty = 0, hydrated = 0, content_hash = NULL, upload_url = NULL, upload_offset = 0,
                        upload_hash = NULL, size = ?2, mtime_ns = ?3 WHERE ino = ?1",
                rusqlite::params![ino as i64, remote.size.unwrap_or(0) as i64, remote.mtime_ns.unwrap_or(row.mtime_ns)],
            )
            .map_err(store::StoreError::from)?;
            let path = store::rel_path(&tx, ino).unwrap_or_default();
            store::log_decision(
                &tx,
                "conflict-keep-both",
                Some(ino),
                row.item_id.as_deref(),
                &path,
                &format!("local version saved as {new_name}"),
            )?;
            rel_copy = store::rel_path(&tx, copy).unwrap_or_default();
            tx.commit().map_err(store::StoreError::from)?;
        }
        self.invalidate(vec![Inval::Inode { ino }, Inval::Entry { parent, name: new_name.clone() }]);
        self.items_changed(&[ino]);
        self.activity(ActivityKind::Conflict, Some(ino), &rel_copy, size, "Kept both versions");
        self.notice(
            NoticeKind::Conflict,
            "Kept both versions",
            &format!("This file changed online and on this device. Your version was saved as \"{new_name}\"."),
            Some(self.mount_point().join(&rel_copy)),
        );
        self.push_wake.notify_one();
        self.hydrate_wake.notify_one();
        Ok(())
    }
}

/// QuickXorHash of the first `size` bytes of an open file.
fn hash_fd(f: &std::fs::File, size: u64) -> std::io::Result<String> {
    let mut h = od_core::quickxor::QuickXorHash::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0u64;
    while off < size {
        let want = buf.len().min((size - off) as usize);
        let n = f.read_at(&mut buf[..want], off)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        off += n as u64;
    }
    Ok(h.finalize_base64())
}

/// OneDrive rejects timestamps outside 1980–2100; never send 0/negative times.
fn clamp_time(ns: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    if ns <= 0 {
        return None;
    }
    let t = chrono::DateTime::from_timestamp_nanos(ns);
    let min = chrono::DateTime::parse_from_rfc3339("1980-01-01T00:00:00Z").ok()?.to_utc();
    let max = chrono::DateTime::parse_from_rfc3339("2099-12-31T23:59:59Z").ok()?.to_utc();
    Some(t.clamp(min, max))
}

/// File types SharePoint may modify on upload (metadata enrichment).
fn is_enriched_type(name: &str) -> bool {
    let ext = od_core::names::split_ext(name).1.to_ascii_lowercase();
    matches!(ext.as_str(), ".docx" | ".xlsx" | ".pptx" | ".doc" | ".xls" | ".ppt" | ".pdf" | ".htm" | ".html" | ".aspx")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_clamping() {
        assert!(clamp_time(0).is_none());
        assert!(clamp_time(-5).is_none());
        let old = clamp_time(1_000_000_000).unwrap(); // 1970
        assert_eq!(old.to_rfc3339(), "1980-01-01T00:00:00+00:00");
        let now = chrono::Utc::now();
        let t = clamp_time(now.timestamp_nanos_opt().unwrap()).unwrap();
        assert_eq!(t, now);
        assert!(is_enriched_type("Report.DOCX"));
        assert!(!is_enriched_type("photo.jpg"));
    }
}
