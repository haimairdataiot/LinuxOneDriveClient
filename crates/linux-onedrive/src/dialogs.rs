//! Secondary dialogs: sync issues, mass-delete confirmation, sharing,
//! version history, choose folders and About.

use crate::client;
use crate::{App, share, util};
use adw::prelude::*;
use gtk::{gio, glib};
use od_core::dbus::{FolderEntry, Issue, PathInfo, VersionInfo};
use std::rc::Rc;

/// Account object path owning a local path (asks the daemon).
async fn account_for(path: &str) -> zbus::Result<String> {
    let json = client::manager().await?.paths_info(&[path]).await?;
    let infos: Vec<PathInfo> = serde_json::from_str(&json).unwrap_or_default();
    infos
        .into_iter()
        .next()
        .map(|i| i.account)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| zbus::Error::Failure("This item isn't in a OneDrive folder.".into()))
}

fn parent_window(app: &Rc<App>) -> Option<gtk::Window> {
    app.window().filter(|w| w.is_visible())
}

// ------------------------------------------------------------ about

pub fn about(app: &Rc<App>) {
    let d = adw::AboutDialog::builder()
        .application_name(od_core::APP_NAME)
        .application_icon(od_core::APP_ID)
        .version(od_core::VERSION)
        .developer_name("Linux OneDrive contributors")
        .license_type(gtk::License::Gpl30)
        .website("https://github.com/haimairdataiot/LinuxOneDriveClient")
        .comments("An unofficial OneDrive client for Linux with Files On-Demand.\n\nNot affiliated with or endorsed by Microsoft. OneDrive is a trademark of Microsoft Corporation.")
        .build();
    d.present(parent_window(app).as_ref());
}

// ------------------------------------------------------------ sync issues

pub fn issues(app: &Rc<App>, account: &str) {
    let accounts: Vec<String> = if account.is_empty() {
        app.model().accounts.iter().map(|a| a.path.clone()).collect()
    } else {
        vec![account.to_string()]
    };
    let parent = parent_window(app);
    let accts = accounts.clone();
    client::call(
        async move {
            let mut all: Vec<Issue> = Vec::new();
            for p in &accts {
                if let Ok(a) = client::account(p).await
                    && let Ok(j) = a.list_issues().await
                {
                    all.extend(serde_json::from_str::<Vec<Issue>>(&j).unwrap_or_default());
                }
            }
            all
        },
        move |list: Vec<Issue>| {
            let dialog = adw::Dialog::builder().title("Sync Issues").content_width(480).content_height(520).build();
            let tv = adw::ToolbarView::new();
            let header = adw::HeaderBar::new();
            let retry = gtk::Button::builder().label("Retry All").build();
            let accts = accounts.clone();
            let d2 = dialog.clone();
            retry.connect_clicked(move |_| {
                let accts = accts.clone();
                client::call(
                    async move {
                        for p in accts {
                            if let Ok(a) = client::account(&p).await {
                                let _ = a.retry_failed().await;
                            }
                        }
                    },
                    {
                        let d2 = d2.clone();
                        move |_| {
                            d2.close();
                        }
                    },
                );
            });
            header.pack_start(&retry);
            tv.add_top_bar(&header);
            if list.is_empty() {
                retry.set_sensitive(false);
                tv.set_content(Some(
                    &adw::StatusPage::builder()
                        .icon_name("object-select-symbolic")
                        .title("No sync issues")
                        .description("Everything that can sync is in sync.")
                        .build(),
                ));
            } else {
                let lb = gtk::ListBox::builder()
                    .css_classes(["boxed-list"])
                    .selection_mode(gtk::SelectionMode::None)
                    .build();
                for i in &list {
                    let row = adw::ActionRow::builder().title(&i.name).subtitle(&i.message).use_markup(false).build();
                    row.set_subtitle_lines(3);
                    row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
                    let show = gtk::Button::builder()
                        .icon_name("folder-open-symbolic")
                        .tooltip_text("Show in folder")
                        .valign(gtk::Align::Center)
                        .css_classes(["flat"])
                        .build();
                    let p = i.path.clone();
                    show.connect_clicked(move |b| util::show_in_folder(b.root().and_downcast_ref::<gtk::Window>(), &p));
                    row.add_suffix(&show);
                    lb.append(&row);
                }
                let clamp = adw::Clamp::builder()
                    .child(&lb)
                    .margin_top(12)
                    .margin_bottom(12)
                    .margin_start(12)
                    .margin_end(12)
                    .build();
                tv.set_content(Some(&gtk::ScrolledWindow::builder().child(&clamp).vexpand(true).build()));
            }
            dialog.set_child(Some(&tv));
            dialog.present(parent.as_ref());
        },
    );
}

