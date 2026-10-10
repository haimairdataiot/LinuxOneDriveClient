//! Connection to the daemon. All D-Bus traffic runs on a background tokio
//! runtime; results and change notifications are delivered to the GTK main
//! loop as [`UiEvent`]s or through [`call`] callbacks.

use futures::StreamExt;
use od_core::dbus::{AccountProxy, ActivityEntry, BUS_NAME, ManagerProxy, Progress};
use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;
use zbus::proxy::CacheProperties;

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

pub fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("lod-gui-io")
            .build()
            .expect("tokio runtime")
    })
}

/// Run `fut` on the I/O runtime and hand its output to `then` on the GTK thread.
pub fn call<T, F>(fut: F, then: impl FnOnce(T) + 'static)
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    runtime().spawn(async move {
        let _ = tx.send(fut.await);
    });
    gtk::glib::spawn_future_local(async move {
        if let Ok(v) = rx.await {
            then(v);
        }
    });
}

/// Snapshot of one account, as shown in the UI.
#[derive(Debug, Clone, Default)]
pub struct AccountView {
    pub path: String,
    pub id: String,
    pub email: String,
    pub display_name: String,
    pub organization: String,
    pub drive_type: String,
    pub mount_point: String,
    pub web_url: String,
    pub state: String,
    pub status_text: String,
    pub quota_used: u64,
    pub quota_total: u64,
    pub quota_state: String,
    pub issue_count: u32,
    pub progress: Progress,
    pub needs_setup: bool,
    pub download_all: bool,
    pub held_deletes: u32,
}

impl AccountView {
    pub fn title(&self) -> String {
        if self.organization.is_empty() { "Personal".to_string() } else { self.organization.clone() }
    }
}

/// Snapshot of the whole daemon.
#[derive(Debug, Clone, Default)]
pub struct Model {
    pub connected: bool,
    pub state: String,
    pub paused_until: i64,
    pub pause_reason: String,
    pub online: bool,
    pub needs_client_id: bool,
    /// Version of an available update, or empty.
    pub update_version: String,
    pub accounts: Vec<AccountView>,
}

impl Model {
    pub fn account(&self, path_or_id: &str) -> Option<&AccountView> {
        self.accounts.iter().find(|a| a.path == path_or_id || a.id == path_or_id)
    }
    pub fn issue_count(&self) -> u32 {
        self.accounts.iter().map(|a| a.issue_count).sum()
    }
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Model(Model),
    Activity { account: String, entry: ActivityEntry },
    SignInFinished { ok: bool, message: String, account: String },
    DeleteReminder { account: String },
}

pub async fn connection() -> zbus::Result<zbus::Connection> {
    static CONN: tokio::sync::OnceCell<zbus::Connection> = tokio::sync::OnceCell::const_new();
    CONN.get_or_try_init(zbus::Connection::session).await.cloned()
}

pub async fn manager() -> zbus::Result<ManagerProxy<'static>> {
    ManagerProxy::builder(&connection().await?).cache_properties(CacheProperties::No).build().await
}

pub async fn account(path: &str) -> zbus::Result<AccountProxy<'static>> {
    AccountProxy::builder(&connection().await?)
        .path(path.to_string())?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

/// Make sure the daemon runs: D-Bus activation when installed, otherwise
/// start the binary next to ours (development builds).
pub async fn ensure_daemon() -> zbus::Result<()> {
    let conn = connection().await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let name = BUS_NAME.try_into().expect("valid bus name");
    if dbus.name_has_owner(name).await.unwrap_or(false) {
        return Ok(());
    }
    if dbus.start_service_by_name(BUS_NAME.try_into().expect("valid"), 0).await.is_ok() {
        return Ok(());
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("linux-onedrived")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| "linux-onedrived".into());
    tracing::info!("starting the daemon: {}", exe.display());
    let _ = std::process::Command::new(exe).spawn();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if dbus.name_has_owner(BUS_NAME.try_into().expect("valid")).await.unwrap_or(false) {
            return Ok(());
        }
    }
    Err(zbus::Error::Failure("the sync service didn't start".into()))
}

