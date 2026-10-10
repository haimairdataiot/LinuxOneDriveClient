//! Sync roots besides an account's own OneDrive folder:
//!
//! * **Shortcuts to shared folders** ("Add shortcut to My files"), found by
//!   the account's engine. Each gets an engine scoped to the shared folder,
//!   mounted under the data directory; the account's folder shows it as a
//!   symbolic link (see `od_engine::shortcuts`). Changes come from delta on
//!   the folder; where a service doesn't offer that, the engine lists the
//!   folder instead.
//! * **SharePoint document libraries**, chosen in Settings or with the
//!   library's "Sync" button in the browser (`odopen://`). Each is mounted
//!   at `~/<Organization>/<Site> - <Library>`, like on Windows.
//!
//! Paths through a shortcut link (`~/OneDrive/Team Docs/a.txt`) are routed
//! to the shortcut's engine, and its events are reported with those paths.

use crate::daemon::{AccountRt, Daemon};
use crate::mounts;
use anyhow::{Context, anyhow, bail};
use od_core::config::LibraryEntry;
use od_core::dbus::{LibraryInfo, SiteInfo, SyncLocation};
use od_core::types::DriveType;
use od_engine::{Engine, Shortcut, store};
use od_graph::GraphClient;
use od_graph::client::graph_base_from_env;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootKind {
    /// Placeholder item id of the shortcut in the account's drive.
    Shortcut(String),
    /// Drive id of the library.
    Library(String),
}

pub struct ExtraRoot {
    pub key: String,
    pub kind: RootKind,
    pub title: Mutex<String>,
    pub mount_point: PathBuf,
    pub engine: Arc<Engine>,
    pub session: Mutex<Option<fuser::BackgroundSession>>,
    /// Where the user sees it, if not at its mount (the shortcut's link).
    pub alias: Mutex<Option<PathBuf>>,
    pub web_url: String,
    /// The shortcut was removed but local changes still need uploading.
    pub orphaned: std::sync::atomic::AtomicBool,
}

impl ExtraRoot {
    /// A path of this root's mount as the user sees it.
    pub fn shown(&self, p: &Path) -> PathBuf {
        match (self.alias.lock().unwrap().as_ref(), p.strip_prefix(&self.mount_point)) {
            (Some(a), Ok(rel)) if rel.as_os_str().is_empty() => a.clone(),
            (Some(a), Ok(rel)) => a.join(rel),
            _ => p.to_path_buf(),
        }
    }

    /// The path a user-visible path stands for in this root's mount.
    pub fn real(&self, p: &Path) -> Option<PathBuf> {
        if p.starts_with(&self.mount_point) {
            return Some(p.to_path_buf());
        }
        let alias = self.alias.lock().unwrap().clone()?;
        let rel = p.strip_prefix(&alias).ok()?;
        Some(if rel.as_os_str().is_empty() { self.mount_point.clone() } else { self.mount_point.join(rel) })
    }

    fn has_pending_changes(&self) -> bool {
        store::pending_upload_stats(&self.engine.store.lock()).map(|(n, _)| n > 0).unwrap_or(true)
    }
}

/// Activity ids of a root's engine, kept apart from the account's own.
pub fn activity_id_offset(key: &str) -> i64 {
    let h = key.bytes().fold(7u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64));
    ((h % 4096) as i64 + 1) << 40
}

fn root_drive_type(t: &str) -> DriveType {
    match t {
        "personal" => DriveType::Personal,
        "documentLibrary" => DriveType::DocumentLibrary,
        _ => DriveType::Business,
    }
}

/// A file-name-safe version of a site or library title.
fn safe(s: &str) -> String {
    let t: String = s.chars().map(|c| if c == '/' || c == '\0' { '-' } else { c }).collect();
    let t = t.trim().trim_matches('.').to_string();
    if t.is_empty() { "Library".into() } else { t }
}

impl Daemon {
    fn shortcut_mount(&self, acct: &AccountRt, id: &str) -> PathBuf {
        od_core::paths::data_dir().join("shortcuts").join(&acct.id).join(id)
    }