// ------------------------------------------------------------ mass delete

pub fn mass_delete(app: &Rc<App>, account: &str) {
    let path = if account.starts_with('/') {
        account.to_string()
    } else {
        app.model().account(account).map(|a| a.path.clone()).unwrap_or_default()
    };
    match parent_window(app) {
        Some(w) => mass_delete_for_window(&w, &path),
        None => {
            app.show_activity();
            if let Some(w) = app.window() {
                mass_delete_for_window(&w, &path);
            }
        }
    }
}

pub fn mass_delete_for_window(window: &impl IsA<gtk::Widget>, account_path: &str) {
    let path = account_path.to_string();
    let parent = window.as_ref().clone();
    client::call(
        {
            let path = path.clone();
            async move { client::account(&path).await?.held_deletes().await }
        },
        move |r: zbus::Result<u32>| {
            let n = r.unwrap_or(0);
            if n == 0 {
                return;
            }
            let d = adw::AlertDialog::new(
                Some(&format!("Delete {n} files from OneDrive?")),
                Some(&format!(
                    "You deleted {n} files from your OneDrive folder on this device. If you remove them, they'll also be deleted from OneDrive and your other devices; you can still get them back from the OneDrive recycle bin. If you restore them, they'll come back to this device."
                )),
            );
            d.add_responses(&[("restore", "Restore Files"), ("remove", "Remove")]);
            d.set_response_appearance("restore", adw::ResponseAppearance::Suggested);
            d.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
            d.set_default_response(Some("restore"));
            d.set_close_response("restore");
            d.connect_response(None, move |_, r| {
                let delete = r == "remove";
                let p = path.clone();
                client::call(async move { client::account(&p).await?.resolve_mass_delete(delete).await }, |_| {});
            });
            d.present(Some(&parent));
        },
    );
}

// ------------------------------------------------------------ sharing

pub fn share(app: &Rc<App>, path: &str) {
    let w = parent_window(app);
    share::open(w.map(|w| w.upcast()), path, share::Page::Share);
}

pub fn manage_access(app: &Rc<App>, path: &str) {
    let w = parent_window(app);
    share::open(w.map(|w| w.upcast()), path, share::Page::ManageAccess);
}

pub fn share_for_window(window: &impl IsA<gtk::Widget>, path: &str) {
    share::open(Some(window.as_ref().clone()), path, share::Page::Share);
}

