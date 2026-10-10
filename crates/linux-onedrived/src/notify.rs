//! Desktop notifications through `org.freedesktop.Notifications`, attributed
//! to the GUI's .desktop entry so they show its name and icon, with actions
//! that open a file/folder or the right page of the app.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use zbus::zvariant::Value;

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn activation_token(&self, id: u32, token: String) -> zbus::Result<()>;
}

/// What clicking a notification (or one of its buttons) does.
#[derive(Debug, Clone)]
pub enum Action {
    /// Open a file or folder with the default application.
    Open(PathBuf),
    /// Show a page of the app: "activity", "issues", "signin:<acct>", "mass-delete:<acct>", "settings".
    Show(String),
    /// Run linux-onedrivectl with these arguments.
    Ctl(Vec<String>),
}

pub struct Notifier {
    proxy: Option<NotificationsProxy<'static>>,
    actions: Mutex<HashMap<u32, Vec<(String, Action)>>>,
    tokens: Mutex<HashMap<u32, String>>,
    by_key: Mutex<HashMap<String, u32>>,
}

impl Notifier {
    pub async fn new(conn: &zbus::Connection) -> Notifier {
        let proxy = NotificationsProxy::new(conn).await.ok();
        Notifier {
            proxy,
            actions: Mutex::new(HashMap::new()),
            tokens: Mutex::new(HashMap::new()),
            by_key: Mutex::new(HashMap::new()),
        }
    }

    /// Show (or replace, when `key` matches an earlier one) a notification.
    pub async fn show(
        &self,
        key: Option<&str>,
        summary: &str,
        body: &str,
        actions: Vec<(&str, &str, Action)>,
        urgent: bool,
    ) {
        let Some(p) = &self.proxy else { return };
        let replaces = key.and_then(|k| self.by_key.lock().unwrap().get(k).copied()).unwrap_or(0);
        let mut flat: Vec<&str> = Vec::new();
        for (k, label, _) in &actions {
            flat.push(k);
            flat.push(label);
        }
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("desktop-entry", Value::from(od_core::APP_ID));
        hints.insert("urgency", Value::U8(if urgent { 2 } else { 1 }));
        match p.notify(od_core::APP_NAME, replaces, od_core::APP_ID, summary, body, &flat, hints, -1).await {
            Ok(id) => {
                if let Some(k) = key {
                    self.by_key.lock().unwrap().insert(k.to_string(), id);
                }
                let mut a = self.actions.lock().unwrap();
                if a.len() > 200 {
                    a.clear();
                }
                a.insert(id, actions.into_iter().map(|(k, _, act)| (k.to_string(), act)).collect());
            }
            Err(e) => tracing::debug!("notification failed: {e}"),
        }
    }

    /// Handle clicks until the daemon exits.
    pub async fn listen(self: std::sync::Arc<Self>) {
        use futures::StreamExt;
        let Some(p) = self.proxy.clone() else { return };
        let (Ok(mut actions), Ok(mut tokens)) = (p.receive_action_invoked().await, p.receive_activation_token().await)
        else {
            return;
        };
        loop {
            tokio::select! {
                Some(sig) = tokens.next() => {
                    if let Ok(a) = sig.args() {
                        self.tokens.lock().unwrap().insert(a.id, a.token.clone());
                    }
                }
                Some(sig) = actions.next() => {
                    let Ok(a) = sig.args() else { continue };
                    let act = self
                        .actions
                        .lock()
                        .unwrap()
                        .get(&a.id)
                        .and_then(|v| v.iter().find(|(k, _)| *k == a.action_key).map(|(_, x)| x.clone()));
                    let token = self.tokens.lock().unwrap().remove(&a.id);
                    if let Some(act) = act {
                        run_action(&act, token.as_deref());
                    }
                }
                else => break,
            }
        }
    }
}

/// Path to a sibling binary (installed next to the daemon), else from $PATH.
pub fn sibling_binary(name: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(name))
}

pub fn run_action(act: &Action, activation_token: Option<&str>) {
    let mut cmd = match act {
        Action::Open(p) => {
            let mut c = std::process::Command::new("xdg-open");
            c.arg(p);
            c
        }
        Action::Show(page) => {
            let mut c = std::process::Command::new(sibling_binary("linux-onedrive"));
            c.arg("--show").arg(page);
            c
        }
        Action::Ctl(args) => {
            let mut c = std::process::Command::new(sibling_binary("linux-onedrivectl"));
            c.args(args);
            c
        }
    };
    if let Some(t) = activation_token {
        cmd.env("XDG_ACTIVATION_TOKEN", t);
    }
    if let Err(e) = cmd.spawn() {
        tracing::warn!("could not run notification action: {e}");
    }
}
