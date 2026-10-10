//! Shortcuts to shared folders and SharePoint libraries synced with an
//! account: the list in Settings ▸ Account, the library picker ("Sync a
//! SharePoint library"), and the browser's Sync button (`odopen://`).

use crate::client::{self, AccountView};
use crate::{App, util};
use adw::prelude::*;
use gtk::gio;
use od_core::dbus::{LibraryInfo, SiteInfo, SyncLocation};
use std::cell::RefCell;
use std::rc::Rc;

/// Refreshes the list after a change.
type Reload = Rc<dyn Fn()>;

/// "Shared folders and libraries" for an account's settings group.
pub fn locations_row(a: &AccountView) -> adw::ExpanderRow {
    let business = a.drive_type != "personal";
    let row = adw::ExpanderRow::builder()
        .title("Shared folders and libraries")
        .subtitle(if business {
            "Shortcuts added with \u{201c}Add shortcut to My files\u{201d}, and SharePoint libraries"
        } else {
            "Shortcuts added with \u{201c}Add shortcut to My files\u{201d}"
        })
        .build();
    let added: Rc<RefCell<Vec<gtk::Widget>>> = Rc::new(RefCell::new(Vec::new()));
    let reload: Rc<RefCell<Option<Reload>>> = Rc::new(RefCell::new(None));
    let (r, acct, added2, reload2) = (row.clone(), a.path.clone(), added.clone(), reload.clone());
    let load: Rc<dyn Fn()> = Rc::new(move || {
        let (r, added, acct, reload) = (r.clone(), added2.clone(), acct.clone(), reload2.clone());
        let a2 = acct.clone();
        client::call(
            async move { client::account(&a2).await?.list_locations().await },
            move |res: zbus::Result<String>| {
                for w in added.borrow_mut().drain(..) {
                    r.remove(&w);
                }
                let list: Vec<SyncLocation> = res.ok().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
                let mut rows: Vec<gtk::Widget> = Vec::new();
                if list.is_empty() {
                    rows.push(adw::ActionRow::builder().title("None yet").css_classes(["dim-label"]).build().upcast());
                }
                for l in list {
                    rows.push(location_row(&acct, &l, reload.borrow().clone()).upcast());
                }
                if business {
                    let add = util::button_row("Sync a SharePoint Library\u{2026}", Some("list-add-symbolic"), None);
                    let (acct, reload) = (acct.clone(), reload.clone());
                    add.connect_activated(move |b| {
                        let reload = reload.borrow().clone();
                        sync_library_dialog(b, &acct, move || {
                            if let Some(f) = &reload {
                                f()
                            }
                        });
                    });
                    rows.push(add.upcast());
                }
                for w in &rows {
                    r.add_row(w);
                }
                *added.borrow_mut() = rows;
            },
        );
    });
    *reload.borrow_mut() = Some(load.clone());
    load();
    row
}

fn location_row(account: &str, l: &SyncLocation, reload: Option<Rc<dyn Fn()>>) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&l.title)
        .subtitle(format!("{} \u{00b7} {}", l.local_path, l.status_text))
        .use_markup(false)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name(if l.kind == "library" {
        "folder-documents-symbolic"
    } else {
        "folder-publicshare-symbolic"
    }));
    let open = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .tooltip_text("Open folder")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    let p = l.local_path.clone();
    open.connect_clicked(move |b| util::open_uri(b.root().and_downcast_ref::<gtk::Window>(), &p));
    row.add_suffix(&open);
    let (heading, body, button) = if l.kind == "library" {
        (
            format!("Stop syncing \u{201c}{}\u{201d}?", l.title),
            "The library's folder is removed from this device. Its files stay online in SharePoint.".to_string(),
            "Stop Sync",
        )
    } else {
        (
            format!("Remove the shortcut \u{201c}{}\u{201d}?", l.title),
            "The shortcut is removed from your OneDrive on all your devices. The shared folder and its files aren't \
             affected, and you can add it again from OneDrive on the web."
                .to_string(),
            "Remove Shortcut",
        )
    };
    let remove = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text(button)
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    let (acct, key) = (account.to_string(), l.key.clone());
    remove.connect_clicked(move |b| {
        let alert = adw::AlertDialog::new(Some(&heading), Some(&body));
        alert.add_responses(&[("cancel", "Cancel"), ("remove", button)]);
        alert.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        alert.set_default_response(Some("cancel"));
        let (acct, key, reload, b2) = (acct.clone(), key.clone(), reload.clone(), b.clone());
        alert.connect_response(None, move |_, r| {
            if r != "remove" {
                return;
            }
            let (acct, key, reload, b3) = (acct.clone(), key.clone(), reload.clone(), b2.clone());
            client::call(
                async move { client::account(&acct).await?.remove_location(&key).await },
                move |r: zbus::Result<()>| match r {
                    Ok(()) => {
                        if let Some(f) = &reload {
                            f()
                        }
                    }
                    Err(e) => util::error_dialog(&b3, "Couldn\u{2019}t do that", &util::friendly(&e)),
                },
            );
        });
        alert.present(Some(b));
    });
    row.add_suffix(&remove);
    row
}

