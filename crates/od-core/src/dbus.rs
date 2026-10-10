//! The daemon's D-Bus contract: names, object paths, client proxies and the
//! JSON payloads exchanged for structured data.
//!
//! Simple values travel as native D-Bus types; lists of records (activity,
//! issues, versions, settings) travel as JSON strings so the GUI, CLI and the
//! Python Nautilus extension can share one schema without hand-written
//! variant marshalling.

use serde::{Deserialize, Serialize};
use zbus::zvariant::OwnedObjectPath;

pub const BUS_NAME: &str = "com.airdataiot.LinuxOneDrive.Daemon";
pub const MANAGER_PATH: &str = "/com/airdataiot/LinuxOneDrive";
pub const MANAGER_IFACE: &str = "com.airdataiot.LinuxOneDrive.Manager";
pub const ACCOUNT_IFACE: &str = "com.airdataiot.LinuxOneDrive.Account";

/// Object path for an account's interface.
pub fn account_path(account_id: &str) -> String {
    format!("{MANAGER_PATH}/account/{account_id}")
}

/// Global daemon interface.
#[zbus::proxy(
    interface = "com.airdataiot.LinuxOneDrive.Manager",
    default_service = "com.airdataiot.LinuxOneDrive.Daemon",
    default_path = "/com/airdataiot/LinuxOneDrive"
)]
pub trait Manager {
    /// Start an interactive sign-in. `flow` is "embedded" (the GUI shows the
    /// returned URL in its WebKit view and intercepts `redirect_uri`, then calls
    /// `CompleteSignIn`) or "browser" (system browser + loopback listener; the
    /// daemon completes on its own and emits `SignInFinished`).
    /// Returns (authorize_url, redirect_uri).
    fn begin_sign_in(&self, login_hint: &str, flow: &str) -> zbus::Result<(String, String)>;
    /// Finish an embedded sign-in with the intercepted redirect URL. Returns the
    /// new (or re-authenticated) account's object path.
    fn complete_sign_in(&self, redirect_url: &str) -> zbus::Result<OwnedObjectPath>;
    /// Start a device-code sign-in. Returns (user_code, verification_uri, message).
    fn begin_device_sign_in(&self) -> zbus::Result<(String, String, String)>;
    fn cancel_sign_in(&self) -> zbus::Result<()>;
    /// Pause all accounts for `seconds` (0 = until resumed).
    fn pause(&self, seconds: u32) -> zbus::Result<()>;
    fn resume(&self) -> zbus::Result<()>;
    /// Settings document as JSON ([`SettingsDoc`]).
    fn get_settings(&self) -> zbus::Result<String>;
    /// Merge a partial [`SettingsDoc`] JSON object into the settings.
    fn set_settings(&self, json: &str) -> zbus::Result<()>;
    /// Status of local paths inside any mount, as JSON array of [`PathInfo`].
    fn paths_info(&self, paths: &[&str]) -> zbus::Result<String>;
    /// Ask the daemon to unmount everything and exit.
    fn quit(&self) -> zbus::Result<()>;
    /// Handle an `odopen://` link (the "Sync" button of a SharePoint library
    /// in the browser): start syncing that library. Returns its local folder.
    fn open_sync_link(&self, url: &str) -> zbus::Result<String>;
    /// Is a newer version published? Returns [`crate::update::UpdateInfo`]
    /// JSON. `force` asks GitHub now instead of using a recent answer.
    fn check_for_update(&self, force: bool) -> zbus::Result<String>;
    /// Download the newest version's package for this system and verify it
    /// against the release's checksums. Returns the path of the package;
    /// installing it is up to the caller (it needs administrator approval).
    fn download_update(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    /// Unix time when a user pause ends; 0 = not paused, -1 = until resumed.
    #[zbus(property)]
    fn paused_until(&self) -> zbus::Result<i64>;
    #[zbus(property)]
    fn pause_reason(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn online(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn accounts(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;
    /// True when no Entra ID application (client) ID is configured.
    #[zbus(property)]
    fn needs_client_id(&self) -> zbus::Result<bool>;
    /// Version of a published update that is newer than this one, or empty.
    #[zbus(property)]
    fn update_version(&self) -> zbus::Result<String>;

    #[zbus(signal)]
    fn sign_in_finished(&self, ok: bool, message: &str, account: OwnedObjectPath) -> zbus::Result<()>;
    /// The user deleted synced files for the first time this session:
    /// remind them that deletes happen everywhere.
    #[zbus(signal)]
    fn delete_reminder(&self, account: OwnedObjectPath) -> zbus::Result<()>;
}

/// Per-account interface at `account_path(id)`.
#[zbus::proxy(
    interface = "com.airdataiot.LinuxOneDrive.Account",
    default_service = "com.airdataiot.LinuxOneDrive.Daemon"
)]
pub trait Account {
    /// Set the "keep on this device" state: "pinned", "inherit" or "unpinned".
    fn set_pin_state(&self, paths: &[&str], state: &str) -> zbus::Result<()>;
    /// Create a sharing link. kind: "view"|"edit"; scope: "anonymous"|"organization".
    /// (Compatibility wrapper; see `CreateLink`.)
    fn create_share_link(&self, path: &str, kind: &str, scope: &str) -> zbus::Result<String>;
    /// [`SharingInfo`] JSON: what this item/account can share.
    fn get_sharing_info(&self, path: &str) -> zbus::Result<String>;
    /// "Manage access": JSON array of [`AccessEntry`].
    fn list_access(&self, path: &str) -> zbus::Result<String>;
    /// Create a link from [`LinkOptions`] JSON; returns [`LinkResult`] JSON.
    fn create_link(&self, path: &str, options: &str) -> zbus::Result<String>;
    /// The default link ("Copy link" in the file manager); returns its URL.
    fn copy_link(&self, path: &str) -> zbus::Result<String>;
    /// Share with people from [`InviteOptions`] JSON; returns [`InviteOutcome`] JSON.
    fn invite(&self, path: &str, options: &str) -> zbus::Result<String>;
    /// Change a person's access ("view" | "edit").
    fn set_access_role(&self, path: &str, id: &str, role: &str) -> zbus::Result<()>;
    /// Remove a link or a person's access.
    fn remove_access(&self, path: &str, id: &str) -> zbus::Result<()>;
    /// Remove people from a "specific people" link.
    fn remove_link_people(&self, path: &str, id: &str, emails: &[&str]) -> zbus::Result<()>;
    /// Remove every link and person (except inherited access); returns [`StopSharingOutcome`] JSON.
    fn stop_sharing(&self, path: &str) -> zbus::Result<String>;
    /// Suggestions for the recipient field: JSON array of [`Person`].
    fn search_people(&self, query: &str) -> zbus::Result<String>;
    /// Ask for permission to suggest people (incremental consent). Returns
    /// (authorize_url, redirect_uri) like `BeginSignIn("embedded")`; finish
    /// with `Manager.CompleteSignIn`.
    fn begin_people_consent(&self) -> zbus::Result<(String, String)>;
    fn web_url(&self, path: &str) -> zbus::Result<String>;
    /// JSON array of [`VersionInfo`].
    fn list_versions(&self, path: &str) -> zbus::Result<String>;
    fn restore_version(&self, path: &str, version_id: &str) -> zbus::Result<()>;
    fn download_version(&self, path: &str, version_id: &str, destination: &str) -> zbus::Result<()>;
    /// JSON array of [`ActivityEntry`], newest first.
    fn recent_activity(&self, limit: u32) -> zbus::Result<String>;
    /// JSON array of [`Issue`].
    fn list_issues(&self) -> zbus::Result<String>;
    fn retry_failed(&self) -> zbus::Result<()>;
    /// Answer a held mass delete: true = delete online too, false = restore locally.
    fn resolve_mass_delete(&self, delete_online: bool) -> zbus::Result<()>;
    /// Change where this account is mounted (remounts).
    fn set_mount_point(&self, path: &str) -> zbus::Result<()>;
    /// "Download all files" (true) vs Files On-Demand (false).
    fn set_download_all(&self, enabled: bool) -> zbus::Result<()>;
    /// Hide (true) or show (false) a remote folder on this device ("Choose folders").
    fn set_folder_excluded(&self, remote_path: &str, excluded: bool) -> zbus::Result<()>;
    /// JSON array of [`FolderEntry`] for the top level (or `remote_path`) of the drive.
    fn list_folders(&self, remote_path: &str) -> zbus::Result<String>;
    /// Start syncing after first-run setup chose the location.
    fn start(&self) -> zbus::Result<()>;
    /// Unlink: unmount, forget tokens and local state (keeps nothing online-only).
    fn sign_out(&self) -> zbus::Result<()>;
    /// Force an immediate remote change check.
    fn refresh(&self) -> zbus::Result<()>;
    /// JSON array of [`RemovedItem`]: copies of files deleted online kept on this device.
    fn list_removed(&self) -> zbus::Result<String>;
    /// Put a kept copy back where it was; returns the restored path.
    fn restore_removed(&self, id: &str) -> zbus::Result<String>;
    /// JSON array of [`BackupFolder`] (Desktop, Documents, Pictures, Music, Videos).
    fn list_backup_folders(&self) -> zbus::Result<String>;
    /// Start or stop backing up a folder (key: "DOCUMENTS", …). Runs in the
    /// background; completion is announced with a notification.
    fn set_folder_backup(&self, key: &str, enabled: bool) -> zbus::Result<()>;
    /// Shortcuts to shared folders and SharePoint libraries synced with this
    /// account: JSON array of [`SyncLocation`].
    fn list_locations(&self) -> zbus::Result<String>;
    /// SharePoint sites matching `query` (empty: followed and frequent):
    /// JSON array of [`SiteInfo`].
    fn search_sites(&self, query: &str) -> zbus::Result<String>;
    /// Document libraries of a site: JSON array of [`LibraryInfo`].
    fn site_libraries(&self, site_id: &str) -> zbus::Result<String>;
    /// Start syncing a document library; returns its local folder.
    fn add_library(&self, site_id: &str, drive_id: &str) -> zbus::Result<String>;
    /// "Stop sync" for a library, or remove a shortcut (key from `ListLocations`).
    fn remove_location(&self, key: &str) -> zbus::Result<()>;

    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn email(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn display_name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn organization(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn drive_type(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn mount_point(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn web_root_url(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn status_text(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn quota_used(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn quota_total(&self) -> zbus::Result<u64>;
    /// "normal" | "nearing" | "critical" | "exceeded"
    #[zbus(property)]
    fn quota_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn issue_count(&self) -> zbus::Result<u32>;
    /// JSON [`Progress`].
    #[zbus(property)]
    fn progress(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn needs_setup(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn download_all(&self) -> zbus::Result<bool>;
    /// Number of deletes held by the mass-delete guard (0 = none pending).
    #[zbus(property)]
    fn held_deletes(&self) -> zbus::Result<u32>;

    #[zbus(signal)]
    fn activity_added(&self, json: &str) -> zbus::Result<()>;
    /// Local paths whose status changed (for file-manager emblems).
    #[zbus(signal)]
    fn items_changed(&self, paths: Vec<String>) -> zbus::Result<()>;
}

/// One row of the activity feed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActivityEntry {
    pub id: i64,
    /// Unix seconds.
    pub time: i64,
    pub kind: crate::types::ActivityKind,
    pub name: String,
    /// Absolute local path of the item (may no longer exist for deletes).
    pub path: String,
    /// Display name of the parent folder ("OneDrive" for the root).
    pub folder: String,
    pub size: u64,
    #[serde(default)]
    pub detail: String,
    /// 0..=100 while transferring, -1 when not applicable.
    #[serde(default = "minus_one")]
    pub percent: i32,
}

fn minus_one() -> i32 {
    -1
}

/// A sync problem the user may need to act on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Issue {
    pub path: String,
    pub name: String,
    pub code: String,
    pub message: String,
    /// Unix seconds when first seen.
    pub since: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VersionInfo {
    pub id: String,
    /// Unix seconds.
    pub modified: i64,
    pub size: u64,
    pub modified_by: String,
    pub current: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Progress {
    pub uploads_pending: u32,
    pub downloads_pending: u32,
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// Name of the file currently transferring, if any.
    #[serde(default)]
    pub current: String,
}

impl Progress {
    pub fn files_pending(&self) -> u32 {
        self.uploads_pending + self.downloads_pending
    }
}

/// Status of a local path, used by file-manager integrations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PathInfo {
    pub path: String,
    /// Account object path, empty if the path is not inside a mount.
    pub account: String,
    /// One of the [`crate::types::ItemStatus`] strings, empty if unknown.
    pub status: String,
    pub pin: String,
    pub shared: bool,
    pub is_dir: bool,
    #[serde(default)]
    pub error: String,
}

/// What can be shared, for the Share dialog.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SharingInfo {
    pub name: String,
    pub is_dir: bool,
    /// "personal" | "business" | "documentLibrary"
    pub drive_type: String,
    pub organization: String,
    pub can_share: bool,
    /// Why sharing isn't possible (not uploaded yet…).
    pub reason: String,
    /// Default link: audience ("anyone" | "organization") and role ("view" | "edit").
    pub default_audience: String,
    pub default_role: String,
    /// The recipient field can suggest people from Microsoft 365.
    pub people_suggestions: bool,
    pub web_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Person {
    pub name: String,
    pub email: String,
}

/// One row of "Manage access".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AccessEntry {
    pub id: String,
    /// "owner" | "link" | "person"
    pub kind: String,
    /// For links: "anyone" | "organization" | "users".
    pub audience: String,
    /// "edit" | "view" | "review" | "blocksDownload" | "owner"
    pub role: String,
    pub url: String,
    pub people: Vec<Person>,
    /// Unix seconds; 0 = never.
    pub expires: i64,
    pub has_password: bool,
    /// Folder the access comes from (personal accounts), empty if direct.
    pub inherited_from: String,
    pub removable: bool,
    pub role_editable: bool,
}

/// Link settings ("Copy link" ⚙ in the Share dialog).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LinkOptions {
    /// "anyone" | "organization" | "existing" | "users"
    pub audience: String,
    /// "view" | "edit" | "review" | "blocksDownload"
    pub role: String,
    /// Unix seconds; 0 = never.
    #[serde(default)]
    pub expires: i64,
    #[serde(default)]
    pub password: String,
    /// For "users" (specific people).
    #[serde(default)]
    pub recipients: Vec<String>,
    #[serde(default)]
    pub notify: bool,
    /// OneDrive keeps one link per kind (audience + permission). If one with
    /// other settings exists, replace it (its current holders lose access)
    /// instead of returning it unchanged.
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LinkResult {
    pub url: String,
    pub created: bool,
    /// False if the expiry/password couldn't be applied (e.g. needs Microsoft 365).
    pub settings_applied: bool,
    pub permission_id: String,
    pub message: String,
    /// An existing link with other settings was returned unchanged (see
    /// [`LinkOptions::replace`]); `expires`/`has_password` describe it.
    #[serde(default)]
    pub existing: bool,
    #[serde(default)]
    pub expires: i64,
    #[serde(default)]
    pub has_password: bool,
}

/// "Send" in the Share dialog.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct InviteOptions {
    pub recipients: Vec<String>,
    /// "view" | "edit"
    pub role: String,
    #[serde(default)]
    pub message: String,
    #[serde(default = "yes")]
    pub notify: bool,
    #[serde(default)]
    pub expires: i64,
    #[serde(default)]
    pub password: String,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct InviteFailure {
    pub email: String,
    pub message: String,
    pub fix_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct InviteOutcome {
    pub granted: u32,
    pub failed: Vec<InviteFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct StopSharingOutcome {
    pub removed: u32,
    pub failed: Vec<String>,
}

/// A file deleted online whose downloaded copy is still kept (7 days).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemovedItem {
    pub id: String,
    pub name: String,
    /// Path relative to the OneDrive folder.
    pub rel_path: String,
    /// Unix seconds.
    pub removed_at: i64,
    pub size: u64,
}

/// A shortcut to a shared folder, or a SharePoint library, synced with an account.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SyncLocation {
    pub key: String,
    /// "shortcut" | "library"
    pub kind: String,
    pub title: String,
    /// Where it appears on this device.
    pub local_path: String,
    /// One of the [`crate::types::SyncState`] strings.
    pub state: String,
    pub status_text: String,
    pub web_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SiteInfo {
    pub id: String,
    pub name: String,
    pub web_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LibraryInfo {
    pub drive_id: String,
    pub name: String,
    pub web_url: String,
    /// Already synced on this device.
    pub synced: bool,
}

/// A folder that can be backed up to OneDrive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupFolder {
    pub key: String,
    pub name: String,
    pub local_path: String,
    pub backed_up: bool,
    pub in_progress: bool,
    pub files: u64,
    pub bytes: u64,
}

/// Folder row for the "Choose folders" dialog.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FolderEntry {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub excluded: bool,
}

/// Settings exposed to the GUI (a projection of [`crate::config::Config`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SettingsDoc {
    pub general: crate::config::General,
    pub network: crate::config::Network,
    pub notifications: crate::config::Notifications,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shapes() {
        let a = ActivityEntry {
            id: 1,
            time: 1_700_000_000,
            kind: crate::types::ActivityKind::Uploaded,
            name: "a.txt".into(),
            path: "/home/u/OneDrive/Docs/a.txt".into(),
            folder: "Docs".into(),
            size: 3,
            detail: String::new(),
            percent: -1,
        };
        let s = serde_json::to_string(&a).unwrap();
        assert!(s.contains("\"kind\":\"uploaded\""));
        let back: ActivityEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(back, a);
        assert_eq!(account_path("ab12"), "/com/airdataiot/LinuxOneDrive/account/ab12");
    }
}
