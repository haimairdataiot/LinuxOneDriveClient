//! Operations used by the daemon's D-Bus API.

use crate::engine::{Engine, EngineError, EngineEvent, EngineResult, Inval};
use crate::store::{self, Item, LocalOnly, ROOT_INO};
use std::path::Path;
use std::sync::Arc;

impl Engine {
    /// Item for an absolute path inside this account's mount.
    pub fn resolve_abs(&self, abs: &Path) -> EngineResult<Item> {
        let mp = self.mount_point();
        let rel = abs
            .strip_prefix(&mp)
            .map_err(|_| EngineError::Other(format!("{} is not inside {}", abs.display(), mp.display())))?;
        let rel = rel.to_string_lossy();
        store::resolve(&self.store.lock(), &rel)?.ok_or(EngineError::Store(store::StoreError::NotFound))
    }

    /// Server id of an item, fetching nothing (fails for never-uploaded items).
    pub fn item_id_of(&self, abs: &Path) -> EngineResult<String> {
        self.resolve_abs(abs)?.item_id.ok_or_else(|| EngineError::Other("This item hasn't been uploaded yet.".into()))
    }

    /// "Choose folders": hide a top-level folder from this device, or show it
    /// again. Hiding never deletes: it refuses while the folder holds
    /// unsynced changes, and only frees clean cached content.
    pub fn set_folder_hidden(self: &Arc<Self>, name: &str, hidden: bool) -> EngineResult<()> {
        let mut evict = Vec::new();
        let ino;
        {
            let c = self.store.lock();
            let folder =
                store::child_folded(&c, ROOT_INO, name)?.ok_or(EngineError::Store(store::StoreError::NotFound))?;
            if !folder.is_dir {
                return Err(EngineError::Other(format!("\"{name}\" is not a folder")));
            }
            ino = folder.ino;
            if hidden {
                for d in store::descendants(&c, ino)? {
                    let Some(it) = store::get(&c, d)? else { continue };
                    if !it.deleted
                        && (it.dirty || (it.item_id.is_none() && it.local_only == LocalOnly::No) || self.is_open(d))
                    {
                        return Err(EngineError::Other(format!(
                            "\"{name}\" has changes that haven't synced yet. Try again when syncing finishes."
                        )));
                    }
                }
                for d in store::descendants(&c, ino)? {
                    if let Some(it) = store::get(&c, d)?
                        && it.hydrated
                        && !it.is_dir
                    {
                        store::set_hydrated(&c, d, false, None)?;
                        evict.push(d);
                    }
                }
                store::set_local_only(&c, ino, LocalOnly::Excluded)?;
                store::log_decision(&c, "hide-folder", Some(ino), folder.item_id.as_deref(), name, "Choose folders")?;
            } else if folder.local_only == LocalOnly::Excluded {
                store::set_local_only(&c, ino, LocalOnly::No)?;
            }
        }
        for d in &evict {
            let _ = self.content.remove(*d);
        }
        self.update_cfg(|c| {
            c.excluded_folders.retain(|f| !f.eq_ignore_ascii_case(name));
            if hidden {
                c.excluded_folders.push(name.to_string());
            }
        });
        self.invalidate(vec![Inval::Entry { parent: ROOT_INO, name: name.to_string() }]);
        self.emit(EngineEvent::StateChanged);
        Ok(())
    }

    /// Top-level folders with their visibility, for the "Choose folders" dialog.
    pub fn top_level_folders(&self) -> EngineResult<Vec<(String, bool)>> {
        let c = self.store.lock();
        let mut st = c.prepare(
            "SELECT name, local_only FROM items WHERE parent_ino = 1 AND ino <> 1 AND is_dir = 1 AND deleted = 0 ORDER BY name",
        )?;
        let rows =
            st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? == LocalOnly::Excluded as i64)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// The network came back or went away (from NetworkManager).
    pub fn network_changed(&self, online: bool) {
        if online {
            self.set_offline(false);
            self.refresh();
            self.local_changed();
        }
    }

    /// Retry everything that failed ("Retry" in Sync issues).
    pub fn retry_failed(&self) -> EngineResult<()> {
        {
            let c = self.store.lock();
            c.execute(
                "UPDATE items SET retry_at = 0, attempts = 0 WHERE error_code IS NOT NULL AND local_only = 0",
                [],
            )?;
        }
        self.local_changed();
        self.hydrate_wake.notify_one();
        Ok(())
    }
}

impl Engine {
    /// Copies of files deleted online that this device still keeps (7 days).
    pub fn removed_items(&self) -> Vec<crate::content::TrashEntry> {
        self.content.list_trash()
    }

    /// Put a kept copy back where it was (it is uploaded again). Returns the
    /// restored path. Blocking: call from a blocking thread, never from a
    /// FUSE handler.
    pub fn restore_removed(&self, id: &str) -> EngineResult<std::path::PathBuf> {
        let src =
            self.content.trash_path(id).ok_or_else(|| EngineError::Other("That file is no longer kept.".into()))?;
        let entry = self
            .content
            .list_trash()
            .into_iter()
            .find(|e| e.id == id)
            .ok_or_else(|| EngineError::Other("That file is no longer kept.".into()))?;
        let mount = self.mount_point();
        let rel = std::path::Path::new(&entry.rel_path);
        let mut target = mount.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if target.exists() {
            let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mut n = 1;
            loop {
                let candidate = target.with_file_name(od_core::names::conflict_name(&name, "restored", n));
                if !candidate.exists() {
                    target = candidate;
                    break;
                }
                n += 1;
            }
        }
        std::fs::copy(&src, &target)?;
        std::fs::remove_file(&src)?;
        store::log_decision(
            &self.store.lock(),
            "restore-removed",
            None,
            None,
            &entry.rel_path,
            "restored by the user",
        )?;
        Ok(target)
    }
}

impl Engine {
    /// Record whether an item is shared (from a sharing change) and refresh
    /// file-manager emblems.
    pub fn set_shared(&self, ino: u64, shared: bool) {
        {
            let c = self.store.lock();
            let _ =
                c.execute("UPDATE items SET shared = ?2 WHERE ino = ?1", rusqlite::params![ino as i64, shared as i64]);
        }
        self.items_changed(&[ino]);
    }
}
