//! D-Bus server side of the contract in `od_core::dbus`.

use crate::daemon::Daemon;
use od_core::dbus::{ActivityEntry, FolderEntry, Issue, PathInfo, VersionInfo};
use od_core::types::{ActivityKind, PinState};
use od_engine::store;
use std::path::Path;
use std::sync::Arc;
use zbus::fdo;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

fn failed(e: impl std::fmt::Display) -> fdo::Error {
    fdo::Error::Failed(e.to_string())
}

pub struct ManagerIface {
    pub d: Arc<Daemon>,
}

#[zbus::interface(name = "com.airdataiot.LinuxOneDrive.Manager")]
impl ManagerIface {
    async fn begin_sign_in(&self, login_hint: String, flow: String) -> fdo::Result<(String, String)> {
        self.d.begin_sign_in(&login_hint, &flow).await.map_err(|e| failed(format!("{e:#}")))
    }

    async fn complete_sign_in(&self, redirect_url: String) -> fdo::Result<OwnedObjectPath> {
        let id = self.d.complete_sign_in(&redirect_url).await.map_err(|e| failed(format!("{e:#}")))?;
        OwnedObjectPath::try_from(od_core::dbus::account_path(&id)).map_err(failed)
    }

    async fn begin_device_sign_in(&self) -> fdo::Result<(String, String, String)> {
        self.d.begin_device_sign_in().await.map_err(|e| failed(format!("{e:#}")))
    }

    async fn cancel_sign_in(&self) {
        self.d.cancel_sign_in();
    }

    async fn pause(&self, seconds: u32) {
        self.d.pause(seconds).await;
    }

    async fn resume(&self) {
        self.d.resume().await;
    }

    async fn get_settings(&self) -> fdo::Result<String> {
        serde_json::to_string(&self.d.settings_doc()).map_err(failed)
    }

    async fn set_settings(&self, json: String) -> fdo::Result<()> {
        self.d.set_settings(&json).await.map_err(|e| failed(format!("{e:#}")))
    }

    async fn paths_info(&self, paths: Vec<String>) -> fdo::Result<String> {
        let mut out = Vec::with_capacity(paths.len());
        for p in paths {
            let abs = Path::new(&p);
            let mut info = PathInfo {
                path: p.clone(),
                account: String::new(),
                status: String::new(),
                pin: String::new(),
                shared: false,
                is_dir: false,
                error: String::new(),
            };
            if let Some((acct, engine, real)) = self.d.locate(abs) {
                info.account = acct.path();
                if let Ok(it) = engine.resolve_abs(&real) {
                    info.status = engine.item_status(&it).as_str().to_string();
                    info.pin = store::effective_pin(&engine.store.lock(), it.ino)
                        .unwrap_or(PinState::Inherit)
                        .as_str()
                        .to_string();
                    info.shared = it.shared;
                    info.is_dir = it.is_dir;
                    info.error = it.error_msg.unwrap_or_default();
                }
            }
            out.push(info);
        }
        serde_json::to_string(&out).map_err(failed)
    }

    async fn quit(&self) {
        self.d.shutdown.cancel();
    }

    async fn check_for_update(&self, force: bool) -> fdo::Result<String> {
        let info = crate::update::cached_or_check(&self.d, force).await;
        serde_json::to_string(&info).map_err(failed)
    }

    async fn download_update(&self) -> fdo::Result<String> {
        let p = crate::update::download(&self.d).await.map_err(|e| failed(format!("{e:#}")))?;
        Ok(p.to_string_lossy().into_owned())
    }

    async fn open_sync_link(&self, url: String) -> fdo::Result<String> {
        let p = self.d.open_sync_link(&url).await.map_err(|e| failed(format!("{e:#}")))?;
        Ok(p.to_string_lossy().into_owned())
    }

    #[zbus(property)]
    async fn state(&self) -> String {
        self.d.overall_state().as_str().to_string()
    }

    #[zbus(property)]
    async fn paused_until(&self) -> i64 {
        self.d.paused_until()
    }

    #[zbus(property)]
    async fn pause_reason(&self) -> String {
        self.d.pause_reason()
    }

    #[zbus(property)]
    async fn online(&self) -> bool {
        self.d.online()
    }

