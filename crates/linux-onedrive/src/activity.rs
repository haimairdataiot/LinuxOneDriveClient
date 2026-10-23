//! The activity panel: the Linux equivalent of the Windows tray flyout.
//!
//! Header (account switcher · open folder · view online · help & settings),
//! a status card with progress, banners for things needing attention, the
//! recent activity list and a storage meter. With more than one account the
//! title is a dropdown that chooses which account the panel shows.

use crate::client::{self, AccountView, Model};
use crate::{App, dialogs, util};
use adw::prelude::*;
use gtk::{gio, glib};
use od_core::dbus::ActivityEntry;
use od_core::types::ActivityKind;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Clone)]
pub struct ActivityWindow {
    pub window: adw::ApplicationWindow,
    title: adw::WindowTitle,
    /// Takes the title's place when there is more than one account.
    account_picker: gtk::DropDown,
    /// The accounts the dropdown lists, for its rows to read.
    picker_accounts: Rc<RefCell<Vec<AccountView>>>,
    /// Set while the dropdown is changed from the model, not by the user.
    picker_quiet: Rc<Cell<bool>>,
    account_button: gtk::MenuButton,
    gear: gtk::MenuButton,
    banner: adw::Banner,
    banner_action: Rc<RefCell<Option<Box<dyn Fn()>>>>,
    status_card: gtk::Box,
    status_icon: gtk::Image,
    status_title: gtk::Label,
    status_detail: gtk::Label,
    progress: gtk::ProgressBar,
    stack: gtk::Stack,
    list: gtk::ListBox,
    storage_label: gtk::Label,
    storage_bar: gtk::LevelBar,
    footer: gtk::Box,
    pub toasts: adw::ToastOverlay,
    entries: Rc<RefCell<Vec<ActivityEntry>>>,
    loaded_for: Rc<RefCell<Option<String>>>,
}

impl ActivityWindow {
    pub fn new(app: &Rc<App>) -> ActivityWindow {
        let window = adw::ApplicationWindow::builder()
            .application(&app.gtk)
            .title(od_core::APP_NAME)
            .icon_name(od_core::APP_ID)
            .default_width(420)
            .default_height(640)
            .width_request(360)
            .height_request(480)
            .hide_on_close(true)
            .build();

        // ---- header
        let header = adw::HeaderBar::new();
        let title = adw::WindowTitle::new(od_core::APP_NAME, "");
        let picker_accounts: Rc<RefCell<Vec<AccountView>>> = Rc::new(RefCell::new(Vec::new()));
        let account_picker = gtk::DropDown::builder()
            .model(&gtk::StringList::new(&[]))
            .factory(&Self::picker_factory(&picker_accounts, false))
            .list_factory(&Self::picker_factory(&picker_accounts, true))
            .tooltip_text("Choose an account")
            .visible(false)
            .build();
        account_picker.update_property(&[gtk::accessible::Property::Label("Account")]);
        let picker_quiet = Rc::new(Cell::new(false));
        let quiet = picker_quiet.clone();
        account_picker.connect_selected_notify(move |picker| {
            if quiet.get() {
                return;
            }
            if let Some(item) = picker.selected_item().and_downcast::<gtk::StringObject>() {
                let path = item.string().to_string();
                let _ = WidgetExt::activate_action(picker, "app.switch-account", Some(&path.to_variant()));
            }
        });
        let titles = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        titles.append(&title);
        titles.append(&account_picker);
        header.set_title_widget(Some(&titles));
        let account_button =
            gtk::MenuButton::builder().icon_name("avatar-default-symbolic").tooltip_text("Accounts").build();
        header.pack_start(&account_button);
        let gear = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Help & Settings")
            .primary(true)
            .build();
        header.pack_end(&gear);
        let online = gtk::Button::builder()
            .icon_name("web-browser-symbolic")
            .tooltip_text("View online")
            .action_name("app.view-online")
            .build();
        header.pack_end(&online);
        let folder = gtk::Button::builder()
            .icon_name("folder-symbolic")
            .tooltip_text("Open folder")
            .action_name("app.open-folder")
            .build();
        header.pack_end(&folder);

        // ---- banner
        let banner = adw::Banner::new("");
        let banner_action: Rc<RefCell<Option<Box<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let ba = banner_action.clone();
        banner.connect_button_clicked(move |_| {
            if let Some(f) = ba.borrow().as_ref() {
                f();
            }
        });

        // ---- status card
        let status_icon = gtk::Image::builder().pixel_size(64).css_classes(["status-icon"]).build();
        let status_title =
            gtk::Label::builder().css_classes(["title-2"]).wrap(true).justify(gtk::Justification::Center).build();
        let status_detail =
            gtk::Label::builder().css_classes(["dim-label"]).wrap(true).justify(gtk::Justification::Center).build();
        let progress = gtk::ProgressBar::builder().margin_top(6).visible(false).build();
        let status_card =
            gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).css_classes(["status-card"]).build();
        status_card.append(&status_icon);
        status_card.append(&status_title);
        status_card.append(&status_detail);
        status_card.append(&progress);