/// Pick a site, then a library, and start syncing it.
pub fn sync_library_dialog(parent: &impl IsA<gtk::Widget>, account: &str, on_added: impl Fn() + 'static) {
    let on_added = Rc::new(on_added);
    let dialog =
        adw::Dialog::builder().title("Sync a SharePoint Library").content_width(460).content_height(560).build();
    let nav = adw::NavigationView::new();
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&nav));
    dialog.set_child(Some(&toasts));

    // Page 1: sites.
    let search = gtk::SearchEntry::builder().placeholder_text("Search for a site").hexpand(true).build();
    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    let status = gtk::Label::builder().css_classes(["dim-label"]).margin_top(12).wrap(true).build();
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
    vbox.append(&search);
    vbox.append(&list);
    vbox.append(&status);
    let page = nav_page("Sync a SharePoint Library", "sites", &vbox);
    nav.add(&page);

    let acct = account.to_string();
    let run: Rc<dyn Fn(String)> = {
        let (list, status, nav, acct, toasts, dialog, on_added) =
            (list.clone(), status.clone(), nav.clone(), acct.clone(), toasts.clone(), dialog.clone(), on_added.clone());
        Rc::new(move |q: String| {
            status.set_text("Searching\u{2026}");
            let (list, status, nav, acct, toasts, dialog, on_added) = (
                list.clone(),
                status.clone(),
                nav.clone(),
                acct.clone(),
                toasts.clone(),
                dialog.clone(),
                on_added.clone(),
            );
            let a2 = acct.clone();
            client::call(
                async move { client::account(&a2).await?.search_sites(&q).await },
                move |r: zbus::Result<String>| {
                    while let Some(c) = list.first_child() {
                        list.remove(&c);
                    }
                    let sites: Vec<SiteInfo> = match r {
                        Ok(j) => serde_json::from_str(&j).unwrap_or_default(),
                        Err(e) => {
                            status.set_text(&util::friendly(&e));
                            return;
                        }
                    };
                    status.set_text(if sites.is_empty() { "No sites found." } else { "" });
                    list.set_visible(!sites.is_empty());
                    for s in sites {
                        let row = adw::ActionRow::builder()
                            .title(&s.name)
                            .subtitle(&s.web_url)
                            .use_markup(false)
                            .activatable(true)
                            .build();
                        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                        let (nav, acct, toasts, dialog, on_added) =
                            (nav.clone(), acct.clone(), toasts.clone(), dialog.clone(), on_added.clone());
                        row.connect_activated(move |_| {
                            libraries_page(&nav, &acct, &s, &toasts, &dialog, on_added.clone());
                        });
                        list.append(&row);
                    }
                },
            );
        })
    };
    let r2 = run.clone();
    search.connect_search_changed(move |e| r2(e.text().to_string()));
    run(String::new());
    dialog.present(Some(parent));
}

