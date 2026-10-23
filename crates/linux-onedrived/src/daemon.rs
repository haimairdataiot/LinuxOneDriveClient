//! The daemon: owns every account (engine + FUSE mount), the sign-in flows,
//! pause state, environment conditions and settings, and forwards engine
//! events to D-Bus and desktop notifications.

use crate::conditions::Conditions;
use crate::dbus_iface::{AccountIface, ManagerIface};
use crate::notify::{Action, Notifier};
use crate::{mounts, secrets};
use anyhow::{Context, anyhow};
use od_core::config::{AccountConfig, Config};
use od_core::dbus::{self as api, SettingsDoc};
use od_core::names::IgnoreSet;
use od_core::types::{DriveType, SyncState};
use od_engine::engine::{EngineEvent, NoticeKind, pause};
use od_engine::{Engine, EngineConfig};
use od_graph::auth::{self, OAuthConfig, Pkce, TokenSet};
use od_graph::client::{build_http_client, graph_base_from_env};
use od_graph::{GraphClient, TokenManager};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

/// One linked account at runtime.
pub struct AccountRt {
    pub id: String,
    pub cfg: Mutex<AccountConfig>,
    pub tokens: Arc<TokenManager>,
    pub graph: GraphClient,
    pub engine: Mutex<Option<Arc<Engine>>>,
    pub session: Mutex<Option<fuser::BackgroundSession>>,
    pub error: Mutex<Option<String>>,
    /// Shortcuts to shared folders and SharePoint libraries being synced.
    pub roots: Mutex<Vec<Arc<crate::roots::ExtraRoot>>>,
    /// Serialises starting and stopping roots.
    pub roots_lock: tokio::sync::Mutex<()>,
}

impl AccountRt {
    pub fn engine(&self) -> Option<Arc<Engine>> {
        self.engine.lock().unwrap().clone()
    }
    pub fn cfg(&self) -> AccountConfig {
        self.cfg.lock().unwrap().clone()
    }
    pub fn path(&self) -> String {
        api::account_path(&self.id)
    }
    pub fn state(&self) -> SyncState {
        if !self.cfg().setup_complete {
            return SyncState::Starting;
        }
        if self.error.lock().unwrap().is_some() {
            return SyncState::Error;
        }
        let Some(main) = self.engine() else { return SyncState::Starting };
        // The most severe state of the account's folder and its shortcuts and libraries.
        let roots: Vec<_> = self.roots.lock().unwrap().iter().map(|r| r.engine.sync_state()).collect();
        roots.into_iter().fold(main.sync_state(), |a, b| if b.severity() > a.severity() { b } else { a })
    }
}

struct PendingSignIn {
    oauth: OAuthConfig,
    pkce: Pkce,
    state: String,
    redirect: String,
    /// Incremental consent for an existing account: (account id, scopes granted).
    consent: Option<(String, Vec<String>)>,
}

/// Scopes requested for an account (defaults + incrementally granted ones).
pub fn scopes_for(extra: &[String]) -> String {
    let mut s = auth::DEFAULT_SCOPES.to_string();
    for e in extra {
        if !s.split(' ').any(|x| x == e) {
            s.push(' ');
            s.push_str(e);
        }
    }
    s
}

pub struct Daemon {
    pub conn: zbus::Connection,
    pub rt: tokio::runtime::Handle,
    pub http: reqwest::Client,
    pub config: Mutex<Config>,
    pub accounts: Mutex<BTreeMap<String, Arc<AccountRt>>>,
    pending: Mutex<Option<PendingSignIn>>,
    signin_task: Mutex<Option<CancellationToken>>,
    /// Sign-ins finish one at a time (see `finish_tokens`).
    finishing: tokio::sync::Mutex<()>,
    /// 0 = not paused, -1 = until resumed, else unix seconds.
    paused_until: Mutex<i64>,
    pause_timer: Mutex<Option<CancellationToken>>,
    conditions: Mutex<Conditions>,
    /// Folder backups being started or stopped: (account id, XDG key).
    backup_jobs: Mutex<std::collections::HashSet<(String, String)>>,
    pub notifier: Arc<Notifier>,
    pub shutdown: CancellationToken,
    /// Result of the last update check.
    pub update: Mutex<Option<od_core::update::UpdateInfo>>,
}

impl Daemon {
    pub async fn new(conn: zbus::Connection) -> anyhow::Result<Arc<Daemon>> {
        let config = Config::load().context("reading the configuration")?;
        let notifier = Arc::new(Notifier::new(&conn).await);
        Ok(Arc::new(Daemon {
            conn,
            rt: tokio::runtime::Handle::current(),
            http: build_http_client(),
            config: Mutex::new(config),
            accounts: Mutex::new(BTreeMap::new()),
            pending: Mutex::new(None),
            signin_task: Mutex::new(None),
            finishing: tokio::sync::Mutex::new(()),
            paused_until: Mutex::new(0),
            pause_timer: Mutex::new(None),
            conditions: Mutex::new(Conditions::default()),
            backup_jobs: Mutex::new(std::collections::HashSet::new()),
            notifier,
            shutdown: CancellationToken::new(),
            update: Mutex::new(None),
        }))
    }