        // ---- recent activity
        let heading = gtk::Label::builder()
            .label("Recent")
            .halign(gtk::Align::Start)
            .css_classes(["heading", "activity-heading"])
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(12)
            .valign(gtk::Align::Start)
            .build();
        let empty = adw::StatusPage::builder()
            .icon_name("document-open-recent-symbolic")
            .title("No recent activity")
            .description("Files you add or change will show up here.")
            .css_classes(["compact"])
            .build();
        let scroller =
            gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&list).build();
        let stack = gtk::Stack::new();
        stack.add_named(&scroller, Some("list"));
        stack.add_named(&empty, Some("empty"));

        let signed_out = adw::StatusPage::builder()
            .icon_name(od_core::APP_ID)
            .title("Set up Linux OneDrive")
            .description("Sign in with your Microsoft account to see your OneDrive files here.")
            .build();
        let sign_in = gtk::Button::builder()
            .label("Sign In")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .action_name("app.add-account")
            .build();
        signed_out.set_child(Some(&sign_in));
        stack.add_named(&signed_out, Some("signed-out"));

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&banner);
        content.append(&status_card);
        content.append(&heading);
        content.append(&stack);

        // ---- footer: storage
        let storage_label =
            gtk::Label::builder().halign(gtk::Align::Start).css_classes(["caption"]).hexpand(true).build();
        let more = gtk::LinkButton::builder()
            .label("Get more storage")
            .uri("https://www.microsoft.com/microsoft-365/onedrive/compare-onedrive-plans")
            .css_classes(["caption"])
            .build();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&storage_label);
        row.append(&more);
        let storage_bar = gtk::LevelBar::builder().min_value(0.0).max_value(1.0).build();
        storage_bar.add_offset_value(gtk::LEVEL_BAR_OFFSET_LOW, 0.80);
        storage_bar.add_offset_value(gtk::LEVEL_BAR_OFFSET_HIGH, 0.95);
        storage_bar.add_offset_value(gtk::LEVEL_BAR_OFFSET_FULL, 1.0);
        let footer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .css_classes(["storage-footer"])
            .build();
        footer.append(&row);
        footer.append(&storage_bar);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));
        toolbar.add_bottom_bar(&footer);
        toolbar.set_bottom_bar_style(adw::ToolbarStyle::Raised);
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&toolbar));
        window.set_content(Some(&toasts));

        let w = ActivityWindow {
            window,
            title,
            account_picker,
            picker_accounts,
            picker_quiet,
            account_button,
            gear,
            banner,
            banner_action,
            status_card,
            status_icon,
            status_title,
            status_detail,
            progress,
            stack,
            list,
            storage_label,
            storage_bar,
            footer,
            toasts,
            entries: Rc::new(RefCell::new(Vec::new())),
            loaded_for: Rc::new(RefCell::new(None)),
        };
        // Keep relative times fresh.
        let w2 = w.clone();
        glib::timeout_add_seconds_local(60, move || {
            w2.render_list();
            glib::ControlFlow::Continue
        });
        w
    }

    /// Rows of the account dropdown, which are object paths of accounts. The
    /// button reads like the window title (who, which OneDrive); the list
    /// adds the address, so two accounts of one person can be told apart.
    fn picker_factory(accounts: &Rc<RefCell<Vec<AccountView>>>, in_list: bool) -> gtk::SignalListItemFactory {
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
            if in_list {
                let name = gtk::Label::builder().xalign(0.0).build();
                let detail = gtk::Label::builder().xalign(0.0).css_classes(["caption", "dim-label"]).build();
                let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).hexpand(true).build();
                text.append(&name);
                text.append(&detail);
                // The account shown has a check mark, like the rows of a plain dropdown.
                let check = gtk::Image::from_icon_name("object-select-symbolic");
                item.bind_property("selected", &check, "opacity")
                    .transform_to(|_, selected: bool| Some(if selected { 1.0f64 } else { 0.0 }))
                    .sync_create()
                    .build();
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
                row.append(&text);
                row.append(&check);
                item.set_child(Some(&row));
            } else {
                item.set_child(Some(&adw::WindowTitle::new("", "")));
            }
        });
        let accounts = accounts.clone();
        factory.connect_bind(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
            let Some(path) = item.item().and_downcast::<gtk::StringObject>() else { return };
            let accounts = accounts.borrow();
            let Some(a) = accounts.iter().find(|a| a.path == path.string().as_str()) else { return };
            let child = item.child();
            if let Some(title) = child.and_downcast_ref::<adw::WindowTitle>() {
                title.set_title(&Self::who(a));
                title.set_subtitle(&format!("OneDrive \u{2013} {}", a.title()));
            } else if let Some(row) = child.and_downcast_ref::<gtk::Box>() {
                let detail =
                    if a.display_name.is_empty() { a.title() } else { format!("{} \u{b7} {}", a.email, a.title()) };
                let text = row.first_child();
                if let Some(name) = text.as_ref().and_then(|t| t.first_child()).and_downcast::<gtk::Label>() {
                    name.set_label(&Self::who(a));
                }
                if let Some(label) = text.and_then(|t| t.last_child()).and_downcast::<gtk::Label>() {
                    label.set_label(&detail);
                }
            }
        });
        factory
    }

    /// The account's owner as shown at the top of the panel.
    fn who(a: &AccountView) -> String {
        if a.display_name.is_empty() { a.email.clone() } else { a.display_name.clone() }
    }

    /// Offer the account dropdown when there is more than one account, and
    /// keep it on the account shown.
    fn update_picker(&self, model: &Model, shown: Option<&AccountView>) {
        let show = model.connected && model.accounts.len() > 1 && shown.is_some();
        self.account_picker.set_visible(show);
        self.title.set_visible(!show);
        if !show {
            return;
        }
        // Only a different list of accounts rebuilds the dropdown; status
        // updates arrive all the time and must not close it under the pointer.
        let changed = {
            let old = self.picker_accounts.borrow();
            old.len() != model.accounts.len()
                || old.iter().zip(&model.accounts).any(|(o, n)| {
                    o.path != n.path
                        || o.email != n.email
                        || o.display_name != n.display_name
                        || o.organization != n.organization
                })
        };
        *self.picker_accounts.borrow_mut() = model.accounts.clone();
        let paths: Vec<&str> = model.accounts.iter().map(|a| a.path.as_str()).collect();
        let pos = shown.and_then(|a| paths.iter().position(|p| *p == a.path)).unwrap_or(0) as u32;
        self.picker_quiet.set(true);
        if changed {
            self.account_picker.set_model(Some(&gtk::StringList::new(&paths)));
        }
        if self.account_picker.selected() != pos {
            self.account_picker.set_selected(pos);
        }
        self.picker_quiet.set(false);
    }

    fn gear_menu(model: &Model) -> gio::Menu {
        let menu = gio::Menu::new();
        let s1 = gio::Menu::new();
        s1.append(Some("Settings"), Some("app.settings"));
        s1.append(Some("View Sync Issues"), Some("app.issues"));
        s1.append(Some("Recently Removed Files"), Some("app.recently-removed"));
        s1.append(Some("Recycle Bin"), Some("app.recycle-bin"));
        menu.append_section(None, &s1);
        let s2 = gio::Menu::new();
        if model.paused_until != 0 {
            s2.append(Some("Resume Syncing"), Some("app.resume"));
        } else {
            let pause = gio::Menu::new();
            for (label, secs) in [("2 Hours", 2u32 * 3600), ("8 Hours", 8 * 3600), ("24 Hours", 24 * 3600)] {
                let item = gio::MenuItem::new(Some(label), None);
                item.set_action_and_target_value(Some("app.pause"), Some(&secs.to_variant()));
                pause.append_item(&item);
            }
            s2.append_submenu(Some("Pause Syncing"), &pause);
        }
        menu.append_section(None, &s2);
        let s3 = gio::Menu::new();
        s3.append(Some("Help"), Some("app.help"));
        s3.append(Some("About Linux OneDrive"), Some("app.about"));
        menu.append_section(None, &s3);
        let s4 = gio::Menu::new();
        s4.append(Some("Quit Linux OneDrive"), Some("app.quit-all"));
        menu.append_section(None, &s4);
        menu
    }

    fn account_menu(model: &Model) -> gio::Menu {
        let menu = gio::Menu::new();
        let accts = gio::Menu::new();
        for a in &model.accounts {
            let item = gio::MenuItem::new(Some(&format!("{} ({})", a.email, a.title())), None);
            item.set_action_and_target_value(Some("app.switch-account"), Some(&a.path.to_variant()));
            accts.append_item(&item);
        }
        menu.append_section(None, &accts);
        let more = gio::Menu::new();
        more.append(Some("Add an Account…"), Some("app.add-account"));
        menu.append_section(None, &more);
        menu
    }

    /// Re-render everything from a new model; `acct` is the account shown.
    pub fn update(&self, model: &Model, acct: Option<AccountView>) {
        self.gear.set_menu_model(Some(&Self::gear_menu(model)));
        self.account_button.set_menu_model(Some(&Self::account_menu(model)));

        for c in ["idle", "syncing", "paused", "offline", "error", "signin-required", "signed-out", "starting"] {
            self.status_card.remove_css_class(&format!("state-{c}"));
        }
        self.update_picker(model, acct.as_ref());
        let Some(a) = acct.filter(|_| model.connected) else {
            self.title.set_title(od_core::APP_NAME);
            self.title.set_subtitle(if model.connected { "Not signed in" } else { "Connecting…" });
            self.status_card.set_visible(false);
            self.footer.set_visible(false);
            self.stack.set_visible_child_name("signed-out");
            self.banner.set_revealed(model.connected && model.needs_client_id);
            if model.needs_client_id {
                self.banner.set_title("No Microsoft application ID is configured; sign-in won't work yet.");
                self.banner.set_button_label(None);
            }
            return;
        };
        self.status_card.set_visible(true);
        self.footer.set_visible(true);
        // Header: who and which OneDrive (the window title already names the app).
        self.title.set_title(&Self::who(&a));
        self.title.set_subtitle(&format!("OneDrive \u{2013} {}", a.title()));
        self.status_card.add_css_class(&format!("state-{}", a.state));
        self.status_icon.set_icon_name(Some(util::state_icon(&a.state)));
        self.status_title.set_label(util::status_title(&a.state));
        let detail = if a.state == "idle" {
            let paused = model.paused_until;
            if paused > 0 {
                format!(
                    "Paused until {}",
                    chrono::DateTime::from_timestamp(paused, 0)
                        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
                        .unwrap_or_default()
                )
            } else {
                String::new()
            }
        } else if a.state == "paused" && model.paused_until > 0 {
            let until = chrono::DateTime::from_timestamp(model.paused_until, 0)
                .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
                .unwrap_or_default();
            format!("Resumes at {until}")
        } else {
            a.status_text.clone()
        };
        self.status_detail.set_label(&detail);
        self.status_detail.set_visible(!detail.is_empty() && detail != util::status_title(&a.state));
        let p = &a.progress;
        if a.state == "syncing" && p.bytes_total > 0 {
            self.progress.set_visible(true);
            self.progress.set_fraction((p.bytes_done as f64 / p.bytes_total as f64).clamp(0.0, 1.0));
        } else if a.state == "syncing" {
            self.progress.set_visible(true);
            self.progress.pulse();
        } else {
            self.progress.set_visible(false);
        }
        self.update_storage(&a);
        self.update_banner(model, &a);
        if self.loaded_for.borrow().as_deref() != Some(a.path.as_str()) {
            *self.loaded_for.borrow_mut() = Some(a.path.clone());
            // Another account: nothing of the previous one may stay on screen.
            self.entries.borrow_mut().clear();
            self.load_activity(&a.path);
        } else {
            self.render_list();
        }
    }

    fn update_storage(&self, a: &AccountView) {
        if a.quota_total == 0 {
            self.storage_label.set_label("Checking storage…");
            self.storage_bar.set_value(0.0);
            return;
        }
        self.storage_label.set_label(&format!(
            "{} of {} used",
            od_core::fmt::size(a.quota_used),
            od_core::fmt::size(a.quota_total)
        ));
        self.storage_bar.set_value((a.quota_used as f64 / a.quota_total as f64).clamp(0.0, 1.0));
    }

    fn update_banner(&self, model: &Model, a: &AccountView) {
        let path = a.path.clone();
        let window = self.window.clone();
        let (title, button, action): (String, Option<&str>, Option<Box<dyn Fn()>>) = if a.held_deletes > 0 {
            (
                format!("You deleted {} files. They're still online until you confirm.", a.held_deletes),
                Some("Review"),
                Some(Box::new(move || {
                    let _ = WidgetExt::activate_action(&window, "app.show-activity", None);
                    crate::dialogs::mass_delete_for_window(&window, &path);
                })),
            )
        } else if a.state == "signin-required" {
            (
                "Your session expired. Sign in to keep syncing.".into(),
                Some("Sign In"),
                Some(Box::new(move || {
                    let _ = WidgetExt::activate_action(&window, "app.add-account", None);
                })),
            )
        } else if a.needs_setup {
            (
                "Finish setting up this account to start syncing.".into(),
                Some("Continue"),
                Some(Box::new(move || {
                    let _ = WidgetExt::activate_action(&window, "app.add-account", None);
                })),
            )
        } else if a.issue_count > 0 {
            (
                format!("{} item{} can't be synced.", a.issue_count, if a.issue_count == 1 { "" } else { "s" }),
                Some("View"),
                Some(Box::new(move || {
                    let _ = WidgetExt::activate_action(&window, "app.issues", None);
                })),
            )
        } else if model.paused_until != 0 {
            (
                "Syncing is paused.".into(),
                Some("Resume"),
                Some(Box::new(move || {
                    let _ = WidgetExt::activate_action(&window, "app.resume", None);
                })),
            )
        } else {
            (String::new(), None, None)
        };
        self.banner.set_title(&title);
        self.banner.set_button_label(button);
        *self.banner_action.borrow_mut() = action;
        self.banner.set_revealed(!title.is_empty());
    }

    /// Fetch the recent activity of the shown account.
    pub fn reload_activity(&self, app: &Rc<App>) {
        if let Some(a) = app.current_account() {
            *self.loaded_for.borrow_mut() = Some(a.path.clone());
            self.load_activity(&a.path);
        }
    }

    fn load_activity(&self, path: &str) {
        let path = path.to_string();
        let me = self.clone();
        let wanted = path.clone();
        client::call(
            async move { client::account(&path).await?.recent_activity(50).await },
            move |r: zbus::Result<String>| {
                // A late answer for an account that is no longer shown is dropped.
                if me.loaded_for.borrow().as_deref() != Some(wanted.as_str()) {
                    return;
                }
                if let Ok(json) = r {
                    *me.entries.borrow_mut() = serde_json::from_str(&json).unwrap_or_default();
                }
                me.render_list();
            },
        );
    }

    pub fn add_activity(&self, _app: &Rc<App>, account: &str, entry: ActivityEntry) {
        if self.loaded_for.borrow().as_deref() != Some(account) {
            return;
        }
        let mut e = self.entries.borrow_mut();
        e.retain(|x| x.id != entry.id);
        e.insert(0, entry);
        e.truncate(50);
        drop(e);
        self.render_list();
    }

    fn render_list(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let entries = self.entries.borrow().clone();
        if self.stack.visible_child_name().as_deref() == Some("signed-out") && self.loaded_for.borrow().is_none() {
            return;
        }
        if entries.is_empty() {
            self.stack.set_visible_child_name("empty");
            return;
        }
        self.stack.set_visible_child_name("list");
        for e in entries.iter().take(50) {
            self.list.append(&self.row(e));
        }
    }

    fn row(&self, e: &ActivityEntry) -> adw::ActionRow {
        let subtitle = format!("{} {} · {}", e.kind.verb(), e.folder, util::relative_time(e.time));
        let row = adw::ActionRow::builder()
            .title(&e.name)
            .subtitle(&subtitle)
            .use_markup(false)
            .activatable(!matches!(e.kind, ActivityKind::Deleted | ActivityKind::DeletedRemote))
            .build();
        row.set_title_lines(1);
        let icon = gtk::Image::from_gicon(&util::file_icon(&e.name, false));
        icon.set_pixel_size(32);
        row.add_prefix(&icon);
        if !matches!(e.kind, ActivityKind::Deleted | ActivityKind::DeletedRemote) {
            let menu = gtk::MenuButton::builder()
                .icon_name("view-more-symbolic")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .tooltip_text("More")
                .build();
            let pop = gtk::Popover::new();
            let vbox = gtk::Box::new(gtk::Orientation::Vertical, 2);
            let path = e.path.clone();
            let win = self.window.clone();
            let toasts = self.toasts.clone();
            let entries: Vec<(&str, Box<dyn Fn()>)> = vec![
                (
                    "Open",
                    Box::new({
                        let p = path.clone();
                        let w = win.clone();
                        move || util::open_uri(Some(w.upcast_ref()), &p)
                    }),
                ),
                (
                    "Show in Folder",
                    Box::new({
                        let p = path.clone();
                        let w = win.clone();
                        move || util::show_in_folder(Some(w.upcast_ref()), &p)
                    }),
                ),
                (
                    "Share…",
                    Box::new({
                        let p = path.clone();
                        let w = win.clone();
                        move || dialogs::share_for_window(&w, &p)
                    }),
                ),
                (
                    "Copy Link",
                    Box::new({
                        let p = path.clone();
                        let t = toasts.clone();
                        move || dialogs::copy_link(&t, &p)
                    }),
                ),
                (
                    "Version History",
                    Box::new({
                        let p = path.clone();
                        let w = win.clone();
                        move || dialogs::versions_for_window(&w, &p)
                    }),
                ),
            ];
            for (label, f) in entries {
                let b = gtk::Button::builder().label(label).css_classes(["flat"]).build();
                if let Some(l) = b.child().and_downcast::<gtk::Label>() {
                    l.set_xalign(0.0);
                }
                let pop2 = pop.clone();
                b.connect_clicked(move |_| {
                    pop2.popdown();
                    f();
                });
                vbox.append(&b);
            }
            pop.set_child(Some(&vbox));
            menu.set_popover(Some(&pop));
            row.add_suffix(&menu);
            let p = e.path.clone();
            let w = self.window.clone();
            row.connect_activated(move |_| util::open_uri(Some(w.upcast_ref()), &p));
        }
        row
    }
}