    fn root_state_dir(acct: &AccountRt, key: &str) -> PathBuf {
        od_core::paths::account_dir(&acct.id).join("roots").join(key)
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_root(
        self: &Arc<Self>,
        acct: &Arc<AccountRt>,
        key: String,
        kind: RootKind,
        title: String,
        drive_id: &str,
        drive_type: &str,
        folder: Option<&str>,
        mount_point: PathBuf,
        web_url: String,
    ) -> anyhow::Result<Arc<ExtraRoot>> {
        let state_dir = Self::root_state_dir(acct, &key);
        od_core::paths::ensure_private_dir(&state_dir)?;
        let import = mounts::prepare(&mount_point, &state_dir)?;
        let mut graph =
            GraphClient::new(self.http.clone(), graph_base_from_env(), acct.tokens.clone()).with_drive(drive_id);
        if let Some(f) = folder {
            graph = graph.with_root(f);
        }
        graph.upload_limiter = acct.graph.upload_limiter.clone();
        graph.download_limiter = acct.graph.download_limiter.clone();
        let a = acct.cfg();
        let mut ec = self.engine_config(&a);
        ec.account_dir = state_dir;
        ec.mount_point = mount_point.clone();
        ec.drive_id = drive_id.to_string();
        ec.drive_type = root_drive_type(drive_type);
        ec.excluded_folders = Vec::new();
        ec.download_all = false;
        ec.track_quota = false;
        // The engine uses delta on the folder and switches to listing by
        // itself where the server doesn't offer that.
        ec.walk_listing = false;
        let engine = tokio::task::spawn_blocking(move || Engine::new(ec, graph)).await??;
        self.apply_pause_to(&engine);
        let session = od_engine::vfs::mount(engine.clone(), self.rt.clone())
            .with_context(|| format!("mounting {}", mount_point.display()))?;
        let root = Arc::new(ExtraRoot {
            key,
            kind,
            title: Mutex::new(title),
            mount_point: mount_point.clone(),
            engine: engine.clone(),
            session: Mutex::new(Some(session)),
            alias: Mutex::new(None),
            web_url,
            orphaned: Default::default(),
        });
        acct.roots.lock().unwrap().push(root.clone());
        engine.start();
        tokio::spawn(self.clone().forward_events(acct.clone(), engine.clone()));
        if let Some(dir) = import {
            let mp = mount_point.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = mounts::import_into(&dir, &mp) {
                    tracing::error!("importing {} failed: {e}; the files are kept there", dir.display());
                }
            });
        }
        tracing::info!(account = %acct.id, key = %root.key, mount = %mount_point.display(), "sync root started");
        Ok(root)
    }

    pub async fn stop_root(&self, root: &ExtraRoot) {
        root.engine.stop();
        root.engine.set_notifier(None);
        let session = root.session.lock().unwrap().take();
        if let Some(s) = session {
            mounts::unmount_session(s, root.mount_point.clone()).await;
        }
    }

    /// Stop a root for good and remove its local state (only when nothing
    /// is waiting to be uploaded).
    async fn retire_root(&self, acct: &AccountRt, root: &Arc<ExtraRoot>) -> bool {
        if root.has_pending_changes() {
            return false;
        }
        acct.roots.lock().unwrap().retain(|r| !Arc::ptr_eq(r, root));
        self.stop_root(root).await;
        let _ = std::fs::remove_dir(&root.mount_point);
        let _ = std::fs::remove_dir_all(Self::root_state_dir(acct, &root.key));
        tracing::info!(account = %acct.id, key = %root.key, "sync root removed");
        true
    }

    /// Start, stop and link the account's shortcuts to match its drive.
    pub async fn sync_shortcuts(self: &Arc<Self>, acct: &Arc<AccountRt>) {
        let _guard = acct.roots_lock.lock().await;
        let Some(main) = acct.engine() else { return };
        let list: Vec<Shortcut> = main.shortcuts();
        let running: Vec<Arc<ExtraRoot>> = acct.roots.lock().unwrap().clone();
        // Gone: retire (or keep uploading, unlinked, until clean).
        for r in &running {
            if let RootKind::Shortcut(id) = &r.kind
                && !list.iter().any(|s| &s.id == id)
                && !self.retire_root(acct, r).await
            {
                r.orphaned.store(true, std::sync::atomic::Ordering::Relaxed);
                *r.alias.lock().unwrap() = None;
            }
        }
        // New: start.
        for s in &list {
            let key = format!("sc-{}", s.id);
            let exists = running.iter().any(|r| r.key == key);
            if let Some(r) = running.iter().find(|r| r.key == key) {
                *r.title.lock().unwrap() = s.name.clone();
                r.orphaned.store(false, std::sync::atomic::Ordering::Relaxed);
            }
            if exists {
                continue;
            }
            let mp = self.shortcut_mount(acct, &s.id);
            let web = String::new();
            if let Err(e) = self
                .start_root(
                    acct,
                    key,
                    RootKind::Shortcut(s.id.clone()),
                    s.name.clone(),
                    &s.drive_id,
                    &s.drive_type,
                    Some(&s.item_id),
                    mp,
                    web,
                )
                .await
            {
                tracing::error!(shortcut = %s.name, "can't sync the shared folder: {e:#}");
            }
        }
        // Show each started shortcut as a link to its mount.
        let targets: HashMap<String, PathBuf> = acct
            .roots
            .lock()
            .unwrap()
            .iter()
            .filter(|r| !r.orphaned.load(std::sync::atomic::Ordering::Relaxed))
            .filter_map(|r| match &r.kind {
                RootKind::Shortcut(id) => Some((id.clone(), r.mount_point.clone())),
                RootKind::Library(_) => None,
            })
            .collect();
        main.set_shortcut_targets(targets);
        let links = main.shortcut_links();
        for r in acct.roots.lock().unwrap().iter() {
            if let RootKind::Shortcut(id) = &r.kind {
                *r.alias.lock().unwrap() = links.iter().find(|(s, _)| &s.id == id).map(|(_, p)| p.clone());
            }
        }
        self.account_props_changed_soon(&acct.id);
    }

    /// Retry retiring orphaned shortcuts once their changes are uploaded.
    pub async fn retire_orphans(&self, acct: &Arc<AccountRt>) {
        let orphans: Vec<Arc<ExtraRoot>> = acct
            .roots
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.orphaned.load(std::sync::atomic::Ordering::Relaxed))
            .cloned()
            .collect();
        for r in orphans {
            self.retire_root(acct, &r).await;
        }
    }

    pub async fn start_libraries(self: &Arc<Self>, acct: &Arc<AccountRt>) {
        let _guard = acct.roots_lock.lock().await;
        for lib in acct.cfg().libraries {
            let key = format!("lib-{}", lib.drive_id);
            if acct.roots.lock().unwrap().iter().any(|r| r.key == key) {
                continue;
            }
            if let Err(e) = self.start_library(acct, &lib).await {
                tracing::error!(library = %lib.name, "can't sync the library: {e:#}");
            }
        }
    }

    async fn start_library(self: &Arc<Self>, acct: &Arc<AccountRt>, lib: &LibraryEntry) -> anyhow::Result<()> {
        self.start_root(
            acct,
            format!("lib-{}", lib.drive_id),
            RootKind::Library(lib.drive_id.clone()),
            format!("{} - {}", lib.site, lib.name),
            &lib.drive_id,
            "documentLibrary",
            None,
            lib.mount_point.clone(),
            lib.web_url.clone(),
        )
        .await
        .map(|_| ())
    }

    /// Stop every extra root of an account (sign-out, shutdown, moving it).
    pub async fn stop_roots(&self, acct: &AccountRt) {
        let roots: Vec<Arc<ExtraRoot>> = std::mem::take(&mut *acct.roots.lock().unwrap());
        for r in roots {
            self.stop_root(&r).await;
        }
    }

    // ------------------------------------------------------------ libraries

    pub async fn search_sites(&self, acct: &AccountRt, query: &str) -> anyhow::Result<Vec<SiteInfo>> {
        if acct.cfg().drive_type == DriveType::Personal {
            bail!("SharePoint libraries are available with work or school accounts.");
        }
        let sites = acct.graph.search_sites(query).await.map_err(|e| anyhow!("{e}"))?;
        Ok(sites
            .into_iter()
            .map(|s| SiteInfo { name: s.title(), web_url: s.web_url.clone().unwrap_or_default(), id: s.id })
            .collect())
    }

    pub async fn site_libraries(&self, acct: &AccountRt, site_id: &str) -> anyhow::Result<Vec<LibraryInfo>> {
        let synced: Vec<String> = acct.cfg().libraries.iter().map(|l| l.drive_id.clone()).collect();
        let drives = acct.graph.site_drives(site_id).await.map_err(|e| anyhow!("{e}"))?;
        Ok(drives
            .into_iter()
            .filter(|d| d.drive_type.as_deref().is_none_or(|t| t == "documentLibrary"))
            .map(|d| LibraryInfo {
                synced: synced.contains(&d.id),
                name: d.name.clone().unwrap_or_else(|| "Documents".into()),
                web_url: d.web_url.clone().unwrap_or_default(),
                drive_id: d.id,
            })
            .collect())
    }

    fn library_mount(acct: &AccountRt, site: &str, name: &str) -> PathBuf {
        let a = acct.cfg();
        let org = if a.organization.is_empty() { "SharePoint".to_string() } else { safe(&a.organization) };
        let base = od_core::paths::home_dir().join(org);
        let first = base.join(format!("{} - {}", safe(site), safe(name)));
        if !first.exists() || std::fs::read_dir(&first).is_ok_and(|mut d| d.next().is_none()) {
            return first;
        }
        (2..).map(|n| base.join(format!("{} - {} ({n})", safe(site), safe(name)))).find(|p| !p.exists()).unwrap()
    }

    /// Start syncing a library; returns where it appears.
    pub async fn add_library(
        self: &Arc<Self>,
        acct: &Arc<AccountRt>,
        site_title: &str,
        drive_id: &str,
    ) -> anyhow::Result<PathBuf> {
        if acct.cfg().drive_type == DriveType::Personal {
            bail!("SharePoint libraries are available with work or school accounts.");
        }
        if let Some(l) = acct.cfg().libraries.iter().find(|l| l.drive_id == drive_id) {
            return Ok(l.mount_point.clone());
        }
        let d = acct.graph.drive_by_id(drive_id).await.map_err(|e| anyhow!("{e}"))?;
        let name = d.name.clone().unwrap_or_else(|| "Documents".into());
        let lib = LibraryEntry {
            drive_id: d.id.clone(),
            site: site_title.to_string(),
            name: name.clone(),
            web_url: d.web_url.clone().unwrap_or_default(),
            mount_point: Self::library_mount(acct, site_title, &name),
        };
        {
            let _guard = acct.roots_lock.lock().await;
            self.start_library(acct, &lib).await?;
        }
        self.update_account(&acct.id, |a| a.libraries.push(lib.clone()));
        self.account_props_changed_soon(&acct.id);
        Ok(lib.mount_point)
    }

    pub async fn add_library_from_site(
        self: &Arc<Self>,
        acct: &Arc<AccountRt>,
        site_id: &str,
        drive_id: &str,
    ) -> anyhow::Result<PathBuf> {
        let title = match acct.graph.site(site_id).await {
            Ok(s) => s.title(),
            Err(_) => "SharePoint".into(),
        };
        self.add_library(acct, &title, drive_id).await
    }

    /// "Stop sync" for a library, or remove a shortcut.
    pub async fn remove_location(self: &Arc<Self>, acct: &Arc<AccountRt>, key: &str) -> anyhow::Result<()> {
        let root = acct.roots.lock().unwrap().iter().find(|r| r.key == key).cloned();
        match root.as_ref().map(|r| r.kind.clone()) {
            Some(RootKind::Shortcut(id)) => {
                let main = acct.engine().ok_or_else(|| anyhow!("This account isn't syncing yet."))?;
                main.remove_shortcut(&id).await.map_err(|e| anyhow!("{e}"))?;
                self.sync_shortcuts(acct).await;
                Ok(())
            }
            Some(RootKind::Library(drive_id)) => {
                let root = root.unwrap();
                if root.has_pending_changes() {
                    bail!("Some changes in this library haven't been uploaded yet. Try again when syncing is done.");
                }
                {
                    let _guard = acct.roots_lock.lock().await;
                    self.retire_root(acct, &root).await;
                }
                self.update_account(&acct.id, |a| a.libraries.retain(|l| l.drive_id != drive_id));
                self.account_props_changed_soon(&acct.id);
                Ok(())
            }
            None => {
                // A library that failed to start: just forget it.
                let drive = key.strip_prefix("lib-").unwrap_or(key).to_string();
                self.update_account(&acct.id, |a| a.libraries.retain(|l| l.drive_id != drive));
                Ok(())
            }
        }
    }

    pub fn locations(&self, acct: &AccountRt) -> Vec<SyncLocation> {
        let mut out: Vec<SyncLocation> = acct
            .roots
            .lock()
            .unwrap()
            .iter()
            .filter(|r| !r.orphaned.load(std::sync::atomic::Ordering::Relaxed))
            .map(|r| SyncLocation {
                key: r.key.clone(),
                kind: match r.kind {
                    RootKind::Shortcut(_) => "shortcut",
                    RootKind::Library(_) => "library",
                }
                .into(),
                title: r.title.lock().unwrap().clone(),
                local_path: r.shown(&r.mount_point).to_string_lossy().into_owned(),
                state: r.engine.sync_state().as_str().to_string(),
                status_text: r.engine.status_text(),
                web_url: r.web_url.clone(),
            })
            .collect();
        // Libraries that couldn't start are still listed (so they can be removed).
        for l in acct.cfg().libraries {
            let key = format!("lib-{}", l.drive_id);
            if !out.iter().any(|o| o.key == key) {
                out.push(SyncLocation {
                    key,
                    kind: "library".into(),
                    title: format!("{} - {}", l.site, l.name),
                    local_path: l.mount_point.to_string_lossy().into_owned(),
                    state: "error".into(),
                    status_text: "Not syncing".into(),
                    web_url: l.web_url,
                });
            }
        }
        out
    }

    /// The library behind a browser "Sync" button (`odopen://sync/?…`).
    pub async fn open_sync_link(self: &Arc<Self>, url: &str) -> anyhow::Result<PathBuf> {
        let u = url::Url::parse(url).context("not a valid link")?;
        if u.scheme() != "odopen" {
            bail!("not an odopen:// link");
        }
        let q: HashMap<String, String> = u.query_pairs().into_owned().collect();
        let get = |k: &str| {
            q.iter().find(|(key, _)| key.eq_ignore_ascii_case(k)).map(|(_, v)| v.trim().to_string()).unwrap_or_default()
        };
        let (site_id, web_id, list_id) = (get("siteId"), get("webId"), get("listId"));
        let web_url = get("webUrl");
        if list_id.is_empty() || site_id.is_empty() {
            bail!("This link doesn't name a document library.");
        }
        let email = get("userEmail");
        let accounts: Vec<Arc<AccountRt>> = self.accounts.lock().unwrap().values().cloned().collect();
        let acct = accounts
            .iter()
            .find(|a| !email.is_empty() && a.cfg().email.eq_ignore_ascii_case(&email))
            .or_else(|| accounts.iter().find(|a| a.cfg().drive_type != DriveType::Personal))
            .cloned()
            .ok_or_else(|| anyhow!("Sign in with your work or school account to sync this library."))?;
        let host = url::Url::parse(&web_url).ok().and_then(|w| w.host_str().map(str::to_string)).unwrap_or_default();
        let strip = |s: &str| s.trim_matches(['{', '}']).to_string();
        let site_key = format!("{host},{},{}", strip(&site_id), strip(&web_id));
        let drive = acct.graph.list_drive(&site_key, &list_id).await.map_err(|e| anyhow!("{e}"))?;
        let title = match get("webTitle") {
            t if t.is_empty() => "SharePoint".to_string(),
            t => t,
        };
        self.add_library(&acct, &title, &drive.id).await
    }

    // ------------------------------------------------------------ routing

    /// The engine responsible for `abs` among an account's roots, and the
    /// path in that engine's mount.
    pub fn route(acct: &AccountRt, abs: &Path) -> Option<(Arc<Engine>, PathBuf)> {
        let roots = acct.roots.lock().unwrap().clone();
        for r in &roots {
            if let Some(p) = r.real(abs) {
                return Some((r.engine.clone(), p));
            }
        }
        if abs.starts_with(acct.cfg().mount_point) {
            return acct.engine().map(|e| (e, abs.to_path_buf()));
        }
        None
    }

    /// The root (if any) whose engine is `engine`.
    pub fn root_of(acct: &AccountRt, engine: &Arc<Engine>) -> Option<Arc<ExtraRoot>> {
        acct.roots.lock().unwrap().iter().find(|r| Arc::ptr_eq(&r.engine, engine)).cloned()
    }
}
