//! Settings window, organised like the official client's settings:
//! Sync and back up · Account · Notifications · Network · Advanced.

use crate::client::{self, AccountView};
use crate::{App, dialogs, util};
use adw::prelude::*;
use gtk::gio;
use od_core::dbus::SettingsDoc;
use serde_json::json;
use std::rc::Rc;

/// Send a partial settings document to the daemon.
fn set(patch: serde_json::Value) {
    let s = patch.to_string();
    client::call(async move { client::manager().await?.set_settings(&s).await }, |r: zbus::Result<()>| {
        if let Err(e) = r {
            tracing::warn!("saving settings failed: {e}");
        }
    });
}

fn switch(title: &str, subtitle: &str, value: bool, on_change: impl Fn(bool) + 'static) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder().title(title).active(value).build();
    if !subtitle.is_empty() {
        row.set_subtitle(subtitle);
    }
    row.connect_active_notify(move |r| on_change(r.is_active()));
    row
}

fn spin(title: &str, min: f64, max: f64, step: f64, value: f64, on_change: impl Fn(f64) + 'static) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(min, max, step);
    row.set_title(title);
    row.set_value(value);
    row.connect_value_notify(move |r| on_change(r.value()));
    row
}

pub fn show(app: &Rc<App>, parent: Option<&gtk::Window>) {
    let app = app.clone();
    let parent = parent.cloned();
    client::call(async move { client::manager().await?.get_settings().await }, move |r: zbus::Result<String>| {
        let doc: SettingsDoc = r.ok().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        build(&app, doc).present(parent.as_ref());
    });
}

