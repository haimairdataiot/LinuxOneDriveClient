//! XDG-compliant locations for configuration, state and file content.
//!
//! File *content* (including edits not yet uploaded) lives under the XDG data
//! directory, never under `~/.cache`, because cache cleaners would destroy
//! unsynced user data.

use std::path::{Path, PathBuf};

const DIR_NAME: &str = "linux-onedrive";

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if Path::new(&v).is_absolute() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

/// `~/.config/linux-onedrive`
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join(DIR_NAME)
}

/// `~/.config/linux-onedrive/config.toml`
pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// `~/.local/share/linux-onedrive`
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join(DIR_NAME)
}

/// `~/.local/state/linux-onedrive`
pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join(DIR_NAME)
}

/// `~/.cache/linux-onedrive` (only for disposable data such as thumbnails).
pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join(DIR_NAME)
}

/// `~/.config/autostart`
pub fn autostart_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("autostart")
}

/// Per-account data directory holding `state.db`, `content/` and `partial/`.
pub fn account_dir(account_id: &str) -> PathBuf {
    data_dir().join("accounts").join(account_id)
}

pub fn log_dir() -> PathBuf {
    state_dir().join("logs")
}

/// Default mount point for a newly added account.
///
/// Personal accounts get `~/OneDrive`; work or school accounts get
/// `~/OneDrive - <Organization>` like the official client.
pub fn default_mount_point(organization: Option<&str>) -> PathBuf {
    match organization.map(str::trim).filter(|o| !o.is_empty()) {
        Some(org) => {
            let safe: String = org.chars().map(|c| if c == '/' || c == '\0' { '_' } else { c }).collect();
            home().join(format!("OneDrive - {safe}"))
        }
        None => home().join("OneDrive"),
    }
}

/// Home directory of the current user.
pub fn home_dir() -> PathBuf {
    home()
}

/// Create a directory (and parents) readable only by the owner.
pub fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if !path.exists() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    } else {
        let meta = std::fs::metadata(path)?;
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn business_mount_point_has_org_suffix() {
        let p = default_mount_point(Some("Contoso Ltd"));
        assert!(p.ends_with("OneDrive - Contoso Ltd"));
        let p = default_mount_point(Some("A/B"));
        assert!(p.ends_with("OneDrive - A_B"));
        assert!(default_mount_point(None).ends_with("OneDrive"));
        assert!(default_mount_point(Some("  ")).ends_with("OneDrive"));
    }
}