async fn fetch_account(path: &str) -> zbus::Result<AccountView> {
    let a = account(path).await?;
    Ok(AccountView {
        path: path.to_string(),
        id: a.id().await?,
        email: a.email().await?,
        display_name: a.display_name().await?,
        organization: a.organization().await?,
        drive_type: a.drive_type().await?,
        mount_point: a.mount_point().await?,
        web_url: a.web_root_url().await?,
        state: a.state().await?,
        status_text: a.status_text().await?,
        quota_used: a.quota_used().await?,
        quota_total: a.quota_total().await?,
        quota_state: a.quota_state().await?,
        issue_count: a.issue_count().await?,
        progress: serde_json::from_str(&a.progress().await?).unwrap_or_default(),
        needs_setup: a.needs_setup().await?,
        download_all: a.download_all().await?,
        held_deletes: a.held_deletes().await?,
    })
}

pub async fn fetch_model() -> zbus::Result<Model> {
    let m = manager().await?;
    let mut accounts = Vec::new();
    for p in m.accounts().await? {
        if let Ok(a) = fetch_account(p.as_str()).await {
            accounts.push(a);
        }
    }
    Ok(Model {
        connected: true,
        state: m.state().await?,
        paused_until: m.paused_until().await?,
        pause_reason: m.pause_reason().await?,
        online: m.online().await?,
        needs_client_id: m.needs_client_id().await?,
        update_version: m.update_version().await.unwrap_or_default(),
        accounts,
    })
}

/// Keep the UI's model current: subscribe to every change signal and
/// re-fetch (debounced). Reconnects when the daemon restarts.
pub async fn watch(tx: async_channel::Sender<UiEvent>) {
    loop {
        if let Err(e) = watch_once(&tx).await {
            tracing::warn!("lost the sync service: {e}");
        }
        let _ = tx.send(UiEvent::Model(Model::default())).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn watch_once(tx: &async_channel::Sender<UiEvent>) -> zbus::Result<()> {
    ensure_daemon().await?;
    let conn = connection().await?;
    // Any PropertiesChanged from the daemon (manager or accounts) → refresh.
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(BUS_NAME)?.build();
    let mut signals = zbus::MessageStream::for_match_rule(rule, &conn, Some(256)).await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let mut owner_changes = dbus.receive_name_owner_changed().await?;

    tx.send(UiEvent::Model(fetch_model().await?)).await.ok();
    let mut pending = false;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            msg = signals.next() => {
                let Some(Ok(msg)) = msg else { return Ok(()) };
                let header = msg.header();
                let member = header.member().map(|m| m.as_str().to_string()).unwrap_or_default();
                let path = header.path().map(|p| p.as_str().to_string()).unwrap_or_default();
                match member.as_str() {
                    "ActivityAdded" => {
                        if let Ok(json) = msg.body().deserialize::<String>()
                            && let Ok(entry) = serde_json::from_str(&json) {
                                tx.send(UiEvent::Activity { account: path, entry }).await.ok();
                            }
                    }
                    "SignInFinished" => {
                        if let Ok((ok, message, account)) = msg.body().deserialize::<(bool, String, zbus::zvariant::OwnedObjectPath)>() {
                            tx.send(UiEvent::SignInFinished { ok, message, account: account.to_string() }).await.ok();
                        }
                        pending = true;
                    }
                    "DeleteReminder" => {
                        if let Ok(account) = msg.body().deserialize::<zbus::zvariant::OwnedObjectPath>() {
                            tx.send(UiEvent::DeleteReminder { account: account.to_string() }).await.ok();
                        }
                    }
                    "ItemsChanged" => {}
                    _ => pending = true,
                }
            }
            change = owner_changes.next() => {
                if let Some(c) = change
                    && let Ok(args) = c.args()
                        && args.name().as_str() == BUS_NAME && args.new_owner().is_none() {
                            return Ok(());
                        }
            }
            _ = tick.tick() => {
                if pending {
                    pending = false;
                    if let Ok(m) = fetch_model().await {
                        tx.send(UiEvent::Model(m)).await.ok();
                    }
                }
            }
        }
    }
}
