//! Update checks and downloads (the rules are in `od_core::update`).
//!
//! The service looks for a new release once a day (unless turned off) and
//! tells the user with a notification, once per version. It downloads a
//! package only when asked, verifies it against the release's `SHA256SUMS`
//! and hands back the path: installing needs administrator approval and is
//! done by the app or the command-line tool, never from here.

use crate::daemon::Daemon;
use crate::dbus_iface::ManagerIface;
use crate::notify::Action;
use anyhow::{Context, anyhow, bail};
use od_core::update::{self, UpdateInfo};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Largest package the updater will download.
const MAX_PACKAGE: u64 = 300 * 1024 * 1024;
/// A check this recent is reused instead of asking GitHub again.
const FRESH_FOR: i64 = 3600;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

async fn fetch_latest(d: &Daemon) -> anyhow::Result<serde_json::Value> {
    let resp = d
        .http
        .get(update::latest_release_url())
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|_| anyhow!("Couldn't reach GitHub. Check your internet connection."))?;
    match resp.status().as_u16() {
        200 => resp.json().await.context("GitHub's answer couldn't be read"),
        404 => bail!("No release has been published yet."),
        403 | 429 => bail!("GitHub is limiting requests right now. Try again later."),
        s => bail!("GitHub answered with an error ({s})."),
    }
}

/// Ask GitHub for the latest release and remember the answer.
pub async fn check(d: &Daemon) -> UpdateInfo {
    let info = match fetch_latest(d).await {
        Ok(v) => update::info_from_release(&v, od_core::VERSION, update::deb_arch(), now()),
        Err(e) => UpdateInfo {
            current: od_core::VERSION.to_string(),
            checked: now(),
            error: e.to_string(),
            ..Default::default()
        },
    };
    let changed = {
        let mut slot = d.update.lock().unwrap();
        let before = slot.as_ref().filter(|u| u.available).map(|u| u.latest.clone());
        let after = Some(&info).filter(|u| u.available).map(|u| u.latest.clone());
        // A failed check doesn't forget an update that was already found.
        if info.error.is_empty() || slot.is_none() {
            *slot = Some(info.clone());
        }
        info.error.is_empty() && before != after
    };
    if changed && let Ok(iface) = d.conn.object_server().interface::<_, ManagerIface>(od_core::dbus::MANAGER_PATH).await
    {
        let _ = iface.get().await.update_version_changed(iface.signal_emitter()).await;
    }
    info
}

/// The last answer if it is recent, otherwise a new check.
pub async fn cached_or_check(d: &Daemon, force: bool) -> UpdateInfo {
    if !force
        && let Some(u) = d.update.lock().unwrap().clone()
        && now() - u.checked < FRESH_FOR
    {
        return u;
    }
    check(d).await
}

fn sha256_file(p: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(format!("{:x}", h.finalize()))
}

async fn get_ok(d: &Daemon, url: &str) -> anyhow::Result<reqwest::Response> {
    let resp =
        d.http.get(url).send().await.map_err(|_| anyhow!("Couldn't reach GitHub. Check your internet connection."))?;
    if !resp.status().is_success() {
        bail!("The download failed (HTTP {}).", resp.status().as_u16());
    }
    Ok(resp)
}

/// Download the update's package and verify it. Returns where it is.
pub async fn download(d: &Daemon) -> anyhow::Result<PathBuf> {
    let info = cached_or_check(d, false).await;
    if !info.error.is_empty() {
        bail!("{}", info.error);
    }
    if !info.available {
        bail!("You already have the latest version ({}).", info.current);
    }
    if !info.installable {
        bail!("Version {} has no installable package for this system. See {}", info.latest, info.page_url);
    }
    if info.asset_size > MAX_PACKAGE {
        bail!("The update is unexpectedly large; it wasn't downloaded.");
    }
    let dir = od_core::paths::cache_dir().join("updates");
    od_core::paths::ensure_private_dir(&dir)?;
    let dest = dir.join(&info.asset_name);

    let sums = get_ok(d, &info.checksums_url).await?.text().await.context("reading the checksums")?;
    let want = update::checksum_for(&sums, &info.asset_name)
        .ok_or_else(|| anyhow!("The release's checksum file doesn't list this package; it wasn't downloaded."))?;

    // Keep only this package (older downloads are of no use any more).
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if e.path() != dest {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let d2 = dest.clone();
    if dest.exists() && tokio::task::spawn_blocking(move || sha256_file(&d2)).await?.ok().as_deref() == Some(&want) {
        return Ok(dest);
    }

    let part = dir.join(format!("{}.part", info.asset_name));
    let mut resp = get_ok(d, &info.asset_url).await?;
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&part)?;
        while let Some(chunk) = resp.chunk().await.map_err(|_| anyhow!("The download was interrupted."))? {
            written += chunk.len() as u64;
            if written > MAX_PACKAGE {
                drop(f);
                let _ = std::fs::remove_file(&part);
                bail!("The update is unexpectedly large; it was discarded.");
            }
            hasher.update(&chunk);
            f.write_all(&chunk)?;
        }
        f.sync_all()?;
    }
    let got = format!("{:x}", hasher.finalize());
    if got != want {
        let _ = std::fs::remove_file(&part);
        bail!("The download didn't match the release's checksum, so it was discarded. Try again later.");
    }
    std::fs::rename(&part, &dest)?;
    tracing::info!(version = %info.latest, file = %dest.display(), "update downloaded and verified");
    Ok(dest)
}

fn notified_file() -> PathBuf {
    od_core::paths::state_dir().join("update-notified")
}

/// Daily check; one notification per new version.
pub async fn watch(d: Arc<Daemon>) {
    let mut wait = Duration::from_secs(120);
    loop {
        tokio::select! {
            _ = d.shutdown.cancelled() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        wait = Duration::from_secs(24 * 3600);
        if !d.cfg().general.check_updates {
            continue;
        }
        let info = check(&d).await;
        if !info.error.is_empty() {
            tracing::debug!("update check: {}", info.error);
            // Offline at start-up is common: try again sooner than tomorrow.
            wait = Duration::from_secs(3600);
            continue;
        }
        if !info.available {
            continue;
        }
        let already = std::fs::read_to_string(notified_file()).unwrap_or_default();
        if already.trim() == info.latest {
            continue;
        }
        tracing::info!(version = %info.latest, "a new version is available");
        if let Some(dir) = notified_file().parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(notified_file(), &info.latest);
        d.notifier
            .show(
                Some("update"),
                &format!("{} {} is available", od_core::APP_NAME, info.latest),
                &format!("You have version {}. Open to see what's new and install it.", info.current),
                vec![("default", "Update…", Action::Show("update".into()))],
                false,
            )
            .await;
    }
}
