//! First-run setup, modelled on the Windows client:
//!
//!   1. "Set up Linux OneDrive": email address
//!   2. Microsoft's sign-in page, embedded (password, MFA, passkeys…)
//!   3. "This is your OneDrive folder" (location, change if wanted)
//!   4. "All your files, ready and on-demand" (status icons explained)
//!   5. "Your OneDrive is ready" → Open my OneDrive folder
//!
//! The embedded page redirects to Microsoft's `nativeclient` URI when done;
//! we intercept that navigation and hand the URL to the daemon, which
//! redeems the code (PKCE verifier and state never leave the daemon).

use crate::client::{self};
use crate::{App, util};
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;
use webkit::prelude::*;

#[derive(Clone)]
pub struct SetupWindow {
    pub window: adw::Window,
    nav: adw::NavigationView,
    email: adw::EntryRow,
    sign_in_button: gtk::Button,
    web_page: adw::NavigationPage,
    web_holder: gtk::Box,
    web_progress: gtk::ProgressBar,
    folder_row: adw::ActionRow,
    account_path: Rc<RefCell<Option<String>>>,
    redirect: Rc<RefCell<String>>,
    /// The sign-in page's final redirect is being handled (it can arrive more than once).
    completing: Rc<std::cell::Cell<bool>>,
    /// Sign-in succeeded and the wizard moved on.
    advanced: Rc<std::cell::Cell<bool>>,
    toasts: adw::ToastOverlay,
}

fn page(title: &str, tag: &str, child: &impl IsA<gtk::Widget>) -> adw::NavigationPage {
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::new());
    tv.set_content(Some(child));
    adw::NavigationPage::builder().title(title).tag(tag).child(&tv).build()
}

fn clamp(child: &impl IsA<gtk::Widget>) -> adw::Clamp {
    adw::Clamp::builder()
        .maximum_size(440)
        .margin_start(18)
        .margin_end(18)
        .margin_top(12)
        .margin_bottom(18)
        .child(child)
        .build()
}

