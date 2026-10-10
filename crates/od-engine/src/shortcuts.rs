//! Shortcuts to shared folders ("Add shortcut to My files").
//!
//! In the user's drive a shortcut is a placeholder item with a `remoteItem`
//! facet; the folder itself lives in someone else's drive. Its content is
//! synced by a separate engine scoped to that folder, which the daemon
//! mounts elsewhere. This engine only records the placeholders (from delta)
//! and shows each one as a symbolic link to that mount, so
//! `~/OneDrive/Team Docs` leads into the shared folder, the way the folder
//! with a chain link does on Windows.
//!
//! Deleting the link removes the shortcut (the shared content is never
//! touched: DELETE on a placeholder only removes the placeholder, research
//! doc 02 §7.4); renaming it renames the shortcut.

use crate::engine::{Engine, EngineEvent, EngineResult, Inval};
use crate::store;
use od_graph::{DriveItem, ItemPatch};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Inode numbers of links, far above the store's (AUTOINCREMENT) inodes.
pub const LINK_INO_BASE: u64 = 1 << 48;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Shortcut {
    /// Placeholder item id in this drive.
    pub id: String,
    pub name: String,
    /// Folder of this drive holding the placeholder.
    pub parent_id: String,
    /// The shared folder: its drive and item id.
    pub drive_id: String,
    pub item_id: String,
    /// "personal" | "business" | "documentLibrary"
    pub drive_type: String,
}

impl Shortcut {
    /// A shortcut to a shared folder, from a delta or listing item.
    pub fn from_item(it: &DriveItem) -> Option<Shortcut> {
        let r = it.remote_item.as_ref()?;
        r.folder.as_ref()?;
        let rp = r.parent_reference.as_ref()?;
        Some(Shortcut {
            id: it.id.clone(),
            name: it.name.clone().filter(|n| !n.is_empty()).or_else(|| r.name.clone())?,
            parent_id: it.parent_id()?.to_string(),
            drive_id: rp.drive_id.clone()?,
            item_id: r.id.clone(),
            drive_type: rp.drive_type.clone().unwrap_or_else(|| "personal".into()),
        })
    }
}

/// A shortcut shown in the filesystem.
#[derive(Debug, Clone)]
pub struct LinkEntry {
    pub ino: u64,
    pub parent: u64,
    pub name: String,
    pub target: PathBuf,
    pub shortcut: Shortcut,
}

#[derive(Default)]
pub(crate) struct Links {
    /// Mount point of each shortcut's own engine, by placeholder id.
    targets: HashMap<String, PathBuf>,
    entries: Vec<LinkEntry>,
    inos: HashMap<String, u64>,
}

pub(crate) fn load(c: &Connection) -> Vec<Shortcut> {
    store::kv_get(c, "shortcuts").ok().flatten().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
}

pub(crate) fn save(c: &Connection, list: &[Shortcut]) -> store::StoreResult<()> {
    store::kv_set(c, "shortcuts", &serde_json::to_string(list).unwrap_or_else(|_| "[]".into()))
}

impl Engine {
    /// Shortcuts to shared folders found in this drive.
    pub fn shortcuts(&self) -> Vec<Shortcut> {
        load(&self.store.lock())
    }

    /// Where each shortcut's content is mounted (by placeholder id); shortcuts
    /// without a target aren't shown.
    pub fn set_shortcut_targets(&self, targets: HashMap<String, PathBuf>) {
        self.links.lock().unwrap().targets = targets;
        self.rebuild_links();
    }

    /// Recompute the links from the recorded shortcuts and refresh the
    /// kernel's view of the folders that gained or lost one.
    pub(crate) fn rebuild_links(&self) {
        let list = self.shortcuts();
        let mut invals = Vec::new();
        {
            let c = self.store.lock();
            let mut links = self.links.lock().unwrap();
            let mut entries = Vec::new();
            for s in list {
                let Some(target) = links.targets.get(&s.id).cloned() else { continue };
                let Ok(Some(parent)) = store::by_item_id(&c, &s.parent_id) else { continue };
                // A real item of the same name wins (it would clash online too).
                if matches!(store::child_folded(&c, parent.ino, &s.name), Ok(Some(_))) {
                    tracing::warn!(name = %s.name, "a shortcut has the same name as an item next to it; not shown");
                    continue;
                }
                let next = LINK_INO_BASE + links.inos.len() as u64;
                let ino = *links.inos.entry(s.id.clone()).or_insert(next);
                entries.push(LinkEntry { ino, parent: parent.ino, name: s.name.clone(), target, shortcut: s });
            }
            let old = std::mem::replace(&mut links.entries, entries);
            for e in old.iter().chain(links.entries.iter()) {
                invals.push(Inval::Entry { parent: e.parent, name: e.name.clone() });
                invals.push(Inval::Inode { ino: e.parent });
            }
        }
        self.invalidate(invals);
    }

    pub(crate) fn link_child(&self, parent: u64, name: &str) -> Option<LinkEntry> {
        let links = self.links.lock().unwrap();
        links.entries.iter().find(|e| e.parent == parent && e.name.to_lowercase() == name.to_lowercase()).cloned()
    }

    pub(crate) fn link_by_ino(&self, ino: u64) -> Option<LinkEntry> {
        if ino < LINK_INO_BASE {
            return None;
        }
        self.links.lock().unwrap().entries.iter().find(|e| e.ino == ino).cloned()
    }

    pub(crate) fn links_in(&self, parent: u64) -> Vec<LinkEntry> {
        self.links.lock().unwrap().entries.iter().filter(|e| e.parent == parent).cloned().collect()
    }

    /// Each shown shortcut and the absolute path of its link.
    pub fn shortcut_links(&self) -> Vec<(Shortcut, PathBuf)> {
        let entries = self.links.lock().unwrap().entries.clone();
        entries.into_iter().map(|e| (e.shortcut, self.local_path(e.parent).join(&e.name))).collect()
    }

    /// Forget a shortcut locally and tell the daemon.
    fn drop_shortcut(&self, id: &str) -> EngineResult<()> {
        {
            let c = self.store.lock();
            let mut list = load(&c);
            list.retain(|s| s.id != id);
            save(&c, &list)?;
        }
        self.rebuild_links();
        self.emit(EngineEvent::ShortcutsChanged);
        Ok(())
    }

    /// Remove a shortcut from the user's drive (the shared folder stays).
    pub async fn remove_shortcut(&self, id: &str) -> EngineResult<()> {
        self.graph.delete_item(id, None).await?;
        self.drop_shortcut(id)
    }

    pub async fn rename_shortcut(&self, id: &str, new_name: &str) -> EngineResult<()> {
        let patch = ItemPatch { name: Some(new_name.to_string()), ..Default::default() };
        let it = self.graph.update_item(id, &patch, None).await?;
        {
            let c = self.store.lock();
            let mut list = load(&c);
            for s in list.iter_mut().filter(|s| s.id == id) {
                s.name = it.name.clone().unwrap_or_else(|| new_name.to_string());
            }
            save(&c, &list)?;
        }
        self.rebuild_links();
        self.emit(EngineEvent::ShortcutsChanged);
        Ok(())
    }

    /// Mount point of a shortcut's content, if it is shown.
    pub fn shortcut_target(&self, id: &str) -> Option<PathBuf> {
        self.links.lock().unwrap().targets.get(id).cloned()
    }
}