    pub fn cfg(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    fn save_config(&self) {
        if let Err(e) = self.config.lock().unwrap().save() {
            tracing::error!("cannot save the configuration: {e}");
        }
    }

    /// Register D-Bus objects, start every configured account and the watchers.
    pub async fn start(self: &Arc<Self>) -> anyhow::Result<()> {
        self.conn.object_server().at(api::MANAGER_PATH, ManagerIface { d: self.clone() }).await?;
        let accounts = self.cfg().accounts;
        // Versions before 0.3.1 could add an account twice when two sign-ins
        // finished together; say so, since syncing one OneDrive into two
        // folders is never what was meant.
        for (i, a) in accounts.iter().enumerate() {
            let id = od_graph::client::normalize_drive_id(&a.drive_id);
            if let Some(b) = accounts[..i].iter().find(|b| same_account(b, &id, &a.email, a.drive_type)) {
                tracing::warn!(
                    "{} is set up twice ({} and {}); unlink one of them",
                    a.email,
                    b.mount_point.display(),
                    a.mount_point.display()
                );
            }
        }
        for a in accounts {
            if let Err(e) = self.add_account_runtime(a.clone()).await {
                tracing::error!(account = %a.id, "cannot load account: {e:#}");
            }
        }
        for acct in self.accounts.lock().unwrap().values().cloned().collect::<Vec<_>>() {
            if acct.cfg().setup_complete && !acct.cfg().disabled {
                let me = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = me.start_account(&acct).await {
                        tracing::error!(account = %acct.id, "cannot start account: {e:#}");
                        *acct.error.lock().unwrap() = Some(format!("{e:#}"));
                        me.state_changed().await;
                    }
                });
            }
        }
        tokio::spawn(self.notifier.clone().listen());
        let me = self.clone();
        tokio::spawn(crate::conditions::watch(move |c| me.apply_conditions(c)));
        tokio::spawn(self.clone().mount_watchdog());
        tokio::spawn(crate::update::watch(self.clone()));
        Ok(())
    }

    // ------------------------------------------------------------ accounts

    async fn add_account_runtime(self: &Arc<Self>, a: AccountConfig) -> anyhow::Result<Arc<AccountRt>> {
        let client_id = self.cfg().client_id().unwrap_or_default();
        let tokens = secrets::load(&a.id).await?.unwrap_or_else(|| TokenSet {
            access_token: String::new(),
            refresh_token: String::new(),
            expires_at: chrono::Utc::now(),
            scope: String::new(),
            claims: None,
        });
        let mut oauth = OAuthConfig::new(client_id);
        oauth.tenant = a.tenant.clone();
        oauth.scopes = scopes_for(&a.extra_scopes);
        let tm = Arc::new(TokenManager::new(
            self.http.clone(),
            oauth,
            tokens,
            secrets::persist_hook(a.id.clone(), a.email.clone()),
        ));
        let graph = GraphClient::new(self.http.clone(), graph_base_from_env(), tm.clone()).with_drive(&a.drive_id);
        let net = self.cfg().network;
        graph.upload_limiter.set_limit(net.upload_limit_kbps as u64 * 1024);
        graph.download_limiter.set_limit(net.download_limit_kbps as u64 * 1024);
        let rt = Arc::new(AccountRt {
            id: a.id.clone(),
            cfg: Mutex::new(a),
            tokens: tm,
            graph,
            engine: Mutex::new(None),
            session: Mutex::new(None),
            error: Mutex::new(None),
            roots: Mutex::new(Vec::new()),
            roots_lock: tokio::sync::Mutex::new(()),
        });
        self.conn.object_server().at(rt.path(), AccountIface { d: self.clone(), id: rt.id.clone() }).await?;
        self.accounts.lock().unwrap().insert(rt.id.clone(), rt.clone());
        self.accounts_changed().await;
        Ok(rt)
    }

    pub fn account(&self, id: &str) -> Option<Arc<AccountRt>> {
        self.accounts.lock().unwrap().get(id).cloned()
    }

    pub(crate) fn engine_config(&self, a: &AccountConfig) -> EngineConfig {
        let c = self.cfg();
        let mut ec =
            EngineConfig::new(&a.id, &od_core::paths::account_dir(&a.id), &a.mount_point, &a.drive_id, a.drive_type);
        ec.ignore = IgnoreSet::new(&c.general.excluded_patterns);
        ec.excluded_folders = a.excluded_folders.clone();
        ec.mass_delete_threshold = c.general.mass_delete_threshold;
        ec.poll_interval = Duration::from_secs(c.general.poll_interval_secs.max(15));
        ec.max_parallel_uploads = c.network.max_parallel_uploads.clamp(1, 16) as usize;
        ec.max_parallel_downloads = c.network.max_parallel_downloads.clamp(1, 16) as usize;
        ec.block_background_hydration = c.general.block_background_hydration;
        ec.download_all = a.download_all;
        ec.auto_free_up_days = c.general.auto_free_up_days;
        ec.server_thumbnails = c.general.server_thumbnails;
        ec.push_notifications = c.network.push_notifications;
        ec
    }

    /// Mount and start syncing an account.
    pub async fn start_account(self: &Arc<Self>, acct: &Arc<AccountRt>) -> anyhow::Result<()> {
        if acct.engine().is_some() {
            return Ok(());
        }
        let a = acct.cfg();
        let account_dir = od_core::paths::account_dir(&a.id);
        od_core::paths::ensure_private_dir(&account_dir)?;
        let import = mounts::prepare(&a.mount_point, &account_dir)?;
        let engine = {
            let ec = self.engine_config(&a);
            let graph = acct.graph.clone();
            tokio::task::spawn_blocking(move || Engine::new(ec, graph)).await??
        };
        self.apply_pause_to(&engine);
        let session = od_engine::vfs::mount(engine.clone(), self.rt.clone())
            .with_context(|| format!("mounting {}", a.mount_point.display()))?;
        *acct.session.lock().unwrap() = Some(session);
        *acct.engine.lock().unwrap() = Some(engine.clone());
        *acct.error.lock().unwrap() = None;
        engine.start();
        tokio::spawn(self.clone().forward_events(acct.clone(), engine.clone()));
        {
            let (me, a) = (self.clone(), acct.clone());
            tokio::spawn(async move {
                me.sync_shortcuts(&a).await;
                me.start_libraries(&a).await;
            });
        }
        if let Some(dir) = import {
            let mp = a.mount_point.clone();
            let me = self.clone();
            tokio::task::spawn_blocking(move || match mounts::import_into(&dir, &mp) {
                Ok(n) if n > 0 => me.rt.block_on(me.notifier.show(
                    None,
                    "Files added to OneDrive",
                    &format!("{n} file(s) found in your OneDrive folder while syncing was off are being uploaded."),
                    vec![("default", "Open folder", Action::Open(mp.clone()))],
                    false,
                )),
                Ok(_) => {}
                Err(e) => tracing::error!("importing {} failed: {e}; the files are kept there", dir.display()),
            });
        }
        tracing::info!(account = %a.id, mount = %a.mount_point.display(), "account started");
        self.state_changed().await;
        Ok(())
    }

    /// Unmount and stop an account.
    pub async fn stop_account(&self, acct: &Arc<AccountRt>) {
        {
            let _guard = acct.roots_lock.lock().await;
            self.stop_roots(acct).await;
        }
        if let Some(e) = acct.engine.lock().unwrap().take() {
            e.stop();
            e.set_notifier(None);
        }
        let session = acct.session.lock().unwrap().take();
        if let Some(s) = session {
            mounts::unmount_session(s, acct.cfg().mount_point).await;
        }
    }

    pub async fn stop_all(&self) {
        let all: Vec<_> = self.accounts.lock().unwrap().values().cloned().collect();
        for a in all {
            self.stop_account(&a).await;
        }
    }

    /// Remount if the mount disappeared (e.g. "Eject" in the file manager).
    async fn mount_watchdog(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            }
            let all: Vec<_> = self.accounts.lock().unwrap().values().cloned().collect();
            for acct in all {
                self.retire_orphans(&acct).await;
                let roots: Vec<_> = acct.roots.lock().unwrap().clone();
                for r in roots {
                    if r.session.lock().unwrap().is_none() || mounts::is_our_mount(&r.mount_point) {
                        continue;
                    }
                    tracing::warn!(mount = %r.mount_point.display(), "a shared folder or library was unmounted; mounting it again");
                    let _ = r.session.lock().unwrap().take();
                    r.engine.set_notifier(None);
                    if mounts::prepare(&r.mount_point, &od_core::paths::account_dir(&acct.id)).is_ok() {
                        match od_engine::vfs::mount(r.engine.clone(), self.rt.clone()) {
                            Ok(s) => *r.session.lock().unwrap() = Some(s),
                            Err(e) => tracing::error!("remount failed: {e}"),
                        }
                    }
                }
                let Some(engine) = acct.engine() else { continue };
                let mp = acct.cfg().mount_point;
                if mounts::is_our_mount(&mp) {
                    continue;
                }
                tracing::warn!(mount = %mp.display(), "the OneDrive folder was unmounted; mounting it again");
                let _ = acct.session.lock().unwrap().take();
                engine.set_notifier(None);
                if let Err(e) = mounts::prepare(&mp, &od_core::paths::account_dir(&acct.id)) {
                    tracing::error!("cannot prepare {}: {e}", mp.display());
                    continue;
                }
                match od_engine::vfs::mount(engine.clone(), self.rt.clone()) {
                    Ok(s) => *acct.session.lock().unwrap() = Some(s),
                    Err(e) => tracing::error!("remount failed: {e}"),
                }
            }
        }
    }

    // ------------------------------------------------------------ sign-in

    fn oauth(&self) -> anyhow::Result<OAuthConfig> {
        let id = self
            .cfg()
            .client_id()
            .ok_or_else(|| anyhow!("No Microsoft application ID is configured. Set client_id under [general] in ~/.config/linux-onedrive/config.toml."))?;
        Ok(OAuthConfig::new(id))
    }

    /// Start an interactive sign-in; returns (authorize URL, redirect URI).
    pub async fn begin_sign_in(self: &Arc<Self>, login_hint: &str, flow: &str) -> anyhow::Result<(String, String)> {
        let oauth = self.oauth()?;
        let pkce = Pkce::generate();
        let state = auth::random_urlsafe(32);
        let hint = Some(login_hint).filter(|h| !h.is_empty());
        if flow == "browser" {
            let server = auth::LoopbackServer::bind().await?;
            let redirect = server.redirect_uri();
            let url = auth::authorize_url(&oauth, &redirect, &state, &pkce, hint);
            let cancel = self.replace_signin_task();
            let me = self.clone();
            let redirect2 = redirect.clone();
            tokio::spawn(async move {
                let wait = server.wait_for_code(&state, Duration::from_secs(600));
                let result = tokio::select! {
                    _ = cancel.cancelled() => return,
                    r = wait => r,
                };
                let outcome = match result {
                    Ok(code) => me.finish_code(&oauth, &code, &redirect2, &pkce).await,
                    Err(e) => Err(e.into()),
                };
                me.emit_sign_in_finished(outcome).await;
            });
            return Ok((url, redirect));
        }
        let redirect = auth::native_client_redirect(&oauth);
        let url = auth::authorize_url(&oauth, &redirect, &state, &pkce, hint);
        *self.pending.lock().unwrap() =
            Some(PendingSignIn { oauth, pkce, state, redirect: redirect.clone(), consent: None });
        Ok((url, redirect))
    }

    /// Embedded sign-in asking an existing account for extra scopes.
    pub async fn begin_consent(
        self: &Arc<Self>,
        account_id: &str,
        email: &str,
        scopes: &[&str],
    ) -> anyhow::Result<(String, String)> {
        let acct = self.account(account_id).ok_or_else(|| anyhow!("unknown account"))?;
        let mut oauth = self.oauth()?;
        let mut extra = acct.cfg().extra_scopes;
        for s in scopes {
            if !extra.iter().any(|e| e == s) {
                extra.push(s.to_string());
            }
        }
        oauth.scopes = scopes_for(&extra);
        let pkce = Pkce::generate();
        let state = auth::random_urlsafe(32);
        let redirect = auth::native_client_redirect(&oauth);
        let url = auth::authorize_url(&oauth, &redirect, &state, &pkce, Some(email));
        *self.pending.lock().unwrap() = Some(PendingSignIn {
            oauth,
            pkce,
            state,
            redirect: redirect.clone(),
            consent: Some((account_id.to_string(), extra)),
        });
        Ok((url, redirect))
    }

    /// Finish the embedded flow with the redirect URL the web view intercepted.
    pub async fn complete_sign_in(self: &Arc<Self>, redirect_url: &str) -> anyhow::Result<String> {
        let p = self.pending.lock().unwrap().take().ok_or_else(|| anyhow!("no sign-in in progress"))?;
        let code = auth::code_from_redirect(redirect_url, &p.state)?;
        let id = self.finish_code(&p.oauth, &code, &p.redirect, &p.pkce).await?;
        if let Some((acct_id, extra)) = p.consent {
            if acct_id == id {
                if let Some(acct) = self.account(&id) {
                    acct.tokens.set_scopes(&p.oauth.scopes);
                }
                self.update_account(&id, |a| a.extra_scopes = extra);
            }
        }
        Ok(id)
    }

    pub async fn begin_device_sign_in(self: &Arc<Self>) -> anyhow::Result<(String, String, String)> {
        // Sign-in with a code is the usual route for phishing under an app's
        // name, so the project's registration doesn't offer it (it can keep
        // "Allow public client flows" off). It works with your own app ID.
        if self.cfg().uses_project_client_id() {
            anyhow::bail!(
                "Sign-in with a code isn't available with the built-in app registration. Run `linux-onedrivectl login` \
                 to sign in with your browser, or set your own application ID (client_id in ~/.config/linux-onedrive/config.toml)."
            );
        }
        let mut oauth = self.oauth()?;
        // Microsoft blocks device code for personal accounts on unverified apps.
        oauth.tenant = "organizations".into();
        let dc = auth::start_device_flow(&self.http, &oauth).await?;
        let cancel = self.replace_signin_task();
        let me = self.clone();
        let dc2 = dc.clone();
        tokio::spawn(async move {
            let poll = auth::poll_device_flow(&me.http, &oauth, &dc2);
            let result = tokio::select! {
                _ = cancel.cancelled() => return,
                r = poll => r,
            };
            let outcome = match result {
                Ok(tokens) => me.finish_tokens(tokens, "organizations").await,
                Err(e) => Err(e.into()),
            };
            me.emit_sign_in_finished(outcome).await;
        });
        Ok((dc.user_code, dc.verification_uri, dc.message))
    }

    pub fn cancel_sign_in(&self) {
        *self.pending.lock().unwrap() = None;
        if let Some(c) = self.signin_task.lock().unwrap().take() {
            c.cancel();
        }
    }

    fn replace_signin_task(&self) -> CancellationToken {
        let t = CancellationToken::new();
        if let Some(old) = self.signin_task.lock().unwrap().replace(t.clone()) {
            old.cancel();
        }
        t
    }

    async fn emit_sign_in_finished(&self, outcome: anyhow::Result<String>) {
        let (ok, msg, path) = match outcome {
            Ok(id) => (true, String::new(), api::account_path(&id)),
            Err(e) => {
                tracing::warn!("sign-in failed: {e:#}");
                (false, format!("{e:#}"), api::MANAGER_PATH.to_string())
            }
        };
        if let Ok(iface) = self.conn.object_server().interface::<_, ManagerIface>(api::MANAGER_PATH).await {
            let path =
                ObjectPath::try_from(path.as_str()).unwrap_or_else(|_| ObjectPath::from_static_str_unchecked("/"));
            let _ = ManagerIface::sign_in_finished(iface.signal_emitter(), ok, &msg, path).await;
        }
    }

    async fn finish_code(
        self: &Arc<Self>,
        oauth: &OAuthConfig,
        code: &str,
        redirect: &str,
        pkce: &Pkce,
    ) -> anyhow::Result<String> {
        let tokens = auth::exchange_code(&self.http, oauth, code, redirect, pkce).await?;
        self.finish_tokens(tokens, "common").await
    }

    /// Create the account (or re-authenticate an existing one) from fresh tokens.
    async fn finish_tokens(self: &Arc<Self>, tokens: TokenSet, tenant: &str) -> anyhow::Result<String> {
        // One at a time: two sign-ins finishing together (a browser sign-in
        // left open and the built-in page, say) would otherwise both find no
        // account yet and both add it. Saving the tokens can wait for the
        // keyring to be unlocked, so the window is not small.
        let _one_at_a_time = self.finishing.lock().await;
        let probe_tm = Arc::new(TokenManager::new(self.http.clone(), self.oauth()?, tokens.clone(), Arc::new(|_| {})));
        let probe = GraphClient::new(self.http.clone(), graph_base_from_env(), probe_tm);
        let me = probe.me().await.context("reading your profile")?;
        let drive = probe.drive().await.context("opening your OneDrive")?;
        let drive_id = od_graph::client::normalize_drive_id(&drive.id);
        let drive_type = match drive.drive_type.as_deref() {
            Some("business") => DriveType::Business,
            Some("documentLibrary") => DriveType::DocumentLibrary,
            _ => DriveType::Personal,
        };

        let email = me.email();

        // Re-authentication of a known account. Every configured account
        // counts, also one that is not running (it failed to start, say).
        let known = self.cfg().accounts.into_iter().find(|a| same_account(a, &drive_id, &email, drive_type));
        if let Some(cfg) = known {
            secrets::save(&cfg.id, &cfg.email, &tokens).await?;
            match self.account(&cfg.id) {
                Some(acct) => {
                    acct.tokens.replace(tokens).await;
                    if let Some(e) = acct.engine() {
                        e.refresh();
                        e.local_changed();
                    }
                }
                None => {
                    let acct = self.add_account_runtime(cfg.clone()).await?;
                    if cfg.setup_complete && !cfg.disabled {
                        let me = self.clone();
                        tokio::spawn(async move {
                            if let Err(e) = me.start_account(&acct).await {
                                tracing::error!(account = %acct.id, "cannot start account: {e:#}");
                                *acct.error.lock().unwrap() = Some(format!("{e:#}"));
                                me.state_changed().await;
                            }
                        });
                    }
                }
            }
            tracing::info!(account = %cfg.id, "signed in again");
            self.state_changed().await;
            return Ok(cfg.id);
        }

        let organization = if drive_type == DriveType::Personal {
            String::new()
        } else {
            probe.organization_name().await.ok().flatten().unwrap_or_default()
        };
        let id = format!("{:08x}", rand_u32());
        let mut mount_point =
            od_core::paths::default_mount_point(Some(organization.as_str()).filter(|o| !o.is_empty()));
        let used: Vec<PathBuf> = self.cfg().accounts.iter().map(|a| a.mount_point.clone()).collect();
        let mut n = 2;
        while used.contains(&mount_point) {
            mount_point = PathBuf::from(format!("{} ({n})", od_core::paths::default_mount_point(None).display()));
            n += 1;
        }
        let tenant = tokens
            .claims
            .as_ref()
            .and_then(|c| c.tid.clone())
            .filter(|_| tenant == "common")
            .map(|tid| if tid == auth::MSA_TENANT_ID { "consumers".to_string() } else { tid })
            .unwrap_or_else(|| tenant.to_string());
        let a = AccountConfig {
            id: id.clone(),
            email,
            display_name: me.display_name.clone().unwrap_or_default(),
            organization,
            drive_id,
            drive_type,
            tenant,
            mount_point,
            download_all: false,
            excluded_folders: vec![],
            backup_folders: vec![],
            disabled: false,
            setup_complete: false,
            web_url: drive.web_url.clone().unwrap_or_default(),
            backups: vec![],
            extra_scopes: vec![],
            libraries: vec![],
        };
        secrets::save(&id, &a.email, &tokens).await?;
        self.config.lock().unwrap().accounts.push(a.clone());
        self.save_config();
        self.add_account_runtime(a).await?;
        tracing::info!(account = %id, "account added; waiting for setup to finish");
        Ok(id)
    }

    /// Unlink: stop, forget tokens, keep the local data aside for recovery.
    pub async fn sign_out(self: &Arc<Self>, id: &str) -> anyhow::Result<()> {
        let acct = self.account(id).ok_or_else(|| anyhow!("unknown account"))?;
        self.stop_account(&acct).await;
        secrets::delete(id).await;
        let dir = od_core::paths::account_dir(id);
        if dir.exists() {
            let aside = od_core::paths::data_dir()
                .join("unlinked")
                .join(format!("{id}-{}", chrono::Utc::now().format("%Y%m%d%H%M%S")));
            std::fs::create_dir_all(aside.parent().unwrap())?;
            std::fs::rename(&dir, &aside)?;
            tracing::info!("account data kept at {}", aside.display());
        }
        let mp = acct.cfg().mount_point;
        let _ = std::fs::remove_dir(&mp); // only if empty
        for l in acct.cfg().libraries {
            let _ = std::fs::remove_dir(&l.mount_point);
        }
        let _ = std::fs::remove_dir_all(od_core::paths::data_dir().join("shortcuts").join(id));
        self.config.lock().unwrap().accounts.retain(|a| a.id != id);
        self.save_config();
        self.accounts.lock().unwrap().remove(id);
        let _ = self.conn.object_server().remove::<AccountIface, _>(acct.path()).await;
        self.accounts_changed().await;
        self.state_changed().await;
        Ok(())
    }

    pub fn update_account(&self, id: &str, f: impl FnOnce(&mut AccountConfig)) {
        if let Some(acct) = self.account(id) {
            let mut c = acct.cfg.lock().unwrap();
            f(&mut c);
            let snapshot = c.clone();
            drop(c);
            let mut cfg = self.config.lock().unwrap();
            if let Some(slot) = cfg.account_mut(id) {
                *slot = snapshot;
            }
        }
        self.save_config();
    }

    // ------------------------------------------------------------ pause & conditions

    pub async fn pause(self: &Arc<Self>, seconds: u32) {
        let until = if seconds == 0 { -1 } else { chrono::Utc::now().timestamp() + seconds as i64 };
        *self.paused_until.lock().unwrap() = until;
        if let Some(t) = self.pause_timer.lock().unwrap().take() {
            t.cancel();
        }
        if seconds > 0 {
            let token = CancellationToken::new();
            *self.pause_timer.lock().unwrap() = Some(token.clone());
            let me = self.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = token.cancelled() => {}
                    _ = tokio::time::sleep(Duration::from_secs(seconds as u64)) => me.resume().await,
                }
            });
        }
        self.for_each_engine(|e| e.set_paused(pause::USER, true));
        self.state_changed().await;
    }

    pub async fn resume(self: &Arc<Self>) {
        *self.paused_until.lock().unwrap() = 0;
        if let Some(t) = self.pause_timer.lock().unwrap().take() {
            t.cancel();
        }
        self.for_each_engine(|e| e.set_paused(pause::USER, false));
        self.state_changed().await;
    }

    pub fn paused_until(&self) -> i64 {
        *self.paused_until.lock().unwrap()
    }

    pub fn pause_reason(&self) -> String {
        if self.paused_until() != 0 {
            return "user".into();
        }
        let c = *self.conditions.lock().unwrap();
        let g = self.cfg().general;
        if c.metered && g.pause_on_metered {
            "metered".into()
        } else if c.power_saver && g.pause_on_power_saver {
            "power-saver".into()
        } else {
            String::new()
        }
    }

    pub fn online(&self) -> bool {
        self.conditions.lock().unwrap().online.unwrap_or(true)
    }

    pub(crate) fn apply_pause_to(&self, e: &Engine) {
        let c = *self.conditions.lock().unwrap();
        let g = self.cfg().general;
        e.set_paused(pause::USER, self.paused_until() != 0);
        e.set_paused(pause::METERED, c.metered && g.pause_on_metered);
        e.set_paused(pause::POWER_SAVER, c.power_saver && g.pause_on_power_saver);
    }

    fn apply_conditions(self: &Arc<Self>, c: Conditions) {
        let prev = std::mem::replace(&mut *self.conditions.lock().unwrap(), c);
        let engines = self.engines();
        for e in &engines {
            self.apply_pause_to(e);
            if prev.online == Some(false) && c.online == Some(true) {
                e.network_changed(true);
            }
        }
        let g = self.cfg();
        if c.metered
            && !prev.metered
            && g.general.pause_on_metered
            && g.notifications.sync_paused
            && !engines.is_empty()
        {
            let me = self.clone();
            tokio::spawn(async move {
                me.notifier
                    .show(
                        Some("paused"),
                        "Syncing is paused",
                        "You're on a metered network, so files aren't syncing to save data.",
                        vec![("default", "Open", Action::Show("activity".into()))],
                        false,
                    )
                    .await;
                me.state_changed().await;
            });
        } else {
            let me = self.clone();
            tokio::spawn(async move { me.state_changed().await });
        }
    }

    /// Every running engine: accounts' folders, shortcuts and libraries.
    pub fn engines(&self) -> Vec<Arc<Engine>> {
        let accounts: Vec<_> = self.accounts.lock().unwrap().values().cloned().collect();
        let mut out = Vec::new();
        for a in accounts {
            out.extend(a.engine());
            out.extend(a.roots.lock().unwrap().iter().map(|r| r.engine.clone()));
        }
        out
    }

    fn for_each_engine(&self, f: impl Fn(&Engine)) {
        for e in self.engines() {
            f(&e);
        }
    }

    /// Aggregate state for the tray: the most severe account state.
    pub fn overall_state(&self) -> SyncState {
        let accounts: Vec<_> = self.accounts.lock().unwrap().values().cloned().collect();
        if accounts.is_empty() {
            return SyncState::SignedOut;
        }
        accounts.iter().map(|a| a.state()).max_by_key(|s| s.severity()).unwrap_or(SyncState::Idle)
    }

    // ------------------------------------------------------------ settings

    pub fn settings_doc(&self) -> SettingsDoc {
        let c = self.cfg();
        SettingsDoc { general: c.general, network: c.network, notifications: c.notifications }
    }

    /// Merge a partial settings document and apply it to running accounts.
    pub async fn set_settings(self: &Arc<Self>, json: &str) -> anyhow::Result<()> {
        let patch: serde_json::Value = serde_json::from_str(json)?;
        let mut current = serde_json::to_value(self.settings_doc())?;
        merge(&mut current, patch);
        let doc: SettingsDoc = serde_json::from_value(current)?;
        let old = self.settings_doc();
        {
            let mut c = self.config.lock().unwrap();
            c.general = doc.general.clone();
            c.network = doc.network.clone();
            c.notifications = doc.notifications.clone();
        }
        self.save_config();
        for acct in self.accounts.lock().unwrap().values() {
            acct.graph.upload_limiter.set_limit(doc.network.upload_limit_kbps as u64 * 1024);
            acct.graph.download_limiter.set_limit(doc.network.download_limit_kbps as u64 * 1024);
            if let Some(e) = acct.engine() {
                let g = doc.general.clone();
                let n = doc.network.clone();
                e.update_cfg(|ec| {
                    ec.ignore = IgnoreSet::new(&g.excluded_patterns);
                    ec.mass_delete_threshold = g.mass_delete_threshold;
                    ec.poll_interval = Duration::from_secs(g.poll_interval_secs.max(15));
                    ec.max_parallel_uploads = n.max_parallel_uploads.clamp(1, 16) as usize;
                    ec.max_parallel_downloads = n.max_parallel_downloads.clamp(1, 16) as usize;
                    ec.block_background_hydration = g.block_background_hydration;
                    ec.auto_free_up_days = g.auto_free_up_days;
                    ec.server_thumbnails = g.server_thumbnails;
                    ec.push_notifications = n.push_notifications;
                });
                self.apply_pause_to(&e);
            }
        }
        if old.general.start_at_login != doc.general.start_at_login
            && let Err(e) = crate::autostart::set_enabled(&self.conn, doc.general.start_at_login).await
        {
            tracing::warn!("could not change autostart: {e:#}");
        }
        self.state_changed().await;
        Ok(())
    }

    // ------------------------------------------------------------ events → D-Bus

    pub async fn accounts_changed(&self) {
        if let Ok(iface) = self.conn.object_server().interface::<_, ManagerIface>(api::MANAGER_PATH).await {
            let _ = iface.get().await.accounts_changed(iface.signal_emitter()).await;
        }
    }

    pub async fn state_changed(&self) {
        if let Ok(iface) = self.conn.object_server().interface::<_, ManagerIface>(api::MANAGER_PATH).await {
            let g = iface.get().await;
            let e = iface.signal_emitter();
            let _ = g.state_changed(e).await;
            let _ = g.paused_until_changed(e).await;
            let _ = g.pause_reason_changed(e).await;
            let _ = g.online_changed(e).await;
        }
    }

    /// Refresh an account's D-Bus properties without waiting.
    pub(crate) fn account_props_changed_soon(self: &Arc<Self>, id: &str) {
        let (me, id) = (self.clone(), id.to_string());
        tokio::spawn(async move {
            me.account_props_changed(&id).await;
            me.state_changed().await;
        });
    }

    async fn account_props_changed(&self, id: &str) {
        let path = api::account_path(id);
        if let Ok(iface) = self.conn.object_server().interface::<_, AccountIface>(path.as_str()).await {
            let g = iface.get().await;
            let e = iface.signal_emitter();
            let _ = g.state_changed(e).await;
            let _ = g.status_text_changed(e).await;
            let _ = g.progress_changed(e).await;
            let _ = g.issue_count_changed(e).await;
            let _ = g.quota_used_changed(e).await;
            let _ = g.quota_total_changed(e).await;
            let _ = g.quota_state_changed(e).await;
            let _ = g.held_deletes_changed(e).await;
            let _ = g.needs_setup_changed(e).await;
            let _ = g.mount_point_changed(e).await;
            let _ = g.download_all_changed(e).await;
        }
    }

    /// Relay engine events: properties (rate-limited), activity, emblem
    /// changes, and user-facing notifications.
    pub(crate) fn forward_events(
        self: Arc<Self>,
        acct: Arc<AccountRt>,
        engine: Arc<Engine>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        // Boxed with an explicit `Send`: forwarding can start shortcut
        // engines, which start forwarding in turn.
        Box::pin(self.forward_events_inner(acct, engine))
    }

    async fn forward_events_inner(self: Arc<Self>, acct: Arc<AccountRt>, engine: Arc<Engine>) {
        let mut rx = engine.subscribe();
        let path = acct.path();
        // A shortcut's or library's engine: its paths are shown through the link.
        let root = Self::root_of(&acct, &engine);
        let is_main = root.is_none();
        let mut dirty = true;
        let mut tick = tokio::time::interval(Duration::from_millis(300));
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => return,
                _ = tick.tick() => {
                    if dirty {
                        dirty = false;
                        self.account_props_changed(&acct.id).await;
                        self.state_changed().await;
                    }
                    let alive = match &root {
                        None => acct.engine().is_some_and(|e| Arc::ptr_eq(&e, &engine)),
                        Some(r) => acct.roots.lock().unwrap().iter().any(|x| Arc::ptr_eq(x, r)),
                    };
                    if !alive {
                        return;
                    }
                }
                ev = rx.recv() => {
                    let ev = match ev {
                        Ok(ev) => ev,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => { dirty = true; continue }
                        Err(_) => return,
                    };
                    match ev {
                        EngineEvent::StateChanged | EngineEvent::ProgressChanged | EngineEvent::QuotaChanged => dirty = true,
                        EngineEvent::Activity(mut entry) => {
                            if let Some(r) = &root {
                                entry.path = r.shown(Path::new(&entry.path)).to_string_lossy().into_owned();
                                entry.id += crate::roots::activity_id_offset(&r.key);
                                if entry.folder == "OneDrive" {
                                    entry.folder = r.title.lock().unwrap().clone();
                                }
                            }
                            if let Ok(iface) = self.conn.object_server().interface::<_, AccountIface>(path.as_str()).await {
                                let json = serde_json::to_string(&entry).unwrap_or_default();
                                let _ = AccountIface::activity_added(iface.signal_emitter(), &json).await;
                            }
                        }
                        EngineEvent::ItemsChanged(paths) => {
                            if let Ok(iface) = self.conn.object_server().interface::<_, AccountIface>(path.as_str()).await {
                                let paths: Vec<String> = paths
                                    .iter()
                                    .map(|p| match &root {
                                        Some(r) => r.shown(p),
                                        None => p.clone(),
                                    })
                                    .map(|p| p.to_string_lossy().into_owned())
                                    .collect();
                                let _ = AccountIface::items_changed(iface.signal_emitter(), paths).await;
                            }
                        }
                        EngineEvent::MassDeleteHeld(n) => {
                            dirty = true;
                            if self.cfg().notifications.mass_delete {
                                self.notifier.show(
                                    Some(&format!("massdel-{}", acct.id)),
                                    &format!("You deleted {n} files from your OneDrive"),
                                    "They haven't been removed online yet. Choose whether to remove them everywhere or restore them.",
                                    vec![("default", "Review", Action::Show(format!("mass-delete:{}", acct.id)))],
                                    true,
                                ).await;
                            }
                        }
                        EngineEvent::AuthRequired => {
                            dirty = true;
                            self.notifier.show(
                                Some(&format!("auth-{}", acct.id)),
                                "Sign in to keep syncing",
                                &format!("Your session for {} expired.", acct.cfg().email),
                                vec![("default", "Sign in", Action::Show(format!("signin:{}", acct.id)))],
                                true,
                            ).await;
                        }
                        EngineEvent::Fatal(msg) => {
                            dirty = true;
                            self.notifier.show(Some(&format!("fatal-{}", acct.id)), "Syncing stopped", &msg,
                                vec![("default", "Open", Action::Show("activity".into()))], true).await;
                        }
                        EngineEvent::Notice { kind, title, body, path } => {
                            self.notice(kind, &title, &body, path).await;
                        }
                        EngineEvent::LocalDelete => self.delete_reminder(&acct).await,
                        EngineEvent::ShortcutsChanged if is_main => {
                            let (me, a) = (self.clone(), acct.clone());
                            tokio::spawn(async move { me.sync_shortcuts(&a).await });
                        }
                        EngineEvent::ShortcutsChanged => {}
                    }
                }
            }
        }
    }

    /// "Deleted files are removed everywhere": shown by the app as a dialog
    /// (with "Don't show this reminder again"), or as a notification when
    /// the app isn't running.
    async fn delete_reminder(&self, acct: &Arc<AccountRt>) {
        if !self.cfg().notifications.delete_reminder {
            return;
        }
        let gui_running = match zbus::fdo::DBusProxy::new(&self.conn).await {
            Ok(p) => p.name_has_owner(od_core::APP_ID.try_into().expect("valid name")).await.unwrap_or(false),
            Err(_) => false,
        };
        if gui_running {
            if let Ok(iface) = self.conn.object_server().interface::<_, ManagerIface>(api::MANAGER_PATH).await {
                let path = acct.path();
                if let Ok(p) = ObjectPath::try_from(path.as_str()) {
                    let _ = ManagerIface::delete_reminder(iface.signal_emitter(), p).await;
                }
            }
            return;
        }
        self.notifier
            .show(
                Some("delete-reminder"),
                "Deleted files are removed everywhere",
                "Files you delete from your OneDrive folder are also deleted online and on your other devices. You can get them back from the OneDrive recycle bin.",
                vec![
                    ("default", "OK", Action::Show("activity".into())),
                    (
                        "dont-show",
                        "Don't show again",
                        Action::Ctl(vec!["settings".into(), "notifications.delete_reminder=false".into()]),
                    ),
                ],
                false,
            )
            .await;
    }

    async fn notice(&self, kind: NoticeKind, title: &str, body: &str, path: Option<PathBuf>) {
        let n = self.cfg().notifications;
        if !n.enabled {
            return;
        }
        let allowed = match kind {
            NoticeKind::Conflict => n.conflicts,
            NoticeKind::OfflineOpen => n.offline_open,
            NoticeKind::DiskFull | NoticeKind::QuotaFull => n.storage_full,
            NoticeKind::SyncError | NoticeKind::Recovered => n.sync_errors,
            NoticeKind::RemoteMassDelete => n.mass_delete,
        };
        if !allowed {
            return;
        }
        let mut actions = vec![];
        if let Some(p) = path {
            let folder = if p.is_dir() { p.clone() } else { p.parent().map(Path::to_path_buf).unwrap_or(p.clone()) };
            actions.push(("default", "Open", Action::Open(p)));
            actions.push(("folder", "Show in folder", Action::Open(folder)));
        } else {
            actions.push(("default", "Open", Action::Show("activity".into())));
        }
        let key = format!("{kind:?}");
        self.notifier.show(Some(&key), title, body, actions, false).await;
    }

    // ------------------------------------------------------------ folder backup

    pub fn backup_folders(&self, id: &str) -> Vec<od_core::dbus::BackupFolder> {
        let Some(acct) = self.account(id) else { return vec![] };
        let cfg = acct.cfg();
        let jobs = self.backup_jobs.lock().unwrap().clone();
        crate::backup::FOLDERS
            .iter()
            .map(|(key, name)| {
                let entry = cfg.backups.iter().find(|b| b.key == *key);
                let local = entry.map(|e| e.original.clone()).unwrap_or_else(|| crate::backup::xdg_dir(key));
                let measured = if entry.is_some() { cfg.mount_point.join(name) } else { local.clone() };
                let (files, bytes) = crate::backup::measure(&measured);
                od_core::dbus::BackupFolder {
                    key: key.to_string(),
                    name: name.to_string(),
                    local_path: local.to_string_lossy().into_owned(),
                    backed_up: entry.is_some(),
                    in_progress: jobs.contains(&(id.to_string(), key.to_string())),
                    files,
                    bytes,
                }
            })
            .collect()
    }

    /// Start or stop backing up a folder; the work runs in the background.
    pub fn set_folder_backup(self: &Arc<Self>, id: &str, key: &str, enabled: bool) -> anyhow::Result<()> {
        let acct = self.account(id).ok_or_else(|| anyhow!("unknown account"))?;
        let key = key.to_ascii_uppercase();
        let name = crate::backup::folder_name(&key).ok_or_else(|| anyhow!("unknown folder {key}"))?;
        let engine = acct.engine().ok_or_else(|| anyhow!("This account isn't syncing yet."))?;
        let cfg = acct.cfg();
        let already = cfg.backups.iter().any(|b| b.key == key);
        if enabled == already {
            return Ok(());
        }
        if !self.backup_jobs.lock().unwrap().insert((id.to_string(), key.clone())) {
            anyhow::bail!("{name} is already being updated");
        }
        let me = self.clone();
        let (id, rt) = (id.to_string(), self.rt.clone());
        std::thread::spawn(move || {
            let result = if enabled {
                me.start_backup(&acct, &engine, &key, name)
            } else {
                me.stop_backup(&acct, &engine, &key, name)
            };
            me.backup_jobs.lock().unwrap().remove(&(id.clone(), key.clone()));
            let (title, body) = match &result {
                Ok(msg) => (
                    if enabled { format!("{name} is backed up") } else { format!("{name} backup stopped") },
                    msg.clone(),
                ),
                Err(e) => (format!("Couldn't update the {name} backup"), format!("{e:#}")),
            };
            rt.block_on(async move {
                me.notifier
                    .show(
                        None,
                        &title,
                        &body,
                        vec![("default", "Open", Action::Show("settings".into()))],
                        result.is_err(),
                    )
                    .await;
                me.state_changed().await;
            });
        });
        Ok(())
    }

    fn start_backup(
        &self,
        acct: &Arc<AccountRt>,
        engine: &Arc<Engine>,
        key: &str,
        name: &str,
    ) -> anyhow::Result<String> {
        let mount = acct.cfg().mount_point;
        let src = crate::backup::xdg_dir(key);
        if src.starts_with(&mount) {
            anyhow::bail!("{} is already inside your OneDrive folder", src.display());
        }
        let target = mount.join(name);
        let mut report = crate::backup::Report::default();
        if src.is_dir() && !src.is_symlink() {
            crate::backup::move_tree(&src, &target, &mut report)?;
            if std::fs::remove_dir(&src).is_err() {
                // Things OneDrive can't hold (links, sockets) stay next to the link.
                let left = src.with_file_name(format!("{name} (not backed up)"));
                std::fs::rename(&src, &left)?;
            }
        } else {
            std::fs::create_dir_all(&target)?;
            if src.is_symlink() {
                std::fs::remove_file(&src)?;
            }
        }
        std::os::unix::fs::symlink(&target, &src)?;
        crate::backup::set_xdg_dir(key, &target)?;
        if let Ok(it) = engine.resolve_abs(&target) {
            let _ = engine.set_pin_state(it.ino, od_core::types::PinState::AlwaysKeep);
        }
        self.update_account(&acct.id, |a| {
            a.backups.retain(|b| b.key != key);
            a.backups.push(od_core::config::BackupEntry { key: key.to_string(), original: src.clone() });
        });
        let mut msg = format!(
            "{} file(s) ({}) moved to OneDrive. {} still opens your files.",
            report.moved,
            od_core::fmt::size(report.bytes),
            src.display()
        );
        if !report.skipped.is_empty() {
            msg.push_str(&format!(
                " {} item(s) couldn't be moved and were left in \"{name} (not backed up)\".",
                report.skipped.len()
            ));
        }
        Ok(msg)
    }

    fn stop_backup(
        &self,
        acct: &Arc<AccountRt>,
        engine: &Arc<Engine>,
        key: &str,
        name: &str,
    ) -> anyhow::Result<String> {
        let cfg = acct.cfg();
        let entry =
            cfg.backups.iter().find(|b| b.key == key).cloned().ok_or_else(|| anyhow!("{name} isn't backed up"))?;
        let target = cfg.mount_point.join(name);
        let original = entry.original;
        if original.is_symlink() {
            std::fs::remove_file(&original)?;
        }
        let mut report = crate::backup::Report::default();
        if target.is_dir() {
            crate::backup::copy_tree(&target, &original, &mut report)?;
        } else {
            std::fs::create_dir_all(&original)?;
        }
        crate::backup::set_xdg_dir(key, &original)?;
        if let Ok(it) = engine.resolve_abs(&target) {
            let _ = engine.set_pin_state(it.ino, od_core::types::PinState::Inherit);
        }
        self.update_account(&acct.id, |a| a.backups.retain(|b| b.key != key));
        Ok(format!(
            "{} file(s) were copied back to {}. They're also still in OneDrive.",
            report.moved,
            original.display()
        ))
    }

    /// Account and engine owning an absolute path (for file-manager
    /// integration), and the path in that engine's mount (paths through a
    /// shortcut's link lead to the shortcut's engine).
    pub fn locate(&self, abs: &Path) -> Option<(Arc<AccountRt>, Arc<Engine>, PathBuf)> {
        let accounts: Vec<_> = self.accounts.lock().unwrap().values().cloned().collect();
        accounts.into_iter().find_map(|a| Self::route(&a, abs).map(|(e, p)| (a, e, p)))
    }

    pub fn account_paths(&self) -> Vec<OwnedObjectPath> {
        self.accounts.lock().unwrap().values().filter_map(|a| OwnedObjectPath::try_from(a.path()).ok()).collect()
    }
}

