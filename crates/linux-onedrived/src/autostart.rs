//! "Start Linux OneDrive when I sign in": the systemd user unit for the
//! daemon plus the XDG autostart entry for the tray app.

use std::path::PathBuf;

pub const UNIT: &str = "linux-onedrived.service";
const DESKTOP_FILE: &str = "com.airdataiot.LinuxOneDrive.desktop";

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
trait SystemdManager {
    fn enable_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
        force: bool,
    ) -> zbus::Result<(bool, Vec<(String, String, String)>)>;
    fn disable_unit_files(&self, files: &[&str], runtime: bool) -> zbus::Result<Vec<(String, String, String)>>;
    fn reload(&self) -> zbus::Result<()>;
}

fn user_entry() -> PathBuf {
    od_core::paths::autostart_dir().join(DESKTOP_FILE)
}

fn system_entry_exists() -> bool {
    std::env::var("XDG_CONFIG_DIRS")
        .unwrap_or_else(|_| "/etc/xdg".into())
        .split(':')
        .any(|d| std::path::Path::new(d).join("autostart").join(DESKTOP_FILE).exists())
}

/// Enable or disable starting at login.
pub async fn set_enabled(conn: &zbus::Connection, enabled: bool) -> anyhow::Result<()> {
    // Tray app: the package installs /etc/xdg/autostart/<id>.desktop; a user
    // override with Hidden=true disables it (XDG Autostart spec).
    let entry = user_entry();
    if enabled {
        if system_entry_exists() {
            let _ = std::fs::remove_file(&entry);
        } else {
            std::fs::create_dir_all(entry.parent().unwrap())?;
            let exe = crate::notify::sibling_binary("linux-onedrive");
            std::fs::write(
                &entry,
                format!(
                    "[Desktop Entry]\nType=Application\nName={}\nExec={} --background\nIcon={}\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
                    od_core::APP_NAME,
                    exe.display(),
                    od_core::APP_ID
                ),
            )?;
        }
    } else {
        std::fs::create_dir_all(entry.parent().unwrap())?;
        std::fs::write(
            &entry,
            format!(
                "[Desktop Entry]\nType=Application\nName={}\nHidden=true\nX-GNOME-Autostart-enabled=false\n",
                od_core::APP_NAME
            ),
        )?;
    }

    // Daemon: enable/disable the user unit (the running instance is unaffected).
    let sd = SystemdManagerProxy::new(conn).await?;
    if enabled {
        let _ = sd.enable_unit_files(&[UNIT], false, true).await;
    } else {
        let _ = sd.disable_unit_files(&[UNIT], false).await;
    }
    let _ = sd.reload().await;
    Ok(())
}