/// "Copy link" from a file manager: the default link is copied to the
/// clipboard, which needs a surface of ours; a small confirmation window
/// provides one and says what happened, like the Windows client's toast.
pub fn copy_link_standalone(app: &Rc<App>, path: &str) {
    let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let window =
        adw::Window::builder().application(&app.gtk).title("Copy Link").default_width(380).resizable(false).build();
    let status = adw::StatusPage::builder()
        .icon_name("insert-link-symbolic")
        .title("Creating link\u{2026}")
        .description(&name)
        .css_classes(["compact"])
        .build();
    let close = gtk::Button::builder().label("Close").halign(gtk::Align::Center).css_classes(["pill"]).build();
    let settings = gtk::Button::builder()
        .label("Link Settings\u{2026}")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "flat"])
        .build();
    let bbox = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    bbox.set_halign(gtk::Align::Center);
    bbox.append(&settings);
    bbox.append(&close);
    status.set_child(Some(&bbox));
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::builder().show_title(false).build());
    tv.set_content(Some(&status));
    window.set_content(Some(&tv));
    let w = window.clone();
    close.connect_clicked(move |_| w.close());
    let keep_open = Rc::new(std::cell::Cell::new(false));
    let (w, p, k) = (window.clone(), path.to_string(), keep_open.clone());
    settings.connect_clicked(move |_| {
        k.set(true);
        share::open(Some(w.clone().upcast()), &p, share::Page::LinkSettings);
    });
    window.present();
    let (st, w) = (status.clone(), window.clone());
    share::copy_link(&window, path, move |r| match r {
        Ok(url) => {
            st.set_title("Link copied");
            st.set_description(Some(&url));
            // Close by itself once the clipboard has been handed over.
            let w2 = w.clone();
            glib::timeout_add_local_once(std::time::Duration::from_secs(6), move || {
                if !keep_open.get() {
                    w2.close();
                }
            });
        }
        Err(e) => {
            st.set_icon_name(Some("dialog-error-symbolic"));
            st.set_title("Couldn\u{2019}t create the link");
            st.set_description(Some(&e));
        }
    });
}

pub fn copy_link(toasts: &adw::ToastOverlay, path: &str) {
    let t = toasts.clone();
    share::copy_link(toasts, path, move |r| match r {
        Ok(_) => util::toast(&t, "Link copied"),
        Err(e) => util::toast(&t, &e),
    });
}

// ------------------------------------------------------------ version history

pub fn versions(app: &Rc<App>, path: &str) {
    let w = parent_window(app);
    versions_dialog(w.as_ref().map(|w| w.upcast_ref::<gtk::Widget>()), path);
}

pub fn versions_for_window(window: &impl IsA<gtk::Widget>, path: &str) {
    versions_dialog(Some(window.as_ref()), path);
}

