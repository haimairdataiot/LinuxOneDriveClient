//! Small UI helpers.

use adw::prelude::*;
use gtk::{gdk, gio};
use std::path::{Path, PathBuf};

/// Directory with our icons when running from a source checkout.
pub fn dev_icon_dir() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/icons");
    p.canonicalize().ok().filter(|p| p.is_dir())
}

/// Whether our icons are installed system-wide.
pub fn icons_installed() -> bool {
    ["/usr/share/icons/hicolor", "/usr/local/share/icons/hicolor"]
        .iter()
        .any(|d| Path::new(d).join("symbolic/apps/linux-onedrive-synced-symbolic.svg").exists())
}

pub fn install_icon_search_path() {
    if !icons_installed()
        && let (Some(dir), Some(display)) = (dev_icon_dir(), gdk::Display::default())
    {
        gtk::IconTheme::for_display(&display).add_search_path(dir);
    }
}

/// A row that acts as a button (libadwaita 1.6 has `ButtonRow`; this also
/// works with 1.5, which Ubuntu 24.04 ships).
pub fn button_row(title: &str, start_icon: Option<&str>, end_icon: Option<&str>) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).activatable(true).build();
    if let Some(icon) = start_icon {
        row.add_prefix(&gtk::Image::from_icon_name(icon));
    }
    if let Some(icon) = end_icon {
        row.add_suffix(&gtk::Image::from_icon_name(icon));
    }
    row
}

/// Open a folder, file or web address with the default application.
pub fn open_uri(parent: Option<&gtk::Window>, uri: &str) {
    // Local files and folders go through the file launcher: the desktop
    // portal's link handler, which the URI launcher uses, refuses file://
    // addresses ("The application launch failed").
    let file = if uri.starts_with('/') {
        Some(gio::File::for_path(uri))
    } else if uri.starts_with("file://") {
        Some(gio::File::for_uri(uri))
    } else {
        None
    };
    match file {
        Some(f) => gtk::FileLauncher::new(Some(&f)).launch(parent, gio::Cancellable::NONE, |r| {
            if let Err(e) = r {
                tracing::warn!("could not open: {e}");
            }
        }),
        None => gtk::UriLauncher::new(uri).launch(parent, gio::Cancellable::NONE, |r| {
            if let Err(e) = r {
                tracing::warn!("could not open: {e}");
            }
        }),
    }
}

/// Show a file in its folder (select it in the file manager).
pub fn show_in_folder(parent: Option<&gtk::Window>, path: &str) {
    let file = gio::File::for_path(path);
    gtk::FileLauncher::new(Some(&file)).open_containing_folder(parent, gio::Cancellable::NONE, |r| {
        if let Err(e) = r {
            tracing::warn!("could not show in folder: {e}");
        }
    });
}

/// Themed icon for a file name, like the file manager would use.
pub fn file_icon(name: &str, is_dir: bool) -> gio::Icon {
    if is_dir {
        return gio::ThemedIcon::new("folder").upcast();
    }
    let (ctype, _) = gio::content_type_guess(Some(name), None::<&[u8]>);
    gio::content_type_get_icon(&ctype)
}

pub fn copy_to_clipboard(widget: &impl IsA<gtk::Widget>, text: &str) {
    widget.as_ref().clipboard().set_text(text);
}

pub fn relative_time(unix: i64) -> String {
    let t = chrono::DateTime::from_timestamp(unix, 0).unwrap_or_default();
    od_core::fmt::relative_time(t, chrono::Utc::now())
}

/// Show a toast if the widget lives inside an AdwToastOverlay.
pub fn toast(overlay: &adw::ToastOverlay, text: &str) {
    overlay.add_toast(adw::Toast::new(text));
}

pub fn error_dialog(parent: &impl IsA<gtk::Widget>, heading: &str, body: &str) {
    let d = adw::AlertDialog::new(Some(heading), Some(body));
    d.add_response("ok", "OK");
    d.present(Some(parent));
}

/// Strip zbus's "org.freedesktop.DBus.Error.Failed: " prefix for display.
pub fn friendly(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(_, Some(msg), _) => msg.clone(),
        other => other.to_string(),
    }
}

pub fn status_title(state: &str) -> &'static str {
    match state {
        "idle" => "Your files are synced",
        "syncing" | "starting" => "Syncing your files",
        "paused" => "Syncing is paused",
        "offline" => "You're offline",
        "error" => "Something went wrong",
        "signin-required" => "Sign in to keep syncing",
        _ => "Set up Linux OneDrive",
    }
}

pub fn state_icon(state: &str) -> &'static str {
    match state {
        "idle" => "linux-onedrive-synced-symbolic",
        "syncing" | "starting" => "linux-onedrive-syncing-symbolic",
        "paused" => "linux-onedrive-paused-symbolic",
        "offline" => "linux-onedrive-offline-symbolic",
        "error" => "linux-onedrive-error-symbolic",
        _ => "linux-onedrive-signedout-symbolic",
    }
}
