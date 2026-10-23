//! App updates: the "Software Update" dialog, the Updates rows in Settings
//! and the install step.
//!
//! The sync service finds new releases on GitHub and downloads and verifies
//! the package (see `od_core::update`). Installing is done here because it
//! needs the user's approval: the system's own administrator prompt
//! (polkit) is shown before the package manager runs. Nothing is installed
//! without that prompt.

use crate::client;
use crate::{App, util};
use adw::prelude::*;
use gtk::{gio, glib};
use od_core::update::UpdateInfo;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

async fn info(force: bool) -> zbus::Result<UpdateInfo> {
    let json = client::manager().await?.check_for_update(force).await?;
    Ok(serde_json::from_str(&json).unwrap_or_default())
}

/// One line saying where things stand.
pub fn summary(u: &UpdateInfo) -> String {
    if !u.error.is_empty() {
        format!("Couldn\u{2019}t check for updates: {}", u.error)
    } else if u.available {
        format!("Version {} is available", u.latest)
    } else {
        "You have the latest version".to_string()
    }
}

/// "Updates" in Settings: the version, a check button and the automatic check switch.
pub fn settings_group(
    app: &Rc<App>,
    check_updates: bool,
    set: impl Fn(serde_json::Value) + 'static,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Updates").build();
    let row = adw::ActionRow::builder()
        .title(format!("{} {}", od_core::APP_NAME, od_core::VERSION))
        .subtitle("Checking\u{2026}")
        .build();
    let button = gtk::Button::builder().label("Check Now").valign(gtk::Align::Center).build();
    row.add_suffix(&button);
    group.add(&row);
    let auto = adw::SwitchRow::builder()
        .title("Check for updates automatically")
        .subtitle("Once a day. Updates are never installed without asking you.")
        .active(check_updates)
        .build();
    auto.connect_active_notify(move |r| set(serde_json::json!({"general": {"check_updates": r.is_active()}})));
    group.add(&auto);

    let available = Rc::new(std::cell::Cell::new(false));
    let show: Rc<dyn Fn(zbus::Result<UpdateInfo>)> = {
        let (row, button, available) = (row.clone(), button.clone(), available.clone());
        Rc::new(move |r| {
            button.set_sensitive(true);
            match r {
                Ok(u) => {
                    row.set_subtitle(&summary(&u));
                    available.set(u.available);
                    button.set_label(if u.available { "Update\u{2026}" } else { "Check Now" });
                    if u.available {
                        button.add_css_class("suggested-action");
                    } else {
                        button.remove_css_class("suggested-action");
                    }
                }
                Err(e) => row.set_subtitle(&util::friendly(&e)),
            }
        })
    };
    let s = show.clone();
    client::call(info(false), move |r| s(r));
    let app = app.clone();
    button.connect_clicked(move |b| {
        if available.get() {
            dialog(&app, b.root().and_downcast::<gtk::Window>().map(|w| w.upcast()));
            return;
        }
        b.set_sensitive(false);
        let s = show.clone();
        client::call(info(true), move |r| s(r));
    });
    group
}

/// The update dialog (from the notification, the tray, Settings or `--show update`).
pub fn dialog(app: &Rc<App>, parent: Option<gtk::Widget>) {
    if present_open() {
        return;
    }
    let app = app.clone();
    client::call(info(true), move |r| {
        let u = match r {
            Ok(u) => u,
            Err(e) => UpdateInfo { error: util::friendly(&e), current: od_core::VERSION.into(), ..Default::default() },
        };
        build_dialog(&app, parent, u);
    });
}