fn versions_dialog(parent: Option<&gtk::Widget>, path: &str) {
    let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let p = path.to_string();
    let parent = parent.cloned();
    client::call(
        {
            let p = p.clone();
            async move {
                let acct = account_for(&p).await?;
                let json = client::account(&acct).await?.list_versions(&p).await?;
                Ok::<_, zbus::Error>((acct, serde_json::from_str::<Vec<VersionInfo>>(&json).unwrap_or_default()))
            }
        },
        move |r| {
            let dialog = adw::Dialog::builder()
                .title(format!("Version History: {name}"))
                .content_width(480)
                .content_height(480)
                .build();
            let tv = adw::ToolbarView::new();
            tv.add_top_bar(&adw::HeaderBar::new());
            let toasts = adw::ToastOverlay::new();
            match r {
                Err(e) => tv.set_content(Some(
                    &adw::StatusPage::builder()
                        .icon_name("dialog-error-symbolic")
                        .title("Can't show versions")
                        .description(util::friendly(&e))
                        .build(),
                )),
                Ok((acct, list)) => {
                    let lb = gtk::ListBox::builder()
                        .css_classes(["boxed-list"])
                        .selection_mode(gtk::SelectionMode::None)
                        .build();
                    for v in list {
                        let when = chrono::DateTime::from_timestamp(v.modified, 0)
                            .map(|t| t.with_timezone(&chrono::Local).format("%-d %B %Y, %H:%M").to_string())
                            .unwrap_or_default();
                        let mut sub = od_core::fmt::size(v.size);
                        if !v.modified_by.is_empty() {
                            sub = format!("{sub} · {}", v.modified_by);
                        }
                        if v.current {
                            sub = format!("Current version · {sub}");
                        }
                        let row = adw::ActionRow::builder().title(&when).subtitle(&sub).build();
                        if !v.current {
                            let restore = gtk::Button::builder().label("Restore").valign(gtk::Align::Center).build();
                            let (acct2, p2, vid, t) = (acct.clone(), p.clone(), v.id.clone(), toasts.clone());
                            restore.connect_clicked(move |_| {
                                let (acct2, p2, vid) = (acct2.clone(), p2.clone(), vid.clone());
                                let t = t.clone();
                                client::call(
                                    async move { client::account(&acct2).await?.restore_version(&p2, &vid).await },
                                    move |r: zbus::Result<()>| match r {
                                        Ok(()) => util::toast(&t, "Version restored"),
                                        Err(e) => util::toast(&t, &util::friendly(&e)),
                                    },
                                );
                            });
                            row.add_suffix(&restore);
                        }
                        let download = gtk::Button::builder()
                            .icon_name("folder-download-symbolic")
                            .tooltip_text("Download a copy")
                            .valign(gtk::Align::Center)
                            .css_classes(["flat"])
                            .build();
                        let (acct2, p2, vid, t, fname) =
                            (acct.clone(), p.clone(), v.id.clone(), toasts.clone(), name.clone());
                        download.connect_clicked(move |b| {
                            let fd = gtk::FileDialog::builder().initial_name(fname.clone()).build();
                            let (acct2, p2, vid, t) = (acct2.clone(), p2.clone(), vid.clone(), t.clone());
                            fd.save(b.root().and_downcast_ref::<gtk::Window>(), gio::Cancellable::NONE, move |r| {
                                let Ok(f) = r else { return };
                                let Some(dest) = f.path() else { return };
                                let dest = dest.to_string_lossy().into_owned();
                                client::call(
                                    async move { client::account(&acct2).await?.download_version(&p2, &vid, &dest).await },
                                    move |r: zbus::Result<()>| match r {
                                        Ok(()) => util::toast(&t, "Version downloaded"),
                                        Err(e) => util::toast(&t, &util::friendly(&e)),
                                    },
                                );
                            });
                        });
                        row.add_suffix(&download);
                        lb.append(&row);
                    }
                    let clamp = adw::Clamp::builder()
                        .child(&lb)
                        .margin_top(12)
                        .margin_bottom(12)
                        .margin_start(12)
                        .margin_end(12)
                        .build();
                    toasts.set_child(Some(&gtk::ScrolledWindow::builder().child(&clamp).vexpand(true).build()));
                    tv.set_content(Some(&toasts));
                }
            }
            dialog.set_child(Some(&tv));
            dialog.present(parent.as_ref());
        },
    );
}

// ------------------------------------------------------------ choose folders

pub fn choose_folders(app: &Rc<App>, account: &str) {
    let path = app.model().account(account).map(|a| a.path.clone()).unwrap_or_else(|| account.to_string());
    let parent = parent_window(app);
    let p = path.clone();
    client::call(async move { client::account(&p).await?.list_folders("/").await }, move |r: zbus::Result<String>| {
        let folders: Vec<FolderEntry> = r.ok().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let dialog = adw::PreferencesDialog::builder().title("Choose Folders").build();
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::builder()
                .title("Folders on this device")
                .description("Hidden folders stay in OneDrive online but don't appear in your OneDrive folder here. Their downloaded files are removed from this device.")
                .build();
        let toasts_target = dialog.clone();
        for f in folders {
            let row = adw::SwitchRow::builder().title(&f.name).active(!f.excluded).build();
            row.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));
            let (acct, fpath, dlg) = (path.clone(), f.path.clone(), toasts_target.clone());
            row.connect_active_notify(move |r| {
                let hide = !r.is_active();
                let (acct, fpath, dlg, r2) = (acct.clone(), fpath.clone(), dlg.clone(), r.clone());
                client::call(
                    async move { client::account(&acct).await?.set_folder_excluded(&fpath, hide).await },
                    move |res: zbus::Result<()>| {
                        if let Err(e) = res {
                            dlg.add_toast(adw::Toast::new(&util::friendly(&e)));
                            r2.set_active(hide);
                        }
                    },
                );
            });
            group.add(&row);
        }
        page.add(&group);
        dialog.add(&page);
        dialog.present(parent.as_ref());
    });
}

// ------------------------------------------------------------ first-delete reminder