    #[zbus(property)]
    async fn accounts(&self) -> Vec<OwnedObjectPath> {
        self.d.account_paths()
    }

    #[zbus(property)]
    async fn version(&self) -> String {
        od_core::VERSION.to_string()
    }

    #[zbus(property)]
    async fn needs_client_id(&self) -> bool {
        self.d.cfg().client_id().is_none()
    }

    #[zbus(property)]
    async fn update_version(&self) -> String {
        match self.d.update.lock().unwrap().as_ref() {
            Some(u) if u.available => u.latest.clone(),
            _ => String::new(),
        }
    }

    #[zbus(signal)]
    pub async fn delete_reminder(emitter: &SignalEmitter<'_>, account: ObjectPath<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn sign_in_finished(
        emitter: &SignalEmitter<'_>,
        ok: bool,
        message: &str,
        account: ObjectPath<'_>,
    ) -> zbus::Result<()>;
}

pub struct AccountIface {
    pub d: Arc<Daemon>,
    pub id: String,
}

impl AccountIface {
    fn acct(&self) -> fdo::Result<Arc<crate::daemon::AccountRt>> {
        self.d.account(&self.id).ok_or_else(|| fdo::Error::UnknownObject(self.id.clone()))
    }

    fn engine(&self) -> fdo::Result<Arc<od_engine::Engine>> {
        self.acct()?.engine().ok_or_else(|| failed("This account isn't syncing yet."))
    }

    /// The engine syncing `path` (the account's folder, a shared folder
    /// through its shortcut, or a library) and the path in its mount.
    fn at(&self, path: &str) -> fdo::Result<(Arc<od_engine::Engine>, String)> {
        let acct = self.acct()?;
        match Daemon::route(&acct, Path::new(path)) {
            Some((e, p)) => Ok((e, p.to_string_lossy().into_owned())),
            None if acct.engine().is_none() => Err(failed("This account isn't syncing yet.")),
            None => Err(failed(format!("{path} isn't in a OneDrive folder."))),
        }
    }

    /// Every engine of the account: its own folder first, then shortcuts and libraries.
    fn all_engines(&self) -> fdo::Result<Vec<(Arc<od_engine::Engine>, Option<Arc<crate::roots::ExtraRoot>>)>> {
        let acct = self.acct()?;
        let mut out = vec![(self.engine()?, None)];
        for r in acct.roots.lock().unwrap().iter() {
            out.push((r.engine.clone(), Some(r.clone())));
        }
        Ok(out)
    }
}

#[zbus::interface(name = "com.airdataiot.LinuxOneDrive.Account")]
impl AccountIface {
    async fn set_pin_state(&self, paths: Vec<String>, state: String) -> fdo::Result<()> {
        let pin: PinState = state.parse().map_err(failed)?;
        for p in paths {
            let (e, p) = self.at(&p)?;
            let it = e.resolve_abs(Path::new(&p)).map_err(failed)?;
            e.set_pin_state(it.ino, pin).map_err(failed)?;
        }
        Ok(())
    }

    async fn create_share_link(&self, path: String, kind: String, scope: String) -> fdo::Result<String> {
        let (e, path) = self.at(&path)?;
        let id = e.item_id_of(Path::new(&path)).map_err(failed)?;
        let url = e.graph.create_link(&id, &kind, &scope).await.map_err(|err| failed(err.user_message()))?;
        e.refresh();
        Ok(url)
    }

    async fn get_sharing_info(&self, path: String) -> fdo::Result<String> {
        let a = self.acct()?;
        let (e, path) = self.at(&path)?;
        let info = crate::sharing::info(&self.d, &a, &e, &path).map_err(|e| failed(format!("{e:#}")))?;
        serde_json::to_string(&info).map_err(failed)
    }

    async fn list_access(&self, path: String) -> fdo::Result<String> {
        let (e, path) = self.at(&path)?;
        let list = crate::sharing::list_access(&e, &path).await.map_err(|e| failed(format!("{e:#}")))?;
        serde_json::to_string(&list).map_err(failed)
    }