fn build_dialog(app: &Rc<App>, parent: Option<gtk::Widget>, u: UpdateInfo) {
    let dialog = adw::Dialog::builder().title("Software Update").content_width(460).build();
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::new());
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
    vbox.set_margin_top(6);
    vbox.set_margin_bottom(18);
    vbox.set_margin_start(18);
    vbox.set_margin_end(18);
    let title = gtk::Label::builder()
        .label(if u.available {
            format!("{} {} is available", od_core::APP_NAME, u.latest)
        } else if u.error.is_empty() {
            format!("{} is up to date", od_core::APP_NAME)
        } else {
            "Couldn\u{2019}t check for updates".to_string()
        })
        .css_classes(["title-2"])
        .wrap(true)
        .build();
    let sub = gtk::Label::builder()
        .label(if !u.error.is_empty() { u.error.clone() } else { format!("You have version {}.", u.current) })
        .css_classes(["dim-label"])
        .wrap(true)
        .build();
    vbox.append(&gtk::Image::builder().icon_name(od_core::APP_ID).pixel_size(64).build());
    vbox.append(&title);
    vbox.append(&sub);
    if u.available && !u.notes.trim().is_empty() {
        let notes = gtk::Label::builder()
            .label(u.notes.trim())
            .wrap(true)
            .xalign(0.0)
            .yalign(0.0)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(12)
            .margin_end(12)
            .build();
        let frame = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(260)
            .propagate_natural_height(true)
            .css_classes(["card"])
            .child(&notes)
            .build();
        vbox.append(&gtk::Label::builder().label("What\u{2019}s new").xalign(0.0).css_classes(["heading"]).build());
        vbox.append(&frame);
    }
    let status = gtk::Label::builder().css_classes(["dim-label", "caption"]).wrap(true).visible(false).build();
    vbox.append(&status);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::Center);
    buttons.set_margin_top(6);
    let later =
        gtk::Button::builder().label(if u.available { "Not Now" } else { "Close" }).css_classes(["pill"]).build();
    // While the system's password prompt is up, this button cancels instead.
    let running: Running = Rc::new(RefCell::new(None));
    let (d, r) = (dialog.clone(), running.clone());
    later.connect_clicked(move |_| match r.borrow().as_ref() {
        Some(proc) => proc.send_signal(SIGTERM),
        None => {
            d.close();
        }
    });
    buttons.append(&later);
    if u.available && u.installable {
        let install = gtk::Button::builder().label("Install Update").css_classes(["pill", "suggested-action"]).build();
        let (app, status, later, dialog2, version) =
            (app.clone(), status.clone(), later.clone(), dialog.clone(), u.latest.clone());
        install.connect_clicked(move |b| {
            b.set_sensitive(false);
            later.set_sensitive(false);
            dialog2.set_can_close(false);
            status.set_visible(true);
            status.set_text("Downloading and checking the update\u{2026}");
            let (app, status, later, b2, dialog3, version) =
                (app.clone(), status.clone(), later.clone(), b.clone(), dialog2.clone(), version.clone());
            let label = later.label().unwrap_or_default();
            install_update(running.clone(), move |step| {
                // Cancel only while nothing has been changed yet.
                let approving = matches!(step, Step::Approving | Step::NoPrompt(_));
                later.set_label(if approving { "Cancel" } else { label.as_str() });
                later.set_sensitive(approving);
                match step {
                    Step::Approving => status.set_text("Waiting for your approval to install\u{2026}"),
                    Step::NoPrompt(path) => {
                        status.set_text(&format!(
                            "Waiting for your approval to install\u{2026} If no password prompt has appeared, it may \
                             be on another screen or in another session. You can cancel and install the update from a \
                             terminal instead:\nsudo apt install \"{path}\""
                        ));
                        // So the command can be copied.
                        status.set_selectable(true);
                    }
                    Step::Busy(other) => status.set_text(&format!(
                        "Waiting for another software update to finish{}\u{2026}",
                        if other.is_empty() { String::new() } else { format!(" ({other})") }
                    )),
                    Step::Installing => status.set_text("Installing the update\u{2026}"),
                    Step::Done => {
                        dialog3.set_can_close(true);
                        finished(&app, &dialog3, &version);
                    }
                    Step::Failed(msg) => {
                        status.set_text(&msg);
                        status.set_selectable(msg.contains("sudo "));
                        b2.set_sensitive(true);
                        later.set_sensitive(true);
                        dialog3.set_can_close(true);
                    }
                }
            });
        });
        buttons.append(&install);
    } else if u.available {
        let open = gtk::Button::builder().label("Open Download Page").css_classes(["pill", "suggested-action"]).build();
        let url = u.page_url.clone();
        open.connect_clicked(move |b| util::open_uri(b.root().and_downcast_ref::<gtk::Window>(), &url));
        buttons.append(&open);
    }
    vbox.append(&buttons);
    tv.set_content(Some(&vbox));
    dialog.set_child(Some(&tv));
    OPEN.with(|o| *o.borrow_mut() = Some(dialog.clone()));
    dialog.connect_closed(|_| OPEN.with(|o| *o.borrow_mut() = None));
    dialog.present(parent.as_ref());
}

thread_local! {
    /// The update dialog while it is open. Asking for it again brings it
    /// back instead of opening a second one, so one install runs at a time.
    static OPEN: RefCell<Option<adw::Dialog>> = const { RefCell::new(None) };
}

/// Bring back the open update dialog, if there is one.
fn present_open() -> bool {
    let Some(dialog) = OPEN.with(|o| o.borrow().clone()) else { return false };
    match dialog.root().and_downcast::<gtk::Window>() {
        Some(window) => {
            window.present();
            true
        }
        None => false,
    }
}

/// The installer while the system asks for approval (Cancel stops it).
type Running = Rc<RefCell<Option<gio::Subprocess>>>;

const SIGTERM: i32 = 15;

enum Step {
    Approving,
    /// Still no approval after a while: the prompt may not be visible.
    /// Carries the downloaded package, for installing it by hand.
    NoPrompt(String),
    /// The package manager waits for another update (named if known).
    Busy(String),
    Installing,
    Done,
    Failed(String),
}