/// "Deleted files are removed everywhere", like the official client shows
/// the first time you delete something from the OneDrive folder.
pub fn delete_reminder(app: &Rc<App>, _account: &str) {
    let d = adw::AlertDialog::new(
        Some("Deleted files are removed everywhere"),
        Some(
            "When you delete files from your OneDrive folder, they're also deleted from OneDrive online and from your other devices. You can restore them from the OneDrive recycle bin for a while.",
        ),
    );
    let dont = gtk::CheckButton::with_label("Don't show this reminder again");
    dont.set_halign(gtk::Align::Center);
    d.set_extra_child(Some(&dont));
    d.add_responses(&[("bin", "Open Recycle Bin"), ("ok", "OK")]);
    d.set_default_response(Some("ok"));
    d.set_close_response("ok");
    let app2 = app.clone();
    d.connect_response(None, move |_, r| {
        if dont.is_active() {
            let patch = r#"{"notifications": {"delete_reminder": false}}"#;
            client::call(async move { client::manager().await?.set_settings(patch).await }, |_| {});
        }
        if r == "bin" {
            let _ = app2.gtk.activate_action("recycle-bin", None);
        }
    });
    if parent_window(app).is_none() {
        app.show_activity();
    }
    d.present(app.window().as_ref());
}

// ------------------------------------------------------------ recently removed

/// Files deleted online whose downloaded copies this device still keeps.
pub fn recently_removed(app: &Rc<App>, account: &str) {
    let path = app
        .model()
        .account(account)
        .map(|a| a.path.clone())
        .or_else(|| app.current_account().map(|a| a.path))
        .unwrap_or_default();
    let parent = parent_window(app);
    let p = path.clone();
    client::call(async move { client::account(&p).await?.list_removed().await }, move |r: zbus::Result<String>| {
        let items: Vec<od_core::dbus::RemovedItem> =
            r.ok().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let dialog =
            adw::Dialog::builder().title("Recently Removed Files").content_width(480).content_height(520).build();
        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&adw::HeaderBar::new());
        let toasts = adw::ToastOverlay::new();
        if items.is_empty() {
            toasts.set_child(Some(
                    &adw::StatusPage::builder()
                        .icon_name("user-trash-symbolic")
                        .title("Nothing to restore")
                        .description("When files are deleted from OneDrive online, the copies downloaded to this device are kept here for 7 days.")
                        .build(),
                ));
        } else {
            let lb =
                gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
            for it in items {
                let when = util::relative_time(it.removed_at);
                let row = adw::ActionRow::builder()
                    .title(&it.name)
                    .subtitle(format!("{} · {} · removed {when}", it.rel_path, od_core::fmt::size(it.size)))
                    .use_markup(false)
                    .build();
                row.add_prefix(&gtk::Image::from_gicon(&util::file_icon(&it.name, false)));
                let restore = gtk::Button::builder().label("Restore").valign(gtk::Align::Center).build();
                let (acct, id, t, row2) = (path.clone(), it.id.clone(), toasts.clone(), row.clone());
                restore.connect_clicked(move |b| {
                    b.set_sensitive(false);
                    let (acct, id, t, row2) = (acct.clone(), id.clone(), t.clone(), row2.clone());
                    client::call(
                        async move { client::account(&acct).await?.restore_removed(&id).await },
                        move |r: zbus::Result<String>| match r {
                            Ok(p) => {
                                row2.set_sensitive(false);
                                util::toast(&t, &format!("Restored to {p}"));
                            }
                            Err(e) => util::toast(&t, &util::friendly(&e)),
                        },
                    );
                });
                row.add_suffix(&restore);
                lb.append(&row);
            }
            let note = gtk::Label::builder()
                    .label("These files were deleted from OneDrive online. Restoring puts them back in your OneDrive folder and uploads them again.")
                    .wrap(true)
                    .css_classes(["dim-label", "caption"])
                    .margin_bottom(12)
                    .xalign(0.0)
                    .build();
            let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
            vbox.append(&note);
            vbox.append(&lb);
            let clamp = adw::Clamp::builder()
                .child(&vbox)
                .margin_top(12)
                .margin_bottom(12)
                .margin_start(12)
                .margin_end(12)
                .build();
            toasts.set_child(Some(&gtk::ScrolledWindow::builder().child(&clamp).vexpand(true).build()));
        }
        tv.set_content(Some(&toasts));
        dialog.set_child(Some(&tv));
        dialog.present(parent.as_ref());
    });
}