    async fn create_link(&self, path: String, options: String) -> fdo::Result<String> {
        let a = self.acct()?;
        let (e, path) = self.at(&path)?;
        let o: od_core::dbus::LinkOptions =
            serde_json::from_str(&options).map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        let r = crate::sharing::create_link(&a, &e, &path, &o).await.map_err(|e| failed(format!("{e:#}")))?;
        serde_json::to_string(&r).map_err(failed)
    }

    async fn copy_link(&self, path: String) -> fdo::Result<String> {
        let a = self.acct()?;
        let (e, path) = self.at(&path)?;
        crate::sharing::copy_link(&self.d, &a, &e, &path).await.map_err(|e| failed(format!("{e:#}")))
    }

    async fn invite(&self, path: String, options: String) -> fdo::Result<String> {
        let (e, path) = self.at(&path)?;
        let o: od_core::dbus::InviteOptions =
            serde_json::from_str(&options).map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        let r = crate::sharing::invite(&e, &path, &o).await.map_err(|e| failed(format!("{e:#}")))?;
        serde_json::to_string(&r).map_err(failed)
    }

    async fn set_access_role(&self, path: String, id: String, role: String) -> fdo::Result<()> {
        let (e, path) = self.at(&path)?;
        crate::sharing::set_role(&e, &path, &id, &role).await.map_err(|e| failed(format!("{e:#}")))
    }

    async fn remove_access(&self, path: String, id: String) -> fdo::Result<()> {
        let (e, path) = self.at(&path)?;
        crate::sharing::remove(&e, &path, &id).await.map_err(|e| failed(format!("{e:#}")))
    }

    async fn remove_link_people(&self, path: String, id: String, emails: Vec<String>) -> fdo::Result<()> {
        let (e, path) = self.at(&path)?;
        crate::sharing::remove_people(&e, &path, &id, emails).await.map_err(|e| failed(format!("{e:#}")))
    }

    async fn stop_sharing(&self, path: String) -> fdo::Result<String> {
        let (e, path) = self.at(&path)?;
        let r = crate::sharing::stop_sharing(&e, &path).await.map_err(|e| failed(format!("{e:#}")))?;
        serde_json::to_string(&r).map_err(failed)
    }

    async fn search_people(&self, query: String) -> fdo::Result<String> {
        let (a, e) = (self.acct()?, self.engine()?);
        serde_json::to_string(&crate::sharing::search_people(&a, &e, &query).await).map_err(failed)
    }

    async fn begin_people_consent(&self) -> fdo::Result<(String, String)> {
        let a = self.acct()?;
        crate::sharing::begin_people_consent(&self.d, &a).await.map_err(|e| failed(format!("{e:#}")))
    }

    async fn web_url(&self, path: String) -> fdo::Result<String> {
        let (e, path) = self.at(&path)?;
        let it = e.resolve_abs(Path::new(&path)).map_err(failed)?;
        if let Some(u) = it.web_url.filter(|u| !u.is_empty()) {
            return Ok(u);
        }
        let id = it.item_id.ok_or_else(|| failed("This item hasn't been uploaded yet."))?;
        let item = e.graph.item(&id).await.map_err(|err| failed(err.user_message()))?;
        item.web_url.ok_or_else(|| failed("No web address for this item."))
    }

    async fn list_versions(&self, path: String) -> fdo::Result<String> {
        let (e, path) = self.at(&path)?;
        let id = e.item_id_of(Path::new(&path)).map_err(failed)?;
        let versions = e.graph.versions(&id).await.map_err(|err| failed(err.user_message()))?;
        let out: Vec<VersionInfo> = versions
            .iter()
            .enumerate()
            .map(|(i, v)| VersionInfo {
                id: v.id.clone(),
                modified: v.last_modified_date_time.map(|t| t.timestamp()).unwrap_or(0),
                size: v.size.unwrap_or(0).max(0) as u64,
                modified_by: v.last_modified_by.as_ref().map(|b| b.display_name()).unwrap_or_default(),
                current: i == 0,
            })
            .collect();
        serde_json::to_string(&out).map_err(failed)
    }

    async fn restore_version(&self, path: String, version_id: String) -> fdo::Result<()> {
        let (e, path) = self.at(&path)?;
        let id = e.item_id_of(Path::new(&path)).map_err(failed)?;
        e.graph.restore_version(&id, &version_id).await.map_err(|err| failed(err.user_message()))?;
        e.refresh();
        Ok(())
    }

