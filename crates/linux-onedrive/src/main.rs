//! linux-onedrive: GTK4/libadwaita app (activity panel, setup, settings)
//! and tray icon. A thin client of the linux-onedrived daemon.

mod activity;
mod client;
mod dialogs;
mod locations;
mod prefs;
mod setup;
mod share;
mod tray;
mod update;
mod util;

use adw::prelude::*;
use client::{Model, UiEvent};
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;
use tray::TrayCmd;

/// Application state shared by all windows (GTK thread only).
pub struct App {
    pub gtk: adw::Application,
    pub model: RefCell<Model>,
    /// Account shown in the activity panel (object path).
    pub current: RefCell<Option<String>>,
    pub activity: RefCell<Option<activity::ActivityWindow>>,
    pub setup: RefCell<Option<setup::SetupWindow>>,
    pub tray: RefCell<Option<ksni::Handle<tray::Tray>>>,
    hold: RefCell<Option<gio::ApplicationHoldGuard>>,
    /// Page requested before the model arrived ("--show settings" at startup).
    pending_show: RefCell<Option<String>>,
    first_model: RefCell<bool>,
}

impl App {
    pub fn model(&self) -> Model {
        self.model.borrow().clone()
    }

    /// The account the panel shows (falls back to the first one).
    pub fn current_account(&self) -> Option<client::AccountView> {
        let m = self.model.borrow();
        let cur = self.current.borrow().clone();
        cur.and_then(|p| m.account(&p).cloned()).or_else(|| m.accounts.first().cloned())
    }

    /// The activity panel, if it is on screen. It hides when closed, and a
    /// dialog attached to a hidden window would never appear, so callers
    /// get `None` then and present their dialog on its own.
    pub fn window(&self) -> Option<gtk::Window> {
        self.activity.borrow().as_ref().map(|a| a.window.clone().upcast::<gtk::Window>()).filter(|w| w.is_visible())
    }

    pub fn show_activity(self: &Rc<Self>) {
        let existing = self.activity.borrow().clone();
        let w = match existing {
            Some(w) => w,
            None => {
                let w = activity::ActivityWindow::new(self);
                *self.activity.borrow_mut() = Some(w.clone());
                w.update(&self.model(), self.current_account());
                w
            }
        };
        w.window.present();
        w.reload_activity(self);
    }

    pub fn show_setup(self: &Rc<Self>, reauth_email: Option<String>) {
        let existing = self.setup.borrow().clone();
        match existing {
            Some(s) => s.window.present(),
            None => {
                let s = setup::SetupWindow::new(self, reauth_email);
                *self.setup.borrow_mut() = Some(s.clone());
                s.window.present();
            }
        }
    }

    /// Handle `--show <page>` and tray/notification requests.
    pub fn show_page(self: &Rc<Self>, page: &str) {
        if !*self.first_model.borrow() {
            *self.pending_show.borrow_mut() = Some(page.to_string());
            return;
        }
        let (kind, arg) = page.split_once(':').unwrap_or((page, ""));
        match kind {
            "activity" | "" => self.show_activity(),
            "settings" => prefs::show(self, self.window().as_ref()),
            "issues" => dialogs::issues(self, arg),
            "folder" => {
                if let Some(a) = self.current_account() {
                    util::open_uri(None, &a.mount_point);
                }
            }
            "signin" => {
                let email = self.model.borrow().account(arg).map(|a| a.email.clone());
                self.show_setup(email);
            }
            "mass-delete" => dialogs::mass_delete(self, arg),
            "share" => dialogs::share(self, arg),
            "manage-access" => dialogs::manage_access(self, arg),
            "copy-link" => dialogs::copy_link_standalone(self, arg),
            "versions" => dialogs::versions(self, arg),
            "choose-folders" => dialogs::choose_folders(self, arg),
            "removed" => dialogs::recently_removed(self, arg),
            "update" => update::dialog(self, self.window().map(|w| w.upcast())),
            "odopen" => locations::open_sync_link(self, &format!("odopen:{arg}")),
            other => tracing::warn!("unknown page {other}"),
        }
    }

    fn on_event(self: &Rc<Self>, ev: UiEvent) {
        match ev {
            UiEvent::Model(m) => self.on_model(m),
            UiEvent::Activity { account, entry } => {
                if let Some(w) = self.activity.borrow().as_ref() {
                    w.add_activity(self, &account, entry);
                }
            }
            UiEvent::SignInFinished { ok, message, account } => {
                if let Some(s) = self.setup.borrow().as_ref() {
                    s.sign_in_finished(self, ok, &message, &account);
                }
            }
            UiEvent::DeleteReminder { account } => dialogs::delete_reminder(self, &account),
        }
    }