fn merge(dst: &mut serde_json::Value, patch: serde_json::Value) {
    match (dst, patch) {
        (serde_json::Value::Object(d), serde_json::Value::Object(p)) => {
            for (k, v) in p {
                merge(d.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }
        (d, p) => *d = p,
    }
}

/// Whether a configured account is the one just signed in to: the same
/// drive, or the same address with the same kind of OneDrive. (One address
/// can have both a personal and a work or school OneDrive; those differ.)
fn same_account(a: &AccountConfig, drive_id: &str, email: &str, drive_type: DriveType) -> bool {
    od_graph::client::normalize_drive_id(&a.drive_id) == drive_id
        || (!email.is_empty() && a.email.eq_ignore_ascii_case(email) && a.drive_type == drive_type)
}

fn rand_u32() -> u32 {
    use rand::Rng;
    rand::rng().random()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_account_by_drive_or_by_address_and_kind() {
        let a: AccountConfig = serde_json::from_value(serde_json::json!({
            "id": "1", "email": "Pat@Example.com", "drive_id": "ABCDEF0123456789",
            "drive_type": "personal", "mount_point": "/home/u/OneDrive",
        }))
        .unwrap();
        // The same drive, however its ID was written.
        assert!(same_account(&a, "abcdef0123456789", "", DriveType::Personal));
        // The same address and kind, even with another drive ID.
        assert!(same_account(&a, "0000000000000001", "pat@example.com", DriveType::Personal));
        // The same address with a work or school OneDrive is another account.
        assert!(!same_account(&a, "b!work-drive", "pat@example.com", DriveType::Business));
        assert!(!same_account(&a, "0000000000000001", "sam@example.com", DriveType::Personal));
    }

    #[test]
    fn settings_merge() {
        let mut a = serde_json::json!({"general": {"a": 1, "b": 2}, "network": {"x": 1}});
        merge(&mut a, serde_json::json!({"general": {"b": 3}}));
        assert_eq!(a, serde_json::json!({"general": {"a": 1, "b": 3}, "network": {"x": 1}}));
    }
}