    async fn download_version(&self, path: String, version_id: String, destination: String) -> fdo::Result<()> {
        let (e, path) = self.at(&path)?;
        let id = e.item_id_of(Path::new(&path)).map_err(failed)?;
        let cancel = tokio_util::sync::CancellationToken::new();
        e.graph
            .download_version_to(&id, &version_id, Path::new(&destination), &cancel)
            .await
            .map_err(|err| failed(err.user_message()))?;
        Ok(())
    }

    async fn recent_activity(&self, limit: u32) -> fdo::Result<String> {
        let limit = limit.clamp(1, 500);
        let mut out: Vec<ActivityEntry> = Vec::new();
        for (e, root) in self.all_engines()? {
            let rows = store::recent_activity(&e.store.lock(), limit).map_err(failed)?;
            let offset = root.as_ref().map(|r| crate::roots::activity_id_offset(&r.key)).unwrap_or(0);
            out.extend(rows.into_iter().map(|r| ActivityEntry {
                id: r.id + offset,
                time: r.time,
                kind: r.kind.parse().unwrap_or(ActivityKind::Error),
                name: r.name,
                path: shown(&root, &r.path),
                folder: match &root {
                    Some(rt) if r.folder == "OneDrive" => rt.title.lock().unwrap().clone(),
                    _ => r.folder,
                },
                size: r.size,
                detail: r.detail,
                percent: -1,
            }));
        }
        out.sort_by(|a, b| b.time.cmp(&a.time));
        out.truncate(limit as usize);
        serde_json::to_string(&out).map_err(failed)
    }

    async fn list_issues(&self) -> fdo::Result<String> {
        let mut out: Vec<Issue> = Vec::new();
        for (e, root) in self.all_engines()? {
            let rows = store::issues(&e.store.lock()).map_err(failed)?;
            out.extend(rows.into_iter().map(|it| Issue {
                path: shown(&root, &e.local_path(it.ino).to_string_lossy()),
                name: it.name.clone(),
                code: it.error_code.clone().unwrap_or_default(),
                message: it.error_msg.clone().unwrap_or_default(),
                since: it.retry_at / 1000,
            }));
        }
        serde_json::to_string(&out).map_err(failed)
    }

    async fn retry_failed(&self) -> fdo::Result<()> {
        for (e, _) in self.all_engines()? {
            e.retry_failed().map_err(failed)?;
        }
        Ok(())
    }

    async fn list_locations(&self) -> fdo::Result<String> {
        let a = self.acct()?;
        serde_json::to_string(&self.d.locations(&a)).map_err(failed)
    }

    async fn search_sites(&self, query: String) -> fdo::Result<String> {
        let a = self.acct()?;
        let sites = self.d.search_sites(&a, &query).await.map_err(|e| failed(format!("{e:#}")))?;
        serde_json::to_string(&sites).map_err(failed)
    }

    async fn site_libraries(&self, site_id: String) -> fdo::Result<String> {
        let a = self.acct()?;
        let libs = self.d.site_libraries(&a, &site_id).await.map_err(|e| failed(format!("{e:#}")))?;
        serde_json::to_string(&libs).map_err(failed)
    }

    async fn add_library(&self, site_id: String, drive_id: String) -> fdo::Result<String> {
        let a = self.acct()?;
        let p = self.d.add_library_from_site(&a, &site_id, &drive_id).await.map_err(|e| failed(format!("{e:#}")))?;
        Ok(p.to_string_lossy().into_owned())
    }

    async fn remove_location(&self, key: String) -> fdo::Result<()> {
        let a = self.acct()?;
        self.d.remove_location(&a, &key).await.map_err(|e| failed(format!("{e:#}")))
    }

    async fn resolve_mass_delete(&self, delete_online: bool) -> fdo::Result<()> {
        for (e, _) in self.all_engines()? {
            if e.held_deletes_count() > 0 {
                e.resolve_mass_delete(delete_online).map_err(failed)?;
            }
        }
        Ok(())
    }