/// Download (verified by the service), then install with the system's
/// administrator prompt. Progress comes from the installer's output: pkexec
/// prints nothing while the prompt is up, so its first line from apt means
/// the update was approved.
fn install_update(running: Running, report: impl Fn(Step) + 'static) {
    let report = Rc::new(report);
    client::call(async { client::manager().await?.download_update().await }, move |r: zbus::Result<String>| {
        let path = match r {
            Ok(p) => p,
            Err(e) => return report(Step::Failed(util::friendly(&e))),
        };
        report(Step::Approving);
        // Wait for another software update (automatic updates, for example)
        // to finish rather than failing at once.
        let argv = ["pkexec", "/usr/bin/apt-get", "-o", "DPkg::Lock::Timeout=600", "install", "-y", path.as_str()];
        let flags = gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_MERGE;
        let proc = match gio::Subprocess::newv(&argv.map(std::ffi::OsStr::new), flags) {
            Ok(p) => p,
            Err(_) => {
                return report(Step::Failed(format!(
                    "The update is downloaded, but this system has no administrator prompt (pkexec). Install it from a terminal: sudo apt install \"{path}\""
                )));
            }
        };
        *running.borrow_mut() = Some(proc.clone());
        let approved = Rc::new(Cell::new(false));
        {
            let (approved, running, report, path) = (approved.clone(), running.clone(), report.clone(), path.clone());
            glib::timeout_add_seconds_local_once(20, move || {
                if !approved.get() && running.borrow().is_some() {
                    report(Step::NoPrompt(path));
                }
            });
        }
        glib::spawn_future_local(async move {
            let mut log = String::new();
            let mut line = String::new();
            let mut busy: Option<String> = None;
            if let Some(out) = proc.stdout_pipe() {
                while let Ok(chunk) = out.read_bytes_future(4096, glib::Priority::DEFAULT).await {
                    if chunk.is_empty() {
                        break;
                    }
                    let text = String::from_utf8_lossy(&chunk);
                    log.push_str(&text);
                    for c in text.chars() {
                        if c != '\n' && c != '\r' {
                            line.push(c);
                            continue;
                        }
                        let l = std::mem::take(&mut line);
                        let l = l.trim();
                        if l.is_empty() || l.starts_with("Error executing command as another user") {
                            continue;
                        }
                        if !approved.get() {
                            approved.set(true);
                            *running.borrow_mut() = None;
                        }
                        let holder = lock_holder(l);
                        if holder != busy {
                            busy = holder.clone();
                            report(match holder {
                                Some(h) => Step::Busy(h),
                                None => Step::Installing,
                            });
                        }
                    }
                }
            }
            let _ = proc.wait_future().await;
            let cancelled = running.borrow_mut().take().is_some() && !proc.has_exited();
            let code = if proc.has_exited() { proc.exit_status() } else { -1 };
            report(match code {
                0 => Step::Done,
                _ if cancelled => Step::Failed("Cancelled. The update wasn\u{2019}t installed.".into()),
                _ if log.contains("No authentication agent found") => Step::Failed(format!(
                    "This session has no administrator prompt, so the update can\u{2019}t be approved here. Install it from a terminal: sudo apt install \"{path}\""
                )),
                // pkexec: 126 = dismissed, 127 = not authorised.
                126 | 127 => {
                    Step::Failed("The update wasn\u{2019}t installed because it wasn\u{2019}t approved.".into())
                }
                _ if log.contains("Unable to acquire the dpkg frontend lock") => {
                    Step::Failed("Another software update is still running. Try again when it has finished.".into())
                }
                _ => {
                    let last = log.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("unknown error");
                    Step::Failed(format!("The installer reported a problem: {last}"))
                }
            });
        });
    });
}

/// "... It is held by process 1234 (unattended-upgr)" from apt: who holds
/// the package system, in words (empty when apt doesn't say).
fn lock_holder(line: &str) -> Option<String> {
    if !line.contains("Could not get lock") {
        return None;
    }
    let name = line.rsplit_once('(').and_then(|(_, rest)| rest.split_once(')')).map(|(n, _)| n).unwrap_or("");
    Some(match name {
        n if n.starts_with("unattended") => "automatic updates".to_string(),
        "packagekitd" => "Software".to_string(),
        n => n.to_string(),
    })
}

/// The new version is installed: restart the sync service and this app so
/// both run it.
fn finished(app: &Rc<App>, dialog: &adw::Dialog, version: &str) {
    let alert = adw::AlertDialog::new(
        Some(&format!("Updated to version {version}")),
        Some(&format!("{} will restart now to finish.", od_core::APP_NAME)),
    );
    alert.add_response("ok", "Restart");
    alert.set_default_response(Some("ok"));
    let app = app.clone();
    alert.connect_response(None, move |_, _| restart(&app));
    alert.present(Some(dialog));
}

fn restart(app: &Rc<App>) {
    use std::process::Command;
    let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
    let managed = Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "linux-onedrived.service"])
        .status()
        .is_ok_and(|s| s.success());
    let gtk_app = app.gtk.clone();
    let relaunch = move || {
        // Start the new app once this one has gone.
        let _ = Command::new("sh").args(["-c", "sleep 1; exec linux-onedrive --background"]).spawn();
        gtk_app.quit();
    };
    if managed {
        let _ = Command::new("systemctl").args(["--user", "restart", "linux-onedrived.service"]).status();
        relaunch();
    } else {
        // Not run by systemd: ask it to stop; the new app starts the new service.
        client::call(async { client::manager().await?.quit().await }, move |_: zbus::Result<()>| relaunch());
    }
}