impl SetupWindow {
    pub fn new(app: &Rc<App>, reauth_email: Option<String>) -> SetupWindow {
        let window = adw::Window::builder()
            .title("Set up Linux OneDrive")
            .icon_name(od_core::APP_ID)
            .default_width(520)
            .default_height(700)
            .modal(false)
            .build();
        window.set_application(Some(&app.gtk));
        let nav = adw::NavigationView::new();
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&nav));
        window.set_content(Some(&toasts));

        // ---- 1. email
        let welcome = adw::StatusPage::builder()
            .icon_name(od_core::APP_ID)
            .title("Set up Linux OneDrive")
            .description("Put your files in OneDrive to get them from any device.")
            .css_classes(["setup-hero"])
            .build();
        let email = adw::EntryRow::builder().title("Email address").input_purpose(gtk::InputPurpose::Email).build();
        if let Some(e) = &reauth_email {
            email.set_text(e);
        }
        let group = adw::PreferencesGroup::new();
        group.add(&email);
        let sign_in_button = gtk::Button::builder()
            .label("Sign In")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .margin_top(18)
            .build();
        let create = gtk::LinkButton::builder().label("Create account").uri("https://signup.live.com/").build();
        let note = gtk::Label::builder()
            .label("Linux OneDrive is an unofficial client and isn't affiliated with Microsoft.")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label", "caption"])
            .margin_top(12)
            .build();
        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
        vbox.append(&welcome);
        vbox.append(&group);
        vbox.append(&sign_in_button);
        vbox.append(&create);
        vbox.append(&note);
        let scroller =
            gtk::ScrolledWindow::builder().child(&clamp(&vbox)).hscrollbar_policy(gtk::PolicyType::Never).build();
        nav.add(&page("Set up Linux OneDrive", "email", &scroller));

        // ---- 2. embedded sign-in
        let web_holder = gtk::Box::builder().orientation(gtk::Orientation::Vertical).vexpand(true).build();
        let web_progress = gtk::ProgressBar::builder().css_classes(["osd"]).build();
        let browser_link = gtk::Button::builder()
            .label("Sign in with your web browser instead")
            .css_classes(["flat"])
            .halign(gtk::Align::Center)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let web_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        web_box.append(&web_progress);
        web_box.append(&web_holder);
        web_box.append(&browser_link);
        let web_page = page("Sign in", "signin", &web_box);

        // ---- 3. folder
        let folder_row = adw::ActionRow::builder().title("OneDrive folder").subtitle("").build();
        folder_row.add_prefix(&gtk::Image::from_icon_name("folder-remote-symbolic"));
        let change = gtk::Button::builder().label("Change Location").valign(gtk::Align::Center).build();
        folder_row.add_suffix(&change);
        let fgroup = adw::PreferencesGroup::builder()
            .description("Files you add to this folder are synced to OneDrive. They appear right away and download only when you open them.")
            .build();
        fgroup.add(&folder_row);
        let next_folder = gtk::Button::builder()
            .label("Next")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .margin_top(18)
            .build();
        let fbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
        fbox.append(
            &adw::StatusPage::builder()
                .icon_name("folder")
                .title("This is your OneDrive folder")
                .css_classes(["compact"])
                .build(),
        );
        fbox.append(&fgroup);
        fbox.append(&next_folder);
        let folder_page = page("Your OneDrive Folder", "folder", &clamp(&fbox));

        // ---- 4. Files On-Demand explained
        let ogroup = adw::PreferencesGroup::new();
        for (icon, title, sub) in [
            (
                "emblem-linux-onedrive-online",
                "Online-only files",
                "Don't take up space on this device. They download when you open them.",
            ),
            (
                "emblem-linux-onedrive-local",
                "Files available on this device",
                "Files you open stay on this device, and you can free up their space at any time.",
            ),
            (
                "emblem-linux-onedrive-pinned",
                "Always available files",
                "Files you mark \"Always keep on this device\" download and stay, even when you're offline.",
            ),
        ] {
            let r = adw::ActionRow::builder().title(title).subtitle(sub).build();
            let img = gtk::Image::from_icon_name(icon);
            img.set_pixel_size(24);
            img.add_css_class("ondemand-icon");
            r.add_prefix(&img);
            ogroup.add(&r);
        }
        let next_ondemand = gtk::Button::builder()
            .label("Next")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .margin_top(18)
            .build();
        let obox = gtk::Box::new(gtk::Orientation::Vertical, 6);
        obox.append(
            &adw::StatusPage::builder()
                .icon_name("emblem-linux-onedrive-online")
                .title("All your files, ready and on-demand")
                .description("Files On-Demand is on, so your whole OneDrive is here without filling your disk. Right-click a file or folder in Files to change how it's kept.")
                .css_classes(["compact"])
                .build(),
        );
        obox.append(&ogroup);
        obox.append(&next_ondemand);
        let ondemand_page = page("Files On-Demand", "ondemand", &clamp(&obox));

        // ---- 5. done
        let open = gtk::Button::builder()
            .label("Open My OneDrive Folder")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        let done_status = adw::StatusPage::builder()
            .icon_name(od_core::APP_ID)
            .title("Your OneDrive is ready")
            .description("Syncing has started. Look for the cloud icon in the top bar to see what's happening.")
            .child(&open)
            .build();
        let done_page = page("Ready", "done", &done_status);

        let s = SetupWindow {
            window,
            nav: nav.clone(),
            email: email.clone(),
            sign_in_button: sign_in_button.clone(),
            web_page,
            web_holder,
            web_progress,
            folder_row: folder_row.clone(),
            account_path: Rc::new(RefCell::new(None)),
            redirect: Rc::new(RefCell::new(String::new())),
            completing: Default::default(),
            advanced: Default::default(),
            toasts,
        };

        // Wiring.
        let (me, a) = (s.clone(), app.clone());
        sign_in_button.connect_clicked(move |_| me.start_embedded(&a));
        let (me, a) = (s.clone(), app.clone());
        email.connect_entry_activated(move |_| me.start_embedded(&a));
        let (me, a) = (s.clone(), app.clone());
        browser_link.connect_clicked(move |_| me.start_browser(&a));
        let me = s.clone();
        change.connect_clicked(move |_| me.choose_folder());
        let (me, op, dp) = (s.clone(), ondemand_page.clone(), done_page.clone());
        next_folder.connect_clicked(move |_| me.confirm_folder(&op, &dp));
        let (n, dp) = (nav.clone(), done_page.clone());
        next_ondemand.connect_clicked(move |_| n.push(&dp));
        let (me, a) = (s.clone(), app.clone());
        open.connect_clicked(move |_| {
            if let Some(acct) = me.account_path.borrow().as_ref().and_then(|p| a.model().account(p).cloned()) {
                util::open_uri(None, &acct.mount_point);
            }
            me.window.close();
        });
        let (me, a) = (s.clone(), app.clone());
        s.window.connect_close_request(move |_| {
            *a.setup.borrow_mut() = None;
            client::call(async move { client::manager().await?.cancel_sign_in().await }, |_| {});
            let _ = &me;
            glib::Propagation::Proceed
        });
        nav.add(&folder_page);
        nav.add(&ondemand_page);
        nav.add(&done_page);
        nav.replace_with_tags(&["email"]);

        // An account that signed in but never finished setup resumes at step 3.
        if let Some(a) = app.model().accounts.into_iter().find(|a| a.needs_setup) {
            *s.account_path.borrow_mut() = Some(a.path.clone());
            s.folder_row.set_subtitle(&a.mount_point);
            nav.push_by_tag("folder");
        }
        s
    }

    fn start_embedded(&self, app: &Rc<App>) {
        if app.model().needs_client_id {
            util::error_dialog(
                &self.window,
                "Sign-in isn't configured",
                "This build has no Microsoft application ID. Set client_id in ~/.config/linux-onedrive/config.toml.",
            );
            return;
        }
        let hint = self.email.text().trim().to_string();
        self.sign_in_button.set_sensitive(false);
        let me = self.clone();
        client::call(
            async move { client::manager().await?.begin_sign_in(&hint, "embedded").await },
            move |r: zbus::Result<(String, String)>| {
                me.sign_in_button.set_sensitive(true);
                match r {
                    Ok((url, redirect)) => me.show_web(&url, &redirect),
                    Err(e) => util::error_dialog(&me.window, "Can't start sign-in", &util::friendly(&e)),
                }
            },
        );
    }

    fn start_browser(&self, app: &Rc<App>) {
        let hint = self.email.text().trim().to_string();
        let me = self.clone();
        let _ = app;
        client::call(
            async move { client::manager().await?.begin_sign_in(&hint, "browser").await },
            move |r: zbus::Result<(String, String)>| match r {
                Ok((url, _)) => {
                    util::open_uri(Some(me.window.upcast_ref()), &url);
                    util::toast(&me.toasts, "Finish signing in in your web browser");
                }
                Err(e) => util::error_dialog(&me.window, "Can't start sign-in", &util::friendly(&e)),
            },
        );
    }

    fn show_web(&self, url: &str, redirect: &str) {
        *self.redirect.borrow_mut() = redirect.to_string();
        self.completing.set(false);
        self.advanced.set(false);
        while let Some(c) = self.web_holder.first_child() {
            self.web_holder.remove(&c);
        }
        // Persistent, app-private session: re-authentication is usually one click.
        let data = od_core::paths::data_dir().join("webkit");
        let cache = od_core::paths::cache_dir().join("webkit");
        let session = webkit::NetworkSession::new(data.to_str(), cache.to_str());
        let view = webkit::WebView::builder().network_session(&session).vexpand(true).hexpand(true).build();
        if let Some(settings) = webkit::prelude::WebViewExt::settings(&view) {
            settings.set_enable_developer_extras(false);
            settings.set_javascript_can_open_windows_automatically(false);
        }
        let me = self.clone();
        view.connect_decide_policy(move |_, decision, kind| {
            let uri = match kind {
                webkit::PolicyDecisionType::NavigationAction | webkit::PolicyDecisionType::NewWindowAction => decision
                    .downcast_ref::<webkit::NavigationPolicyDecision>()
                    .and_then(|d| d.navigation_action())
                    .and_then(|a| a.request())
                    .and_then(|r| r.uri())
                    .map(|u| u.to_string()),
                _ => None,
            };
            let Some(uri) = uri else { return false };
            let redirect = me.redirect.borrow().clone();
            if !redirect.is_empty() && uri.starts_with(&redirect) {
                decision.ignore();
                me.complete(&uri);
                return true;
            }
            if kind == webkit::PolicyDecisionType::NewWindowAction {
                // Help and privacy links open in the normal browser.
                decision.ignore();
                util::open_uri(Some(me.window.upcast_ref()), &uri);
                return true;
            }
            false
        });
        let bar = self.web_progress.clone();
        view.connect_estimated_load_progress_notify(move |v| {
            let p = v.estimated_load_progress();
            bar.set_fraction(p);
            bar.set_visible(p < 1.0);
        });
        let me = self.clone();
        view.connect_load_failed(move |_, _, failing, err| {
            if !me.redirect.borrow().is_empty() && failing.starts_with(me.redirect.borrow().as_str()) {
                return true;
            }
            util::toast(&me.toasts, &format!("Couldn't load the sign-in page: {}", err.message()));
            false
        });
        view.load_uri(url);
        self.web_holder.append(&view);
        if self.nav.find_page("signin").is_none() {
            self.nav.add(&self.web_page);
        }
        self.nav.push_by_tag("signin");
    }

    fn complete(&self, redirect_url: &str) {
        // Handle the redirect once: a second delivery would be refused by
        // the service (its code is already used) and look like a failure.
        if self.completing.replace(true) {
            return;
        }
        let url = redirect_url.to_string();
        let me = self.clone();
        if url.contains("error=") {
            let parsed = url::Url::parse(&url).ok();
            let desc = parsed
                .as_ref()
                .and_then(|u| u.query_pairs().find(|(k, _)| k == "error_description").map(|(_, v)| v.into_owned()))
                .unwrap_or_default();
            let consent =
                desc.contains("AADSTS65001") || desc.contains("AADSTS90094") || desc.to_lowercase().contains("admin");
            let body = if consent {
                "Your organization requires an administrator to approve Linux OneDrive before you can use it. Ask your IT administrator to grant consent."
                    .to_string()
            } else {
                desc.lines().next().unwrap_or("Sign-in was cancelled.").to_string()
            };
            self.completing.set(false);
            self.nav.pop_to_tag("email");
            util::error_dialog(&self.window, "Couldn't sign in", &body);
            let _ = me;
            return;
        }
        client::call(
            async move { client::manager().await?.complete_sign_in(&url).await },
            move |r: zbus::Result<zbus::zvariant::OwnedObjectPath>| match r {
                Ok(path) => me.signed_in(path.as_str()),
                // Already signed in by another report of the same attempt.
                Err(_) if me.advanced.get() => {}
                Err(e) => {
                    me.completing.set(false);
                    me.nav.pop_to_tag("email");
                    util::error_dialog(&me.window, "Couldn't sign in", &util::friendly(&e));
                }
            },
        );
    }

    /// Browser / device-code flows report through this signal.
    pub fn sign_in_finished(&self, _app: &Rc<App>, ok: bool, message: &str, account: &str) {
        if ok {
            self.signed_in(account);
        } else if !message.is_empty() && !self.advanced.get() {
            util::error_dialog(&self.window, "Couldn't sign in", message);
        }
    }

    fn signed_in(&self, path: &str) {
        // Reported both by the call's reply and by the service's signal.
        if self.advanced.replace(true) {
            return;
        }
        *self.account_path.borrow_mut() = Some(path.to_string());
        let me = self.clone();
        let p = path.to_string();
        client::call(
            async move {
                let a = client::account(&p).await?;
                Ok::<_, zbus::Error>((a.needs_setup().await?, a.mount_point().await?))
            },
            move |r| match r {
                Ok((true, mount)) => {
                    me.folder_row.set_subtitle(&mount);
                    me.nav.push_by_tag("folder");
                }
                Ok((false, _)) => {
                    // Re-authentication of an existing account: done.
                    util::toast(&me.toasts, "You're signed in again");
                    let w = me.window.clone();
                    glib::timeout_add_seconds_local_once(1, move || w.close());
                }
                Err(e) => {
                    me.advanced.set(false);
                    util::error_dialog(&me.window, "Couldn't finish sign-in", &util::friendly(&e));
                }
            },
        );
    }

    fn choose_folder(&self) {
        let dialog = gtk::FileDialog::builder().title("Choose your OneDrive folder location").modal(true).build();
        let me = self.clone();
        dialog.select_folder(Some(&self.window), gio::Cancellable::NONE, move |r| {
            if let Ok(f) = r
                && let Some(p) = f.path()
            {
                // The OneDrive folder is created inside the chosen location.
                let current = me.folder_row.subtitle().map(|s| s.to_string()).unwrap_or_default();
                let base = std::path::Path::new(&current)
                    .file_name()
                    .map(|n| n.to_owned())
                    .unwrap_or_else(|| "OneDrive".into());
                me.folder_row.set_subtitle(&p.join(base).to_string_lossy());
            }
        });
    }

    fn confirm_folder(&self, ondemand: &adw::NavigationPage, _done: &adw::NavigationPage) {
        let Some(path) = self.account_path.borrow().clone() else { return };
        let mount = self.folder_row.subtitle().map(|s| s.to_string()).unwrap_or_default();
        let me = self.clone();
        let od = ondemand.clone();
        client::call(
            async move {
                let a = client::account(&path).await?;
                if a.mount_point().await? != mount {
                    a.set_mount_point(&mount).await?;
                }
                a.start().await
            },
            move |r: zbus::Result<()>| match r {
                Ok(()) => me.nav.push(&od),
                Err(e) => util::error_dialog(&me.window, "Couldn't set up the folder", &util::friendly(&e)),
            },
        );
    }
}
