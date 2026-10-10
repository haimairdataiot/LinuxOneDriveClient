//! App updates from the project's GitHub releases.
//!
//! The service asks GitHub for the latest release, compares its version with
//! the running one and, when the user chooses to update, downloads the
//! package for this system and checks it against the release's `SHA256SUMS`
//! file. Installing is a separate, explicit step that needs the user's
//! administrator approval; nothing is ever installed silently.

use serde::{Deserialize, Serialize};

/// GitHub repository whose releases the app updates from.
pub const UPDATE_REPO: &str = "haimairdataiot/LinuxOneDriveClient";

/// Name of the checksum file published with every release.
pub const CHECKSUMS_ASSET: &str = "SHA256SUMS";

/// Environment variable that points the update check at another API base
/// (used by tests; a real installation never sets it).
pub const UPDATE_API_ENV: &str = "LINUX_ONEDRIVE_UPDATE_API";

/// What the app knows about updates. Sent to the GUI and CLI as JSON.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UpdateInfo {
    /// Version that is running.
    pub current: String,
    /// Newest released version (without a leading "v"); empty if unknown.
    pub latest: String,
    /// `latest` is newer than `current`.
    pub available: bool,
    /// A package for this system is published with a checksum, so the app
    /// can download and install it.
    pub installable: bool,
    /// Release notes, as written for the release (Markdown).
    pub notes: String,
    /// The release's page on GitHub.
    pub page_url: String,
    pub asset_name: String,
    pub asset_url: String,
    pub asset_size: u64,
    pub checksums_url: String,
    /// Unix seconds; 0 if unknown.
    pub published: i64,
    /// When this information was fetched (Unix seconds).
    pub checked: i64,
    /// Why the check failed (offline, no releases yet…); empty on success.
    pub error: String,
}

/// Address of the "latest release" API call.
pub fn latest_release_url() -> String {
    let base = std::env::var(UPDATE_API_ENV).unwrap_or_else(|_| "https://api.github.com".to_string());
    format!("{}/repos/{UPDATE_REPO}/releases/latest", base.trim_end_matches('/'))
}

/// The update check was redirected to a test server.
pub fn api_is_overridden() -> bool {
    std::env::var_os(UPDATE_API_ENV).is_some()
}

/// Only packages published in the project's own releases are accepted.
pub fn is_trusted_asset_url(url: &str) -> bool {
    api_is_overridden() || url.starts_with(&format!("https://github.com/{UPDATE_REPO}/releases/download/"))
}

/// Debian architecture name of this build.
pub fn deb_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "armhf",
        "x86" => "i386",
        "riscv64" => "riscv64",
        other => other,
    }
}

/// "v1.2.3", "1.2.3" or "1.2.3-4" as comparable numbers (major, minor,
/// patch, package revision). Anything else is not a release version.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let (core, rev) = match s.split_once('-') {
        Some((c, r)) => (c, r.parse::<u64>().ok()?),
        None => (s, 0),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch, rev))
}

/// Is `candidate` a newer version than `current`? Unparseable versions are
/// never newer.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

/// Read GitHub's "latest release" answer.
pub fn info_from_release(release: &serde_json::Value, current: &str, arch: &str, now: i64) -> UpdateInfo {
    let text = |k: &str| release[k].as_str().unwrap_or_default().to_string();
    let mut info = UpdateInfo { current: current.to_string(), checked: now, ..Default::default() };
    let tag = text("tag_name");
    if parse_version(&tag).is_none() {
        info.error = "The latest release has no version number.".into();
        return info;
    }
    if release["draft"].as_bool() == Some(true) || release["prerelease"].as_bool() == Some(true) {
        info.error = "The latest release isn't a final release.".into();
        return info;
    }
    info.latest = tag.trim_start_matches(['v', 'V']).to_string();
    info.available = is_newer(&tag, current);
    info.notes = text("body");
    info.page_url = text("html_url");
    info.published =
        chrono::DateTime::parse_from_rfc3339(&text("published_at")).map(|t| t.timestamp()).unwrap_or_default();
    let suffix = format!("_{arch}.deb");
    for a in release["assets"].as_array().into_iter().flatten() {
        let name = a["name"].as_str().unwrap_or_default();
        let url = a["browser_download_url"].as_str().unwrap_or_default();
        if name == CHECKSUMS_ASSET {
            info.checksums_url = url.to_string();
        } else if name.ends_with(&suffix) && info.asset_name.is_empty() {
            info.asset_name = name.to_string();
            info.asset_url = url.to_string();
            info.asset_size = a["size"].as_u64().unwrap_or_default();
        }
    }
    info.installable = !info.asset_url.is_empty()
        && !info.checksums_url.is_empty()
        && is_trusted_asset_url(&info.asset_url)
        && is_trusted_asset_url(&info.checksums_url)
        // A file name, never a path.
        && !info.asset_name.contains(['/', '\\'])
        && !info.asset_name.starts_with('.');
    info
}