    async fn set_mount_point(&self, path: String) -> fdo::Result<()> {
        let acct = self.acct()?;
        let new = std::path::PathBuf::from(&path);
        if !new.is_absolute() {
            return Err(fdo::Error::InvalidArgs("the folder must be an absolute path".into()));
        }
        let was_running = acct.engine().is_some();
        self.d.stop_account(&acct).await;
        let old = acct.cfg().mount_point;
        self.d.update_account(&self.id, |a| a.mount_point = new.clone());
        if old != new {
            let _ = std::fs::remove_dir(&old);
        }
        if was_running {
            self.d.start_account(&acct).await.map_err(|e| failed(format!("{e:#}")))?;
        }
        Ok(())
    }

    async fn set_download_all(&self, enabled: bool) -> fdo::Result<()> {
        self.d.update_account(&self.id, |a| a.download_all = enabled);
        if let Some(e) = self.acct()?.engine() {
            e.update_cfg(|c| c.download_all = enabled);
            let pin = if enabled { PinState::AlwaysKeep } else { PinState::Inherit };
            e.set_pin_state(store::ROOT_INO, pin).map_err(failed)?;
        }
        Ok(())
    }

    async fn set_folder_excluded(&self, remote_path: String, excluded: bool) -> fdo::Result<()> {
        let name = remote_path.trim_matches('/').to_string();
        self.engine()?.set_folder_hidden(&name, excluded).map_err(failed)?;
        self.d.update_account(&self.id, |a| {
            a.excluded_folders.retain(|f| !f.eq_ignore_ascii_case(&name));
            if excluded {
                a.excluded_folders.push(name.clone());
            }
        });
        Ok(())
    }

    async fn list_folders(&self, _remote_path: String) -> fdo::Result<String> {
        let e = self.engine()?;
        let out: Vec<FolderEntry> = e
            .top_level_folders()
            .map_err(failed)?
            .into_iter()
            .map(|(name, excluded)| FolderEntry { path: format!("/{name}"), name, size: 0, excluded })
            .collect();
        serde_json::to_string(&out).map_err(failed)
    }

    async fn start(&self) -> fdo::Result<()> {
        let acct = self.acct()?;
        self.d.update_account(&self.id, |a| a.setup_complete = true);
        self.d.start_account(&acct).await.map_err(|e| failed(format!("{e:#}")))
    }

    async fn sign_out(&self) -> fdo::Result<()> {
        self.d.sign_out(&self.id).await.map_err(|e| failed(format!("{e:#}")))
    }

    async fn refresh(&self) -> fdo::Result<()> {
        for (e, _) in self.all_engines()? {
            e.refresh();
        }
        Ok(())
    }

    async fn list_removed(&self) -> fdo::Result<String> {
        let e = self.engine()?;
        let out: Vec<od_core::dbus::RemovedItem> = e
            .removed_items()
            .into_iter()
            .map(|t| od_core::dbus::RemovedItem {
                id: t.id,
                name: t.name,
                rel_path: t.rel_path,
                removed_at: t.removed_at,
                size: t.size,
            })
            .collect();
        serde_json::to_string(&out).map_err(failed)
    }

    async fn list_backup_folders(&self) -> fdo::Result<String> {
        let (d, id) = (self.d.clone(), self.id.clone());
        let list = tokio::task::spawn_blocking(move || d.backup_folders(&id)).await.map_err(failed)?;
        serde_json::to_string(&list).map_err(failed)
    }

    async fn set_folder_backup(&self, key: String, enabled: bool) -> fdo::Result<()> {
        self.d.set_folder_backup(&self.id, &key, enabled).map_err(|e| failed(format!("{e:#}")))
    }

    async fn restore_removed(&self, id: String) -> fdo::Result<String> {
        let e = self.engine()?;
        let p = tokio::task::spawn_blocking(move || e.restore_removed(&id)).await.map_err(failed)?.map_err(failed)?;
        Ok(p.to_string_lossy().into_owned())
    }

    #[zbus(property)]
    async fn id(&self) -> String {
        self.id.clone()
    }

    #[zbus(property)]
    async fn email(&self) -> String {
        self.acct().map(|a| a.cfg().email).unwrap_or_default()
    }

    #[zbus(property)]
    async fn display_name(&self) -> String {
        self.acct().map(|a| a.cfg().display_name).unwrap_or_default()
    }

