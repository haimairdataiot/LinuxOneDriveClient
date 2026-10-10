//! Tray icon (StatusNotifierItem via ksni). Works with GNOME's AppIndicator
//! extension (single click opens this menu, double click shows the activity
//! panel) and KDE Plasma (left click shows the panel).

use crate::client::Model;
use ksni::menu::{MenuItem, StandardItem, SubMenu};

/// What the user picked in the tray.
#[derive(Debug, Clone)]
pub enum TrayCmd {
    ShowActivity,
    OpenFolder,
    ViewOnline,
    Pause(u32),
    Resume,
    Settings,
    Update,
    Issues,
    SignIn,
    Quit,
}

pub struct Tray {
    pub model: Model,
    pub tx: async_channel::Sender<TrayCmd>,
}

impl Tray {
    fn send(&self, c: TrayCmd) {
        let _ = self.tx.try_send(c);
    }

    fn state(&self) -> &str {
        if !self.model.connected { "signed-out" } else { &self.model.state }
    }

    fn status_line(&self) -> String {
        if !self.model.connected {
            return "Starting…".into();
        }
        if self.model.accounts.is_empty() {
            return "Not signed in".into();
        }
        let text: Vec<String> = self.model.accounts.iter().map(|a| a.status_text.clone()).collect();
        text.into_iter().find(|t| t != "Your files are synced").unwrap_or_else(|| "Your files are synced".into())
    }
}

fn item(label: &str, icon: &str, f: impl Fn(&mut Tray) + Send + 'static) -> MenuItem<Tray> {
    StandardItem { label: label.into(), icon_name: icon.into(), activate: Box::new(f), ..Default::default() }.into()
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "linux-onedrive".into()
    }

    fn title(&self) -> String {
        od_core::APP_NAME.into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn icon_name(&self) -> String {
        crate::util::state_icon(self.state()).into()
    }

    fn icon_theme_path(&self) -> String {
        if crate::util::icons_installed() {
            String::new()
        } else {
            crate::util::dev_icon_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_name: self.icon_name(),
            title: od_core::APP_NAME.into(),
            description: self.status_line(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayCmd::ShowActivity);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut m: Vec<MenuItem<Self>> = vec![
            StandardItem { label: self.status_line(), enabled: false, ..Default::default() }.into(),
            MenuItem::Separator,
        ];
        let signed_in = !self.model.accounts.is_empty();
        if !signed_in {
            m.push(item("Sign in…", "avatar-default-symbolic", |t| t.send(TrayCmd::SignIn)));
        }
        m.push(item("Show Activity", "view-list-symbolic", |t| t.send(TrayCmd::ShowActivity)));
        if signed_in {
            m.push(item("Open OneDrive Folder", "folder-symbolic", |t| t.send(TrayCmd::OpenFolder)));
            m.push(item("View Online", "web-browser-symbolic", |t| t.send(TrayCmd::ViewOnline)));
            let issues = self.model.issue_count();
            if issues > 0 {
                m.push(item(&format!("Sync Issues ({issues})"), "dialog-warning-symbolic", |t| {
                    t.send(TrayCmd::Issues)
                }));
            }
            m.push(MenuItem::Separator);
            if self.model.paused_until != 0 {
                m.push(item("Resume Syncing", "media-playback-start-symbolic", |t| t.send(TrayCmd::Resume)));
            } else {
                m.push(
                    SubMenu {
                        label: "Pause Syncing".into(),
                        icon_name: "media-playback-pause-symbolic".into(),
                        submenu: vec![
                            item("2 Hours", "", |t| t.send(TrayCmd::Pause(2 * 3600))),
                            item("8 Hours", "", |t| t.send(TrayCmd::Pause(8 * 3600))),
                            item("24 Hours", "", |t| t.send(TrayCmd::Pause(24 * 3600))),
                        ],
                        ..Default::default()
                    }
                    .into(),
                );
            }
        }
        if !self.model.update_version.is_empty() {
            m.push(item(
                &format!("Install Update ({})\u{2026}", self.model.update_version),
                "software-update-available-symbolic",
                |t| t.send(TrayCmd::Update),
            ));
        }
        m.push(item("Settings", "emblem-system-symbolic", |t| t.send(TrayCmd::Settings)));
        m.push(MenuItem::Separator);
        m.push(item("Quit Linux OneDrive", "application-exit-symbolic", |t| t.send(TrayCmd::Quit)));
        m
    }
}

/// Start the tray on the I/O runtime. Returns a handle to push model updates.
pub async fn spawn(tx: async_channel::Sender<TrayCmd>) -> Option<ksni::Handle<Tray>> {
    use ksni::TrayMethods;
    match (Tray { model: Model::default(), tx }).spawn().await {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::warn!("no tray available ({e}); the app stays reachable from the app grid");
            None
        }
    }
}