fn build(app: &Rc<App>, doc: SettingsDoc) -> adw::PreferencesDialog {
    // A fixed height (pages scroll): by default the dialog is as tall as its
    // longest page, which does not fit on a small laptop screen when the
    // dialog opens as a window of its own.
    let dialog = adw::PreferencesDialog::builder().title("Settings").search_enabled(true).content_height(640).build();
    let model = app.model();
    let acct = app.current_account();

    // ---------------------------------------------------------------- Sync and back up
    let sync =
        adw::PreferencesPage::builder().title("Sync and back up").icon_name("emblem-synchronizing-symbolic").build();
    if let Some(a) = &acct {
        let g = adw::PreferencesGroup::builder()
            .title("Files On-Demand")
            .description("Save space on this device by downloading files only when you use them.")
            .build();
        let free = gtk::CheckButton::new();
        let all = gtk::CheckButton::new();
        all.set_group(Some(&free));
        if a.download_all {
            all.set_active(true)
        } else {
            free.set_active(true)
        }
        let r1 = adw::ActionRow::builder()
            .title("Free up disk space")
            .subtitle("Files appear right away and download when you open them (recommended)")
            .activatable_widget(&free)
            .build();
        r1.add_prefix(&free);
        let r2 = adw::ActionRow::builder()
            .title("Download all files")
            .subtitle("Every file is downloaded now and kept on this device")
            .activatable_widget(&all)
            .build();
        r2.add_prefix(&all);
        let path = a.path.clone();
        all.connect_toggled(move |b| {
            let enabled = b.is_active();
            let p = path.clone();
            client::call(async move { client::account(&p).await?.set_download_all(enabled).await }, |_| {});
        });
        g.add(&r1);
        g.add(&r2);
        sync.add(&g);
        // "Manage back up": Desktop, Documents, Pictures, Music, Videos.
        let backup = adw::PreferencesGroup::builder()
            .title("Back up important folders")
            .description("Move these folders into OneDrive so they're protected and available on your other devices. They keep working from the same place.")
            .build();
        let manage = util::button_row("Manage Back Up", None, Some("go-next-symbolic"));
        let (app2, p2) = (app.clone(), a.path.clone());
        manage.connect_activated(move |_| dialogs::manage_backup(&app2, &p2));
        backup.add(&manage);
        sync.add(&backup);
    }
    let g = adw::PreferencesGroup::builder().title("Preferences").build();
    g.add(&switch("Start Linux OneDrive when I sign in", "", doc.general.start_at_login, |v| {
        set(json!({"general": {"start_at_login": v}}))
    }));
    g.add(&switch(
        "Pause syncing on metered networks",
        "Saves mobile data and limited connections",
        doc.general.pause_on_metered,
        |v| set(json!({"general": {"pause_on_metered": v}})),
    ));
    g.add(&switch(
        "Pause syncing in power-saver mode",
        "Helps your battery last longer",
        doc.general.pause_on_power_saver,
        |v| set(json!({"general": {"pause_on_power_saver": v}})),
    ));
    let days = [0u32, 1, 14, 30, 60];
    let combo = adw::ComboRow::builder()
        .title("Free up space for unused files")
        .subtitle("Files you haven't opened become online-only again")
        .model(&gtk::StringList::new(&["Never", "After 1 day", "After 14 days", "After 30 days", "After 60 days"]))
        .selected(days.iter().position(|d| *d == doc.general.auto_free_up_days).unwrap_or(0) as u32)
        .build();
    combo.connect_selected_notify(move |c| {
        let d = days.get(c.selected() as usize).copied().unwrap_or(0);
        set(json!({"general": {"auto_free_up_days": d}}));
    });
    g.add(&combo);
    sync.add(&g);
    dialog.add(&sync);

    // ---------------------------------------------------------------- Account
    let account_page = adw::PreferencesPage::builder().title("Account").icon_name("avatar-default-symbolic").build();
    for a in &model.accounts {
        account_page.add(&account_group(app, &dialog, a));
    }
    let add = adw::PreferencesGroup::new();
    let add_row = util::button_row("Add an Account", Some("list-add-symbolic"), None);
    let app2 = app.clone();
    let d2 = dialog.clone();
    add_row.connect_activated(move |_| {
        d2.close();
        app2.show_setup(None);
    });
    add.add(&add_row);
    account_page.add(&add);
    dialog.add(&account_page);

    // ---------------------------------------------------------------- Notifications
    let n = &doc.notifications;
    let notif = adw::PreferencesPage::builder()
        .title("Notifications")
        .icon_name("preferences-system-notifications-symbolic")
        .build();
    let g = adw::PreferencesGroup::builder().title("Show notifications").build();
    let master = switch("Notifications", "Show notifications from Linux OneDrive", n.enabled, |v| {
        set(json!({"notifications": {"enabled": v}}))
    });
    g.add(&master);
    let detail = adw::PreferencesGroup::builder().title("Notify me when").build();
    for (key, title, value) in [
        ("mass_delete", "Many files are deleted", n.mass_delete),
        ("sync_paused", "Syncing pauses automatically", n.sync_paused),
        ("conflicts", "A file was changed in two places", n.conflicts),
        ("storage_full", "My OneDrive or this device is almost full", n.storage_full),
        ("sync_errors", "Files can't be synced", n.sync_errors),
        ("offline_open", "An online-only file can't open while offline", n.offline_open),
        ("shared_with_me", "Files are shared with me", n.shared_with_me),
    ] {
        let key = key.to_string();
        let row = switch(title, "", value, move |v| set(json!({"notifications": {key.as_str(): v}})));
        master.bind_property("active", &row, "sensitive").sync_create().build();
        detail.add(&row);
    }
    notif.add(&g);
    notif.add(&detail);
    dialog.add(&notif);

    // ---------------------------------------------------------------- Network
    let net = &doc.network;
    let network =
        adw::PreferencesPage::builder().title("Network").icon_name("network-transmit-receive-symbolic").build();
    for (dir, key, value) in [
        ("Upload", "upload_limit_kbps", net.upload_limit_kbps),
        ("Download", "download_limit_kbps", net.download_limit_kbps),
    ] {
        let g = adw::PreferencesGroup::builder().title(format!("{dir} rate")).build();
        let limit = adw::SwitchRow::builder().title("Limit to").active(value > 0).build();
        let rate = adw::SpinRow::with_range(50.0, 100_000.0, 50.0);
        rate.set_title("KB/s");
        rate.set_value(if value > 0 { value as f64 } else { 1000.0 });
        limit.bind_property("active", &rate, "sensitive").sync_create().build();
        let (k1, r1) = (key.to_string(), rate.clone());
        limit.connect_active_notify(move |l| {
            let v = if l.is_active() { r1.value() as u32 } else { 0 };
            set(json!({"network": {k1.as_str(): v}}));
        });
        let (k2, l2) = (key.to_string(), limit.clone());
        rate.connect_value_notify(move |r| {
            if l2.is_active() {
                set(json!({"network": {k2.as_str(): r.value() as u32}}));
            }
        });
        g.add(&limit);
        g.add(&rate);
        network.add(&g);
    }
    let g = adw::PreferencesGroup::builder().title("Transfers").build();
    g.add(&spin("Simultaneous uploads", 1.0, 16.0, 1.0, net.max_parallel_uploads as f64, |v| {
        set(json!({"network": {"max_parallel_uploads": v as u32}}))
    }));
    g.add(&spin("Simultaneous downloads", 1.0, 16.0, 1.0, net.max_parallel_downloads as f64, |v| {
        set(json!({"network": {"max_parallel_downloads": v as u32}}))
    }));
    network.add(&g);
    dialog.add(&network);

    // ---------------------------------------------------------------- Advanced
    let general = &doc.general;
    let adv = adw::PreferencesPage::builder().title("Advanced").icon_name("applications-engineering-symbolic").build();
    adv.add(&crate::update::settings_group(&app, general.check_updates, set));
    let g = adw::PreferencesGroup::builder().title("Safety").build();
    g.add(&spin(
        "Ask before deleting more than this many files online",
        0.0,
        100_000.0,
        10.0,
        general.mass_delete_threshold as f64,
        |v| set(json!({"general": {"mass_delete_threshold": v as u32}})),
    ));
    g.add(&switch(
        "Keep thumbnailers and search from downloading files",
        "Previews of online-only files come from OneDrive instead",
        general.block_background_hydration,
        |v| set(json!({"general": {"block_background_hydration": v}})),
    ));
    g.add(&switch(
        "Show previews of online-only files",
        "Uses OneDrive's thumbnails, so photos and videos don't download",
        general.server_thumbnails,
        |v| set(json!({"general": {"server_thumbnails": v}})),
    ));
    adv.add(&g);
    let g = adw::PreferencesGroup::builder().title("Sharing").build();
    let kinds = ["auto", "anyone-edit", "anyone-view", "organization-edit", "organization-view"];
    let default_link = adw::ComboRow::builder()
        .title("Copy Link creates")
        .subtitle(
            "Used by Copy Link in the file manager; work or school accounts only can limit links to your organization",
        )
        .model(&gtk::StringList::new(&[
            "OneDrive\u{2019}s default",
            "Anyone with the link can edit",
            "Anyone with the link can view",
            "People in your organization can edit",
            "People in your organization can view",
        ]))
        .selected(kinds.iter().position(|k| *k == general.default_link).unwrap_or(0) as u32)
        .build();
    default_link.connect_selected_notify(move |c| {
        let k = kinds.get(c.selected() as usize).copied().unwrap_or("auto");
        set(json!({"general": {"default_link": k}}));
    });
    g.add(&default_link);
    adv.add(&g);
    let g = adw::PreferencesGroup::builder()
        .title("Excluded files")
        .description("Files matching these patterns stay on this device and are never uploaded. Separate patterns with commas, e.g. *.bak, build-*")
        .build();
    let patterns = adw::EntryRow::builder()
        .title("Patterns")
        .text(general.excluded_patterns.join(", "))
        .show_apply_button(true)
        .build();
    patterns.connect_apply(|e| {
        let list: Vec<String> = e.text().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        set(json!({"general": {"excluded_patterns": list}}));
    });
    g.add(&patterns);
    adv.add(&g);
    let g = adw::PreferencesGroup::builder().title("Sync").build();
    g.add(&spin("Check for changes every (seconds)", 15.0, 3600.0, 15.0, general.poll_interval_secs as f64, |v| {
        set(json!({"general": {"poll_interval_secs": v as u64}}))
    }));
    let levels = ["error", "warn", "info", "debug"];
    let log = adw::ComboRow::builder()
        .title("Log detail")
        .subtitle("Logs are in the system journal: journalctl --user -u linux-onedrived")
        .model(&gtk::StringList::new(&["Errors only", "Warnings", "Normal", "Debug"]))
        .selected(levels.iter().position(|l| *l == general.log_level).unwrap_or(2) as u32)
        .build();
    log.connect_selected_notify(move |c| {
        let l = levels.get(c.selected() as usize).copied().unwrap_or("info");
        set(json!({"general": {"log_level": l}}));
    });
    g.add(&log);
    adv.add(&g);
    dialog.add(&adv);
    dialog
}