    fn on_model(self: &Rc<Self>, m: Model) {
        let first = !*self.first_model.borrow() && m.connected;
        *self.model.borrow_mut() = m.clone();
        if let Some(w) = self.activity.borrow().as_ref() {
            w.update(&m, self.current_account());
        }
        if let Some(h) = self.tray.borrow().clone() {
            let tm = m.clone();
            client::runtime().spawn(async move {
                h.update(|t| t.model = tm).await;
            });
        }
        if first {
            *self.first_model.borrow_mut() = true;
            // First run: no account yet → open setup, like Windows does after install.
            let pending = self.pending_show.borrow_mut().take();
            if m.accounts.is_empty() {
                self.show_setup(None);
            } else if let Some(a) = m.accounts.iter().find(|a| a.needs_setup) {
                let _ = a;
                self.show_setup(None);
            } else if let Some(p) = pending {
                self.show_page(&p);
            }
        }
    }

    fn on_tray(self: &Rc<Self>, cmd: TrayCmd) {
        match cmd {
            TrayCmd::ShowActivity => self.show_activity(),
            TrayCmd::OpenFolder => self.show_page("folder"),
            TrayCmd::ViewOnline => {
                if let Some(a) = self.current_account() {
                    util::open_uri(None, &a.web_url);
                }
            }
            TrayCmd::Pause(s) => client::call(async move { client::manager().await?.pause(s).await }, |_| {}),
            TrayCmd::Resume => client::call(async move { client::manager().await?.resume().await }, |_| {}),
            TrayCmd::Settings => self.show_page("settings"),
            TrayCmd::Update => self.show_page("update"),
            TrayCmd::Issues => self.show_page("issues"),
            TrayCmd::SignIn => self.show_setup(None),
            TrayCmd::Quit => self.confirm_quit(),
        }
    }

    /// "Quit Linux OneDrive": like Windows, quitting stops syncing.
    pub fn confirm_quit(self: &Rc<Self>) {
        let d = adw::AlertDialog::new(
            Some("Quit Linux OneDrive?"),
            Some(
                "Your files won't sync until you open Linux OneDrive again. Files that are only online can't be opened while it's closed.",
            ),
        );
        d.add_responses(&[("cancel", "Cancel"), ("quit", "Quit")]);
        d.set_response_appearance("quit", adw::ResponseAppearance::Destructive);
        d.set_default_response(Some("cancel"));
        let app = self.clone();
        d.connect_response(None, move |_, r| {
            if r == "quit" {
                let gtk_app = app.gtk.clone();
                client::call(async move { client::manager().await?.quit().await }, move |_: zbus::Result<()>| {
                    gtk_app.quit();
                });
            }
        });
        let parent = self.window();
        match parent {
            Some(p) => d.present(Some(&p)),
            None => {
                // No window yet: present on a transient activity window.
                self.show_activity();
                let w = self.window();
                d.present(w.as_ref());
            }
        }
    }

    fn install_actions(self: &Rc<Self>) {
        let add = |name: &str, f: Box<dyn Fn(&Rc<App>, Option<&glib::Variant>)>, ty: Option<&glib::VariantTy>| {
            let a = gio::SimpleAction::new(name, ty);
            let app = self.clone();
            a.connect_activate(move |_, p| f(&app, p));
            self.gtk.add_action(&a);
        };
        add("show-activity", Box::new(|a, _| a.show_activity()), None);
        add("open-folder", Box::new(|a, _| a.show_page("folder")), None);
        add(
            "view-online",
            Box::new(|a, _| {
                if let Some(acc) = a.current_account() {
                    util::open_uri(a.window().as_ref(), &acc.web_url);
                }
            }),
            None,
        );
        add(
            "recycle-bin",
            Box::new(|a, _| {
                let url = match a.current_account() {
                    Some(acc) if acc.drive_type == "personal" => "https://onedrive.live.com/?v=recyclebin".to_string(),
                    Some(acc) => format!(
                        "{}/_layouts/15/RecycleBin.aspx",
                        acc.web_url
                            .trim_end_matches('/')
                            .rsplit_once("/Documents")
                            .map(|(b, _)| b)
                            .unwrap_or(&acc.web_url)
                    ),
                    None => "https://onedrive.live.com/?v=recyclebin".to_string(),
                };
                util::open_uri(a.window().as_ref(), &url);
            }),
            None,
        );
        add("settings", Box::new(|a, _| a.show_page("settings")), None);
        add("recently-removed", Box::new(|a, _| a.show_page("removed")), None);
        add("issues", Box::new(|a, _| a.show_page("issues")), None);
        add("add-account", Box::new(|a, _| a.show_setup(None)), None);
        add(
            "pause",
            Box::new(|_, p| {
                let secs = p.and_then(|v| v.get::<u32>()).unwrap_or(7200);
                client::call(async move { client::manager().await?.pause(secs).await }, |_| {});
            }),
            Some(glib::VariantTy::UINT32),
        );
        add(
            "resume",
            Box::new(|_, _| client::call(async move { client::manager().await?.resume().await }, |_| {})),
            None,
        );
        add(
            "switch-account",
            Box::new(|a, p| {
                if let Some(path) = p.and_then(|v| v.get::<String>()) {
                    *a.current.borrow_mut() = Some(path);
                    if let Some(w) = a.activity.borrow().as_ref() {
                        w.update(&a.model(), a.current_account());
                        w.reload_activity(a);
                    }
                }
            }),
            Some(glib::VariantTy::STRING),
        );
        add("quit-all", Box::new(|a, _| a.confirm_quit()), None);
        add("about", Box::new(|a, _| dialogs::about(a)), None);
        add(
            "help",
            Box::new(|a, _| {
                util::open_uri(a.window().as_ref(), "https://github.com/haimairdataiot/LinuxOneDriveClient")
            }),
            None,
        );
        self.gtk.set_accels_for_action("app.settings", &["<Control>comma"]);
        self.gtk.set_accels_for_action("app.quit-all", &["<Control>q"]);
    }
}