fn nav_page(title: &str, tag: &str, child: &impl IsA<gtk::Widget>) -> adw::NavigationPage {
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::new());
    let clamp = adw::Clamp::builder()
        .maximum_size(520)
        .margin_top(12)
        .margin_bottom(18)
        .margin_start(12)
        .margin_end(12)
        .child(child)
        .build();
    let scroller =
        gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&clamp).build();
    tv.set_content(Some(&scroller));
    adw::NavigationPage::builder().title(title).tag(tag).child(&tv).build()
}

fn libraries_page(
    nav: &adw::NavigationView,
    account: &str,
    site: &SiteInfo,
    toasts: &adw::ToastOverlay,
    dialog: &adw::Dialog,
    on_added: Rc<dyn Fn()>,
) {
    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    let status = gtk::Label::builder().label("Loading\u{2026}").css_classes(["dim-label"]).wrap(true).build();
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
    vbox.append(&status);
    vbox.append(&list);
    nav.push(&nav_page(&site.name, "libraries", &vbox));
    let (acct, site_id) = (account.to_string(), site.id.clone());
    let (a2, s2) = (acct.clone(), site_id.clone());
    let (toasts, dialog) = (toasts.clone(), dialog.clone());
    client::call(
        async move { client::account(&a2).await?.site_libraries(&s2).await },
        move |r: zbus::Result<String>| {
            let libs: Vec<LibraryInfo> = match r {
                Ok(j) => serde_json::from_str(&j).unwrap_or_default(),
                Err(e) => {
                    status.set_text(&util::friendly(&e));
                    return;
                }
            };
            status.set_text(if libs.is_empty() { "This site has no document libraries you can sync." } else { "" });
            status.set_visible(libs.is_empty());
            for l in libs {
                let row = adw::ActionRow::builder().title(&l.name).use_markup(false).build();
                row.add_prefix(&gtk::Image::from_icon_name("folder-documents-symbolic"));
                if l.synced {
                    row.add_suffix(&gtk::Label::builder().label("Syncing").css_classes(["dim-label"]).build());
                } else {
                    let sync = gtk::Button::builder()
                        .label("Sync")
                        .valign(gtk::Align::Center)
                        .css_classes(["suggested-action"])
                        .build();
                    let (acct, site_id, drive, toasts, dialog, on_added) = (
                        acct.clone(),
                        site_id.clone(),
                        l.drive_id.clone(),
                        toasts.clone(),
                        dialog.clone(),
                        on_added.clone(),
                    );
                    sync.connect_clicked(move |b| {
                        b.set_sensitive(false);
                        let (acct, site_id, drive) = (acct.clone(), site_id.clone(), drive.clone());
                        let (b2, toasts, dialog, on_added) =
                            (b.clone(), toasts.clone(), dialog.clone(), on_added.clone());
                        client::call(
                            async move { client::account(&acct).await?.add_library(&site_id, &drive).await },
                            move |r: zbus::Result<String>| match r {
                                Ok(path) => {
                                    on_added();
                                    dialog.close();
                                    util::open_uri(b2.root().and_downcast_ref::<gtk::Window>(), &path);
                                }
                                Err(e) => {
                                    b2.set_sensitive(true);
                                    toasts.add_toast(adw::Toast::new(&util::friendly(&e)));
                                }
                            },
                        );
                    });
                    row.add_suffix(&sync);
                }
                list.append(&row);
            }
        },
    );
}

/// The "Sync" button of a SharePoint library in the browser.
pub fn open_sync_link(app: &Rc<App>, url: &str) {
    let url = url.to_string();
    let app = app.clone();
    client::call(async move { client::manager().await?.open_sync_link(&url).await }, move |r: zbus::Result<String>| {
        match r {
            Ok(path) => {
                let n = gio::Notification::new("Library added");
                n.set_body(Some(&format!("Files from the library appear in {path}")));
                app.gtk.send_notification(Some("odopen"), &n);
                util::open_uri(app.window().as_ref(), &path);
            }
            Err(e) => {
                let d = adw::AlertDialog::new(Some("Couldn\u{2019}t sync this library"), Some(&util::friendly(&e)));
                d.add_response("ok", "OK");
                d.present(app.window().as_ref());
            }
        }
    });
}