fn account_group(app: &Rc<App>, dialog: &adw::PreferencesDialog, a: &AccountView) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder()
        .title(&a.email)
        .description(if a.drive_type == "personal" {
            "Personal account".to_string()
        } else if a.organization.is_empty() {
            "Work or school account".to_string()
        } else {
            a.organization.clone()
        })
        .build();
    let storage = adw::ActionRow::builder()
        .title("Storage")
        .subtitle(if a.quota_total > 0 {
            format!("{} of {} used", od_core::fmt::size(a.quota_used), od_core::fmt::size(a.quota_total))
        } else {
            "Checking…".into()
        })
        .build();
    let manage = gtk::Button::builder().label("Manage").valign(gtk::Align::Center).build();
    manage.connect_clicked(|b| {
        util::open_uri(b.root().and_downcast_ref::<gtk::Window>(), "https://account.microsoft.com/storage")
    });
    storage.add_suffix(&manage);
    g.add(&storage);

    let folder =
        adw::ActionRow::builder().title("OneDrive folder").subtitle(&a.mount_point).subtitle_selectable(true).build();
    let change = gtk::Button::builder().label("Change…").valign(gtk::Align::Center).build();
    let path = a.path.clone();
    let row = folder.clone();
    change.connect_clicked(move |b| {
        let fd = gtk::FileDialog::builder().title("Move your OneDrive folder").build();
        let path = path.clone();
        let row = row.clone();
        let parent = b.root().and_downcast::<gtk::Window>();
        fd.select_folder(parent.as_ref(), gio::Cancellable::NONE, move |r| {
            let Ok(f) = r else { return };
            let Some(dir) = f.path() else { return };
            let current = row.subtitle().map(|s| s.to_string()).unwrap_or_default();
            let base =
                std::path::Path::new(&current).file_name().map(|n| n.to_owned()).unwrap_or_else(|| "OneDrive".into());
            let target = dir.join(base).to_string_lossy().into_owned();
            let row = row.clone();
            let t2 = target.clone();
            client::call(
                async move { client::account(&path).await?.set_mount_point(&t2).await },
                move |r: zbus::Result<()>| match r {
                    Ok(()) => row.set_subtitle(&target),
                    Err(e) => util::error_dialog(&row, "Couldn't move the folder", &util::friendly(&e)),
                },
            );
        });
    });
    folder.add_suffix(&change);
    g.add(&folder);

    let choose = adw::ActionRow::builder()
        .title("Choose folders")
        .subtitle("Pick which top-level folders appear on this device")
        .activatable(true)
        .build();
    choose.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    let (app2, p) = (app.clone(), a.path.clone());
    choose.connect_activated(move |_| dialogs::choose_folders(&app2, &p));
    g.add(&choose);
    g.add(&crate::locations::locations_row(a));

    let unlink = util::button_row("Unlink This Device", None, None);
    unlink.add_css_class("error");
    let (p, email, d) = (a.path.clone(), a.email.clone(), dialog.clone());
    unlink.connect_activated(move |row| {
        let alert = adw::AlertDialog::new(
            Some("Unlink this device?"),
            Some(&format!(
                "Files stop syncing for {email} and the OneDrive folder is removed from this device. Your files stay in OneDrive online. Changes that haven't synced yet are kept in Linux OneDrive's data folder."
            )),
        );
        alert.add_responses(&[("cancel", "Cancel"), ("unlink", "Unlink")]);
        alert.set_response_appearance("unlink", adw::ResponseAppearance::Destructive);
        alert.set_default_response(Some("cancel"));
        let (p, d) = (p.clone(), d.clone());
        alert.connect_response(None, move |_, r| {
            if r == "unlink" {
                let p = p.clone();
                let d = d.clone();
                client::call(async move { client::account(&p).await?.sign_out().await }, move |_: zbus::Result<()>| {
                    d.close();
                });
            }
        });
        alert.present(Some(row));
    });
    g.add(&unlink);
    g
}