#[derive(clap::Parser, Debug)]
#[command(name = "linux-onedrive", version, about = "Linux OneDrive")]
struct Args {
    /// Start with only the tray icon (used at login).
    #[arg(long)]
    background: bool,
    /// Show a page: activity, settings, issues, folder, update, signin:<account>,
    /// mass-delete:<account>, share:<path>, versions:<path>, choose-folders:<account>.
    #[arg(long)]
    show: Option<String>,
    /// Links to open: `odopen://` (a SharePoint library's Sync button).
    urls: Vec<String>,
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    // --help, --version and bad arguments are answered here: once the
    // application is running it stays (tray), so they would never exit.
    if let Err(e) = <Args as clap::Parser>::try_parse() {
        e.exit();
    }
    let gtk_app = adw::Application::builder()
        .application_id(od_core::APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    let state: Rc<RefCell<Option<Rc<App>>>> = Rc::new(RefCell::new(None));

    let st = state.clone();
    gtk_app.connect_startup(move |gtk_app| {
        util::install_icon_search_path();
        let css = gtk::CssProvider::new();
        css.load_from_string(include_str!("style.css"));
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
        let app = Rc::new(App {
            gtk: gtk_app.clone(),
            model: RefCell::new(Model::default()),
            current: RefCell::new(None),
            activity: RefCell::new(None),
            setup: RefCell::new(None),
            tray: RefCell::new(None),
            hold: RefCell::new(Some(gtk_app.hold())),
            pending_show: RefCell::new(None),
            first_model: RefCell::new(false),
        });
        app.install_actions();

        // Daemon model → UI.
        let (tx, rx) = async_channel::unbounded::<UiEvent>();
        client::runtime().spawn(client::watch(tx));
        let a = app.clone();
        glib::spawn_future_local(async move {
            while let Ok(ev) = rx.recv().await {
                a.on_event(ev);
            }
        });

        // Tray → UI.
        let (ttx, trx) = async_channel::unbounded::<TrayCmd>();
        let a = app.clone();
        client::call(tray::spawn(ttx), move |h| *a.tray.borrow_mut() = h);
        let a = app.clone();
        glib::spawn_future_local(async move {
            while let Ok(cmd) = trx.recv().await {
                a.on_tray(cmd);
            }
        });
        *st.borrow_mut() = Some(app);
    });

    let st = state.clone();
    gtk_app.connect_command_line(move |_, cmdline| {
        let args: Vec<String> = cmdline.arguments().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let parsed = match <Args as clap::Parser>::try_parse_from(&args) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("{e}");
                return glib::ExitCode::from(if e.use_stderr() { 2 } else { 0 });
            }
        };
        if let Some(app) = st.borrow().clone() {
            let links: Vec<&String> = parsed.urls.iter().filter(|u| u.starts_with("odopen:")).collect();
            for u in &links {
                app.show_page(&format!("odopen:{}", u.trim_start_matches("odopen:")));
            }
            if !links.is_empty() {
                return glib::ExitCode::SUCCESS;
            }
            if let Some(page) = parsed.show {
                app.show_page(&page);
            } else if !parsed.background {
                if *app.first_model.borrow() {
                    app.show_activity();
                } else {
                    *app.pending_show.borrow_mut() = Some("activity".into());
                }
            }
        }
        glib::ExitCode::SUCCESS
    });

    let st = state.clone();
    gtk_app.connect_shutdown(move |_| {
        if let Some(app) = st.borrow().as_ref() {
            app.hold.borrow_mut().take();
        }
    });

    gtk_app.run()
}