    #[zbus(property)]
    async fn organization(&self) -> String {
        self.acct().map(|a| a.cfg().organization).unwrap_or_default()
    }

    #[zbus(property)]
    async fn drive_type(&self) -> String {
        self.acct().map(|a| a.cfg().drive_type.as_str().to_string()).unwrap_or_default()
    }

    #[zbus(property)]
    async fn mount_point(&self) -> String {
        self.acct().map(|a| a.cfg().mount_point.to_string_lossy().into_owned()).unwrap_or_default()
    }

    #[zbus(property)]
    async fn web_root_url(&self) -> String {
        self.acct()
            .map(|a| a.cfg().web_url)
            .ok()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| "https://onedrive.live.com/".into())
    }

    #[zbus(property)]
    async fn state(&self) -> String {
        self.acct().map(|a| a.state().as_str().to_string()).unwrap_or_default()
    }

    #[zbus(property)]
    async fn status_text(&self) -> String {
        let Ok(acct) = self.acct() else { return String::new() };
        if !acct.cfg().setup_complete {
            return "Finish setting up your OneDrive".into();
        }
        if let Some(err) = acct.error.lock().unwrap().clone() {
            return err;
        }
        let Some(main) = acct.engine() else { return "Starting…".into() };
        // The busiest part of the account speaks: the user's own folder, or
        // a shared folder or library (by name), so the text matches the state.
        let mut best = (main.sync_state(), main.status_text());
        for r in acct.roots.lock().unwrap().iter() {
            let s = r.engine.sync_state();
            if s.severity() > best.0.severity() {
                best = (s, format!("{}: {}", r.title.lock().unwrap(), r.engine.status_text()));
            }
        }
        best.1
    }

    #[zbus(property)]
    async fn quota_used(&self) -> u64 {
        self.engine().ok().and_then(|e| e.quota()).and_then(|q| q.used).unwrap_or(0).max(0) as u64
    }

    #[zbus(property)]
    async fn quota_total(&self) -> u64 {
        self.engine().ok().and_then(|e| e.quota()).and_then(|q| q.total).unwrap_or(0).max(0) as u64
    }

    #[zbus(property)]
    async fn quota_state(&self) -> String {
        self.engine().ok().and_then(|e| e.quota()).and_then(|q| q.state).unwrap_or_else(|| "normal".into())
    }

    #[zbus(property)]
    async fn issue_count(&self) -> u32 {
        let engines = self.all_engines().unwrap_or_default();
        engines.iter().map(|(e, _)| store::issue_count(&e.store.lock()).unwrap_or(0)).sum()
    }

    #[zbus(property)]
    async fn progress(&self) -> String {
        let mut p = od_core::dbus::Progress::default();
        for (e, _) in self.all_engines().unwrap_or_default() {
            let q = e.progress();
            p.uploads_pending += q.uploads_pending;
            p.downloads_pending += q.downloads_pending;
            p.bytes_done += q.bytes_done;
            p.bytes_total += q.bytes_total;
            if p.current.is_empty() {
                p.current = q.current;
            }
        }
        serde_json::to_string(&p).unwrap_or_default()
    }

    #[zbus(property)]
    async fn needs_setup(&self) -> bool {
        self.acct().map(|a| !a.cfg().setup_complete).unwrap_or(false)
    }

    #[zbus(property)]
    async fn download_all(&self) -> bool {
        self.acct().map(|a| a.cfg().download_all).unwrap_or(false)
    }

    #[zbus(property)]
    async fn held_deletes(&self) -> u32 {
        self.all_engines().unwrap_or_default().iter().map(|(e, _)| e.held_deletes_count()).sum()
    }

    #[zbus(signal)]
    pub async fn activity_added(emitter: &SignalEmitter<'_>, json: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn items_changed(emitter: &SignalEmitter<'_>, paths: Vec<String>) -> zbus::Result<()>;
}

/// A path recorded by a shortcut's engine, as the user sees it (through the link).
fn shown(root: &Option<Arc<crate::roots::ExtraRoot>>, p: &str) -> String {
    match root {
        Some(r) if !p.is_empty() => r.shown(Path::new(p)).to_string_lossy().into_owned(),
        _ => p.to_string(),
    }
}
