//! `~/.config/linux-onedrive/config.toml`: global preferences and the list of
//! linked accounts. The daemon is the only writer; the GUI and CLI change
//! settings through the daemon's D-Bus API.

use crate::types::DriveType;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The project's application (client) ID in Microsoft Entra ID. A client ID
/// is public by nature (it is visible in every sign-in address), not a
/// secret. Forks and organisations should register their own application
/// and build with `LINUX_ONEDRIVE_CLIENT_ID=<id>`, or set `general.client_id`
/// in the config file.
pub const PROJECT_CLIENT_ID: &str = "c53e40be-ecf6-4ce5-bbc2-aa756a2cee0b";

/// The client ID this build uses unless the config file names another one.
pub const BUILTIN_CLIENT_ID: &str = match option_env!("LINUX_ONEDRIVE_CLIENT_ID") {
    Some(id) => id,
    None => PROJECT_CLIENT_ID,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub general: General,
    pub network: Network,
    pub notifications: Notifications,
    #[serde(rename = "account")]
    pub accounts: Vec<AccountConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct General {
    /// Entra ID application (client) ID. Empty = use the built-in one.
    pub client_id: String,
    pub start_at_login: bool,
    pub pause_on_metered: bool,
    pub pause_on_power_saver: bool,
    /// Dehydrate unpinned files not opened for this many days (0 = never).
    pub auto_free_up_days: u32,
    /// Hold deletes and ask when more than this many are queued within a minute (0 = never ask).
    pub mass_delete_threshold: u32,
    /// Seconds between delta polls while idle (push notifications trigger earlier polls).
    pub poll_interval_secs: u64,
    /// Extra glob patterns kept local-only.
    pub excluded_patterns: Vec<String>,
    /// Refuse to download online-only files when opened by thumbnailers/indexers.
    pub block_background_hydration: bool,
    /// Show OneDrive's previews of online-only photos, videos and PDFs.
    pub server_thumbnails: bool,
    /// "Copy link" default: "auto" (Microsoft's default for the account type),
    /// "anyone-edit", "anyone-view", "organization-view", "organization-edit".
    pub default_link: String,
    /// tracing filter, e.g. "info" or "od_engine=debug,info".
    pub log_level: String,
    /// Look for a new version once a day and tell the user (never installs by itself).
    pub check_updates: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            start_at_login: true,
            pause_on_metered: true,
            pause_on_power_saver: false,
            auto_free_up_days: 0,
            mass_delete_threshold: 200,
            poll_interval_secs: 60,
            check_updates: true,
            excluded_patterns: Vec::new(),
            block_background_hydration: true,
            server_thumbnails: true,
            default_link: "auto".into(),
            log_level: "info".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Network {
    /// KiB/s, 0 = unlimited.
    pub upload_limit_kbps: u32,
    /// KiB/s, 0 = unlimited.
    pub download_limit_kbps: u32,
    pub max_parallel_uploads: u32,
    pub max_parallel_downloads: u32,
    /// Use the real-time socket.io notification channel when available.
    pub push_notifications: bool,
}

impl Default for Network {
    fn default() -> Self {
        Self {
            upload_limit_kbps: 0,
            download_limit_kbps: 0,
            max_parallel_uploads: 4,
            max_parallel_downloads: 4,
            push_notifications: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Notifications {
    pub enabled: bool,
    pub mass_delete: bool,
    pub sync_paused: bool,
    pub conflicts: bool,
    pub storage_full: bool,
    pub sync_errors: bool,
    pub shared_with_me: bool,
    pub offline_open: bool,
    /// Remind that deleting in the OneDrive folder deletes everywhere.
    #[serde(default = "default_true")]
    pub delete_reminder: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            enabled: true,
            mass_delete: true,
            sync_paused: true,
            conflicts: true,
            storage_full: true,
            sync_errors: true,
            shared_with_me: true,
            offline_open: true,
            delete_reminder: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccountConfig {
    /// Local identifier (hex), used in paths and D-Bus object paths.
    pub id: String,
    pub email: String,
    #[serde(default)]
    pub display_name: String,
    /// Organization name for work/school accounts.
    #[serde(default)]
    pub organization: String,
    pub drive_id: String,
    pub drive_type: DriveType,
    /// OAuth tenant segment used for refresh ("consumers", "organizations" or a tenant id).
    #[serde(default = "default_tenant")]
    pub tenant: String,
    pub mount_point: PathBuf,
    /// Equivalent of turning Files On-Demand off: keep everything downloaded.
    #[serde(default)]
    pub download_all: bool,
    /// Remote folder paths hidden from this device ("Choose folders").
    #[serde(default)]
    pub excluded_folders: Vec<String>,
    /// XDG folders backed up into this account (Desktop, Documents, …).
    #[serde(default)]
    pub backup_folders: Vec<String>,
    /// Account is linked but syncing is disabled (e.g. after "Unlink" is pending).
    #[serde(default)]
    pub disabled: bool,
    /// First-run setup (folder location) has been confirmed.
    #[serde(default = "default_true")]
    pub setup_complete: bool,
    /// Web address of the drive ("View online").
    #[serde(default)]
    pub web_url: String,
    /// Folders backed up into this account (Desktop, Documents, …).
    #[serde(default)]
    pub backups: Vec<BackupEntry>,
    /// Extra OAuth scopes granted later (e.g. People.Read for suggestions).
    #[serde(default)]
    pub extra_scopes: Vec<String>,
    /// SharePoint document libraries synced with this account.
    #[serde(default)]
    pub libraries: Vec<LibraryEntry>,
}

/// A SharePoint document library synced on this device.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LibraryEntry {
    pub drive_id: String,
    pub site: String,
    pub name: String,
    #[serde(default)]
    pub web_url: String,
    pub mount_point: PathBuf,
}

/// A folder moved into OneDrive by "Manage back up".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupEntry {
    /// XDG key: DESKTOP, DOCUMENTS, PICTURES, MUSIC or VIDEOS.
    pub key: String,
    /// Where the folder was before (now a link to the OneDrive copy).
    pub original: PathBuf,
}

fn default_true() -> bool {
    true
}

fn default_tenant() -> String {
    "common".into()
}

impl Config {
    /// Effective client id (config overrides the build-time default).
    pub fn client_id(&self) -> Option<String> {
        let c = self.general.client_id.trim();
        if !c.is_empty() {
            return Some(c.to_string());
        }
        if let Ok(env) = std::env::var("LINUX_ONEDRIVE_CLIENT_ID")
            && !env.trim().is_empty()
        {
            return Some(env.trim().to_string());
        }
        Some(BUILTIN_CLIENT_ID.trim().to_string()).filter(|s| !s.is_empty())
    }

    /// Sign-in goes through the project's own app registration (as opposed
    /// to one supplied by the user, their organisation or a fork).
    pub fn uses_project_client_id(&self) -> bool {
        self.client_id().as_deref() == Some(PROJECT_CLIENT_ID)
    }

    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&crate::paths::config_file())
    }

    /// Atomically write the file (temp file + rename) with 0600 permissions.
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = path.parent() {
            crate::paths::ensure_private_dir(dir)?;
        }
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        {
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.write_all(b"# linux-onedrive configuration. Managed by linux-onedrived; edit while it is stopped.\n")?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&crate::paths::config_file())
    }

    pub fn account(&self, id: &str) -> Option<&AccountConfig> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn account_mut(&mut self, id: &str) -> Option<&mut AccountConfig> {
        self.accounts.iter_mut().find(|a| a.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg/config.toml");
        let missing = Config::load_from(&path).unwrap();
        assert_eq!(missing, Config::default());

        let mut cfg = Config::default();
        cfg.general.client_id = "abc".into();
        cfg.network.upload_limit_kbps = 512;
        cfg.accounts.push(AccountConfig {
            id: "0a1b2c".into(),
            email: "me@example.com".into(),
            display_name: "Me".into(),
            organization: String::new(),
            drive_id: "D1".into(),
            drive_type: DriveType::Personal,
            tenant: "consumers".into(),
            mount_point: "/home/me/OneDrive".into(),
            download_all: false,
            excluded_folders: vec![],
            backup_folders: vec!["Documents".into()],
            disabled: false,
            setup_complete: true,
            web_url: String::new(),
            backups: vec![],
            extra_scopes: vec![],
            libraries: vec![],
        });
        cfg.save_to(&path).unwrap();
        let back = Config::load_from(&path).unwrap();
        assert_eq!(back, cfg);
        assert_eq!(back.client_id().as_deref(), Some("abc"));

        // Partial files fill in defaults.
        std::fs::write(&path, "[general]\npoll_interval_secs = 15\n").unwrap();
        let partial = Config::load_from(&path).unwrap();
        assert_eq!(partial.general.poll_interval_secs, 15);
        assert!(partial.general.pause_on_metered);
        assert_eq!(partial.network.max_parallel_uploads, 4);
    }
}