// ------------------------------------------------------------ folder backup

/// "Manage back up": choose which personal folders live in OneDrive.
pub fn manage_backup(app: &Rc<App>, account: &str) {
    let path = account.to_string();
    let parent = parent_window(app);
    let p = path.clone();
    client::call(
        async move { client::account(&p).await?.list_backup_folders().await },
        move |r: zbus::Result<String>| {
            let folders: Vec<od_core::dbus::BackupFolder> =
                r.ok().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
            let dialog = adw::PreferencesDialog::builder().title("Manage Back Up").build();
            let page = adw::PreferencesPage::new();
            let group = adw::PreferencesGroup::builder()
                .title("Folders on this device")
                .description("Backed-up folders move into your OneDrive folder; the old location keeps working. Turning a backup off copies the files back here, and they stay in OneDrive too.")
                .build();
            for f in folders {
                let icon = match f.key.as_str() {
                    "DESKTOP" => "user-desktop-symbolic",
                    "DOCUMENTS" => "folder-documents-symbolic",
                    "PICTURES" => "folder-pictures-symbolic",
                    "MUSIC" => "folder-music-symbolic",
                    _ => "folder-videos-symbolic",
                };
                let sub = if f.in_progress {
                    "Updating…".to_string()
                } else if f.backed_up {
                    format!("Backed up · {} files · {}", f.files, od_core::fmt::size(f.bytes))
                } else {
                    format!("{} files · {} · {}", f.files, od_core::fmt::size(f.bytes), f.local_path)
                };
                let row = adw::SwitchRow::builder()
                    .title(&f.name)
                    .subtitle(&sub)
                    .active(f.backed_up)
                    .sensitive(!f.in_progress)
                    .build();
                row.add_prefix(&gtk::Image::from_icon_name(icon));
                let (acct, key, name, dlg) = (path.clone(), f.key.clone(), f.name.clone(), dialog.clone());
                row.connect_active_notify(move |r| {
                    let on = r.is_active();
                    let (acct, key, r2, dlg2) = (acct.clone(), key.clone(), r.clone(), dlg.clone());
                    let go = move || {
                        r2.set_sensitive(false);
                        r2.set_subtitle("Updating… you'll get a notification when it's done");
                        let (acct, key, r3, dlg3) = (acct.clone(), key.clone(), r2.clone(), dlg2.clone());
                        client::call(
                            async move { client::account(&acct).await?.set_folder_backup(&key, on).await },
                            move |res: zbus::Result<()>| {
                                if let Err(e) = res {
                                    dlg3.add_toast(adw::Toast::new(&util::friendly(&e)));
                                    r3.set_sensitive(true);
                                    r3.set_active(!on);
                                }
                            },
                        );
                    };
                    if on {
                        let alert = adw::AlertDialog::new(
                            Some(&format!("Back up {name}?")),
                            Some(&format!("The files in your {name} folder will move to OneDrive and upload. Your {name} folder keeps working from the same place, and the files are kept on this device.")),
                        );
                        alert.add_responses(&[("cancel", "Cancel"), ("go", "Start Backup")]);
                        alert.set_response_appearance("go", adw::ResponseAppearance::Suggested);
                        let r4 = r.clone();
                        alert.connect_response(None, move |_, resp| {
                            if resp == "go" {
                                go();
                            } else {
                                r4.set_active(false);
                            }
                        });
                        alert.present(Some(r));
                    } else {
                        go();
                    }
                });
                group.add(&row);
            }
            page.add(&group);
            dialog.add(&page);
            dialog.present(parent.as_ref());
        },
    );
}
