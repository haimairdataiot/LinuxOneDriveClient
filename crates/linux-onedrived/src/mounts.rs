//! Mount-point hygiene and systemd readiness.
//!
//! * stale FUSE mounts ("Transport endpoint is not connected", left behind
//!   by a crash because AppArmor blocks fusermount3's auto_unmount on
//!   Ubuntu) are lazily unmounted before mounting;
//! * we never mount over a non-empty directory: anything written there
//!   while the daemon was down is moved to an import folder and copied into
//!   OneDrive after mounting (research doc 04 §4.7).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Is `path` currently a mount point (according to /proc/self/mountinfo)?
pub fn is_mounted(path: &Path) -> bool {
    let target = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => path.to_path_buf(),
    };
    mount_entries().iter().any(|(mp, _)| *mp == target || Path::new(mp) == path)
}

/// Is `path` mounted by us (fstype fuse.onedrive)?
pub fn is_our_mount(path: &Path) -> bool {
    mount_entries().iter().any(|(mp, fstype)| (mp == path) && fstype == "fuse.onedrive")
}

fn mount_entries() -> Vec<(PathBuf, String)> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            // id parent maj:min root mountpoint opts ... - fstype source superopts
            let mut parts = l.split(" - ");
            let left: Vec<&str> = parts.next()?.split(' ').collect();
            let right: Vec<&str> = parts.next()?.split(' ').collect();
            Some((PathBuf::from(unescape(left.get(4)?)), right.first()?.to_string()))
        })
        .collect()
}

fn unescape(s: &str) -> String {
    // mountinfo escapes space, tab, newline and backslash as \ooo (octal).
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 4 <= b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Lazily unmount (works on dead "ENOTCONN" mounts too).
pub fn force_unmount(path: &Path) {
    let _ = Command::new("fusermount3").arg("-uz").arg(path).status();
}

/// Unmount a FUSE session without getting stuck. A folder in use (an open
/// file-manager window, a shell inside it) can't be unmounted normally, and
/// the session would then never end: detach it instead, so it disappears
/// now and is released when its last user lets go.
pub async fn unmount_session(session: fuser::BackgroundSession, mp: std::path::PathBuf) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(session.umount_and_join().is_ok());
    });
    match tokio::time::timeout(std::time::Duration::from_secs(3), rx).await {
        Ok(Ok(true)) => {}
        _ => {
            tracing::info!(mount = %mp.display(), "the folder is in use; detaching it");
            force_unmount(&mp);
        }
    }
}

/// Get `mp` ready to be mounted. Returns the import folder if stray content was moved aside.
pub fn prepare(mp: &Path, import_root: &Path) -> std::io::Result<Option<PathBuf>> {
    if is_mounted(mp) || std::fs::metadata(mp).err().is_some_and(|e| e.raw_os_error() == Some(libc::ENOTCONN)) {
        tracing::warn!(path = %mp.display(), "cleaning up a stale mount");
        force_unmount(mp);
    }
    if !mp.exists() {
        std::fs::create_dir_all(mp)?;
        return Ok(None);
    }
    let meta = std::fs::symlink_metadata(mp)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(std::io::Error::other(format!("{} is not a folder", mp.display())));
    }
    let entries: Vec<_> = std::fs::read_dir(mp)?.flatten().collect();
    if entries.is_empty() {
        return Ok(None);
    }
    let dest = import_root.join(format!("import-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S")));
    std::fs::create_dir_all(&dest)?;
    for e in entries {
        let target = dest.join(e.file_name());
        if let Err(err) = std::fs::rename(e.path(), &target) {
            tracing::error!("cannot move {} aside: {err}", e.path().display());
            return Err(err);
        }
    }
    tracing::warn!(from = %mp.display(), to = %dest.display(), "the OneDrive folder was not empty; its contents will be added to OneDrive");
    Ok(Some(dest))
}

/// Copy previously moved-aside files into the mounted OneDrive (they get
/// uploaded like any new file), then delete the import folder.
pub fn import_into(src: &Path, mount: &Path) -> std::io::Result<usize> {
    let mut n = 0;
    copy_tree(src, mount, &mut n)?;
    std::fs::remove_dir_all(src)?;
    Ok(n)
}

fn copy_tree(src: &Path, dst: &Path, n: &mut usize) -> std::io::Result<()> {
    for e in std::fs::read_dir(src)?.flatten() {
        let name = e.file_name();
        let mut target = dst.join(&name);
        if target.exists() {
            let s = name.to_string_lossy();
            let alt = od_core::names::conflict_name(&s, &od_core::hostname(), 1);
            target = dst.join(alt);
        }
        let ft = e.file_type()?;
        if ft.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_tree(&e.path(), &target, n)?;
        } else if ft.is_file() {
            std::fs::copy(e.path(), &target)?;
            *n += 1;
        }
    }
    Ok(())
}

/// Tell systemd we're ready (Type=notify), or send other states.
pub fn sd_notify(state: &str) {
    let Some(sock) = std::env::var_os("NOTIFY_SOCKET") else { return };
    let path = sock.to_string_lossy().into_owned();
    let Ok(s) = std::os::unix::net::UnixDatagram::unbound() else { return };
    let r = if let Some(name) = path.strip_prefix('@') {
        use std::os::linux::net::SocketAddrExt;
        std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
            .and_then(|addr| s.send_to_addr(state.as_bytes(), &addr))
    } else {
        s.send_to(state.as_bytes(), &path)
    };
    if let Err(e) = r {
        tracing::debug!("sd_notify failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_unescape() {
        assert_eq!(unescape("/home/u/OneDrive\\040-\\040Contoso"), "/home/u/OneDrive - Contoso");
        assert_eq!(unescape("/plain"), "/plain");
    }

    #[test]
    fn prepare_moves_stray_files() {
        let d = tempfile::tempdir().unwrap();
        let mp = d.path().join("OneDrive");
        let imp = d.path().join("data");
        assert!(prepare(&mp, &imp).unwrap().is_none());
        assert!(mp.is_dir());
        std::fs::write(mp.join("stray.txt"), b"x").unwrap();
        let moved = prepare(&mp, &imp).unwrap().unwrap();
        assert!(moved.join("stray.txt").exists());
        assert_eq!(std::fs::read_dir(&mp).unwrap().count(), 0);
        // Importing copies into the (here: plain) target and removes the import dir.
        assert_eq!(import_into(&moved, &mp).unwrap(), 1);
        assert!(mp.join("stray.txt").exists() && !moved.exists());
        assert!(!is_mounted(&mp));
    }
}