/// The SHA-256 (lowercase hex) that a `SHA256SUMS` file lists for `name`.
pub fn checksum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        // "<hash>  <name>" or "<hash> *<name>" (binary mode).
        let file = parts.next()?.trim_start_matches('*');
        let file = file.rsplit('/').next().unwrap_or(file);
        (file == name && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn versions_compare_by_number() {
        assert_eq!(parse_version("v0.2.0"), Some((0, 2, 0, 0)));
        assert_eq!(parse_version("1.10.3-2"), Some((1, 10, 3, 2)));
        assert_eq!(parse_version("2"), Some((2, 0, 0, 0)));
        assert_eq!(parse_version("nightly"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("1.2.3-beta"), None);
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("0.2.0-2", "0.2.0"));
        assert!(!is_newer("v0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("garbage", "0.2.0"));
    }

    fn release(tag: &str) -> serde_json::Value {
        let dl = format!("https://github.com/{UPDATE_REPO}/releases/download/{tag}");
        json!({
            "tag_name": tag,
            "body": "- Faster\n- Safer",
            "html_url": format!("https://github.com/{UPDATE_REPO}/releases/tag/{tag}"),
            "published_at": "2026-10-10T12:00:00Z",
            "draft": false,
            "prerelease": false,
            "assets": [
                { "name": "linux-onedrive_9.9.9-1_arm64.deb", "browser_download_url": format!("{dl}/linux-onedrive_9.9.9-1_arm64.deb"), "size": 11 },
                { "name": "linux-onedrive_9.9.9-1_amd64.deb", "browser_download_url": format!("{dl}/linux-onedrive_9.9.9-1_amd64.deb"), "size": 22 },
                { "name": "SHA256SUMS", "browser_download_url": format!("{dl}/SHA256SUMS"), "size": 200 },
            ],
        })
    }

    #[test]
    fn picks_the_package_for_this_system() {
        let info = info_from_release(&release("v9.9.9"), "0.2.0", "amd64", 1000);
        assert!(info.available && info.installable);
        assert_eq!(info.latest, "9.9.9");
        assert_eq!(info.asset_name, "linux-onedrive_9.9.9-1_amd64.deb");
        assert_eq!(info.asset_size, 22);
        assert!(info.checksums_url.ends_with("/SHA256SUMS"));
        assert_eq!(info.notes, "- Faster\n- Safer");
        assert!(info.published > 0);

        // Same version: nothing to do.
        let same = info_from_release(&release("v0.2.0"), "0.2.0", "amd64", 1000);
        assert!(!same.available);

        // No package for this architecture: tell, but can't install.
        let other = info_from_release(&release("v9.9.9"), "0.2.0", "riscv64", 1000);
        assert!(other.available && !other.installable);
    }

    #[test]
    fn refuses_untrusted_or_incomplete_releases() {
        // No checksum file.
        let mut r = release("v9.9.9");
        r["assets"].as_array_mut().unwrap().pop();
        assert!(!info_from_release(&r, "0.2.0", "amd64", 0).installable);

        // Package hosted somewhere else.
        let mut r = release("v9.9.9");
        r["assets"][1]["browser_download_url"] = json!("https://example.com/linux-onedrive_9.9.9-1_amd64.deb");
        assert!(!info_from_release(&r, "0.2.0", "amd64", 0).installable);

        // Pre-releases and drafts are not offered.
        let mut r = release("v9.9.9");
        r["prerelease"] = json!(true);
        let info = info_from_release(&r, "0.2.0", "amd64", 0);
        assert!(!info.available && !info.error.is_empty());

        // A tag that isn't a version.
        let info = info_from_release(&release("latest"), "0.2.0", "amd64", 0);
        assert!(!info.available && !info.error.is_empty());
    }

    #[test]
    fn finds_checksums() {
        let h = "a".repeat(64);
        let sums =
            format!("{h}  linux-onedrive_1.0.0-1_amd64.deb\nBBBB  other.deb\n{}  *dist/second.deb\n", "C".repeat(64));
        assert_eq!(checksum_for(&sums, "linux-onedrive_1.0.0-1_amd64.deb"), Some(h));
        assert_eq!(checksum_for(&sums, "second.deb"), Some("c".repeat(64)));
        assert_eq!(checksum_for(&sums, "other.deb"), None, "not a SHA-256");
        assert_eq!(checksum_for(&sums, "missing.deb"), None);
    }
}
