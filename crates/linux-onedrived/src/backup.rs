//! Folder backup: the Linux equivalent of the official client's
//! "Manage back up" (Known Folder Move).
//!
//! Start: every file of e.g. ~/Documents is copied into ~/OneDrive/Documents
//! (through the mount, so it uploads), verified (size + QuickXorHash of the
//! copy) and only then removed from the original folder. The emptied folder
//! is replaced by a symlink to the OneDrive copy, the XDG user-dirs entry is
//! pointed at it, and the folder is kept on this device ("Always keep").
//! Items that can't be moved (symlinks, sockets…) are left in
//! "~/Documents (not backed up)" rather than lost.
//!
//! Stop: the symlink is replaced by a real folder again and the files are
//! copied back; the OneDrive copy stays online.

use std::path::{Path, PathBuf};

/// (XDG key, folder name in OneDrive, default directory name)
pub const FOLDERS: &[(&str, &str)] = &[
    ("DESKTOP", "Desktop"),
    ("DOCUMENTS", "Documents"),
    ("PICTURES", "Pictures"),
    ("MUSIC", "Music"),
    ("VIDEOS", "Videos"),
];

pub fn folder_name(key: &str) -> Option<&'static str> {
    FOLDERS.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, n)| *n)
}

fn user_dirs_file() -> PathBuf {
    od_core::paths::autostart_dir().parent().map(|p| p.join("user-dirs.dirs")).unwrap_or_default()
}

/// Current location of an XDG user dir (resolved, without following a symlink).
pub fn xdg_dir(key: &str) -> PathBuf {
    let home = od_core::paths::home_dir();
    let text = std::fs::read_to_string(user_dirs_file()).unwrap_or_default();
    let var = format!("XDG_{key}_DIR=");
    for line in text.lines() {
        if let Some(v) = line.trim().strip_prefix(&var) {
            let v = v.trim().trim_matches('"');
            let p = v.replace("$HOME", &home.to_string_lossy());
            return PathBuf::from(p);
        }
    }
    home.join(folder_name(key).unwrap_or(key))
}

/// Point an XDG user dir somewhere else (rewrites user-dirs.dirs in place).
pub fn set_xdg_dir(key: &str, path: &Path) -> std::io::Result<()> {
    let file = user_dirs_file();
    let home = od_core::paths::home_dir();
    let shown = match path.strip_prefix(&home) {
        Ok(rel) => format!("$HOME/{}", rel.display()),
        Err(_) => path.display().to_string(),
    };
    let var = format!("XDG_{key}_DIR=");
    let line = format!("{var}\"{shown}\"");
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let mut found = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|l| {
            if l.trim().starts_with(&var) {
                found = true;
                line.clone()
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        out.push(line);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, out.join("\n") + "\n")?;
    std::fs::rename(tmp, file)
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Report {
    pub moved: u64,
    pub bytes: u64,
    pub skipped: Vec<String>,
}

fn hash_file(p: &Path) -> std::io::Result<String> {
    od_core::quickxor::hash_file(p)
}

/// A name not yet used in `dir` (conflict-style suffix).
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let host = od_core::hostname();
    (1..).map(|n| dir.join(od_core::names::conflict_name(name, &host, n))).find(|p| !p.exists()).expect("free name")
}

/// Move the contents of `src` into `dst`, verifying each copy before the
/// original is removed. Returns what was moved and what was left behind.
pub fn move_tree(src: &Path, dst: &Path, report: &mut Report) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let ft = entry.file_type()?;
        let from = entry.path();
        if ft.is_dir() {
            let to = dst.join(&name);
            if to.exists() && !to.is_dir() {
                report.skipped.push(from.display().to_string());
                continue;
            }
            move_tree(&from, &to, report)?;
            // Remove only if everything inside was moved.
            let _ = std::fs::remove_dir(&from);
        } else if ft.is_file() {
            let to = free_name(dst, &name);
            let meta = entry.metadata()?;
            match std::fs::copy(&from, &to) {
                Ok(n) if n == meta.len() => {}
                Ok(_) | Err(_) => {
                    let _ = std::fs::remove_file(&to);
                    report.skipped.push(from.display().to_string());
                    continue;
                }
            }
            if let Ok(m) = meta.modified() {
                let _ = std::fs::File::options().write(true).open(&to).and_then(|f| f.set_modified(m));
            }
            // Verify the copy before deleting the original.
            if hash_file(&from)? != hash_file(&to)? {
                let _ = std::fs::remove_file(&to);
                report.skipped.push(from.display().to_string());
                continue;
            }
            std::fs::remove_file(&from)?;
            report.moved += 1;
            report.bytes += meta.len();
        } else {
            // Symlinks, sockets, devices: OneDrive can't hold them; leave them.
            report.skipped.push(from.display().to_string());
        }
    }
    Ok(())
}

/// Copy (not move) `src` into `dst` (used when stopping a backup).
pub fn copy_tree(src: &Path, dst: &Path, report: &mut Report) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_tree(&entry.path(), &dst.join(&name), report)?;
        } else if ft.is_file() {
            let to = free_name(dst, &name);
            match std::fs::copy(entry.path(), &to) {
                Ok(n) => {
                    report.moved += 1;
                    report.bytes += n;
                    if let Ok(m) = entry.metadata().and_then(|m| m.modified()) {
                        let _ = std::fs::File::options().write(true).open(&to).and_then(|f| f.set_modified(m));
                    }
                }
                Err(_) => report.skipped.push(entry.path().display().to_string()),
            }
        }
    }
    Ok(())
}

pub fn measure(p: &Path) -> (u64, u64) {
    let mut files = 0;
    let mut bytes = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => {
                    files += 1;
                    bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                }
                _ => {}
            }
            if files > 200_000 {
                return (files, bytes);
            }
        }
    }
    (files, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_verifies_and_leaves_unmovable_items() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("Documents");
        let dst = d.path().join("OneDrive/Documents");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"alpha").unwrap();
        std::fs::write(src.join("sub/b.txt"), b"beta").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", src.join("link")).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("a.txt"), b"already there").unwrap();
        let mut r = Report::default();
        move_tree(&src, &dst, &mut r).unwrap();
        assert_eq!(r.moved, 2);
        assert_eq!(std::fs::read(dst.join("sub/b.txt")).unwrap(), b"beta");
        // Name clash: both kept.
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"already there");
        let host = od_core::hostname();
        assert_eq!(std::fs::read(dst.join(format!("a-{host}.txt"))).unwrap(), b"alpha");
        assert!(!src.join("a.txt").exists() && !src.join("sub").exists());
        assert_eq!(r.skipped.len(), 1, "the symlink stays behind");
    }

    #[test]
    fn user_dirs_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        // SAFETY (test only): single-threaded test sets its own config dir.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", d.path()) };
        std::fs::write(
            d.path().join("user-dirs.dirs"),
            "XDG_DOCUMENTS_DIR=\"$HOME/Documents\"\nXDG_MUSIC_DIR=\"$HOME/Music\"\n",
        )
        .unwrap();
        let home = od_core::paths::home_dir();
        assert_eq!(xdg_dir("DOCUMENTS"), home.join("Documents"));
        set_xdg_dir("DOCUMENTS", &home.join("OneDrive/Documents")).unwrap();
        let text = std::fs::read_to_string(d.path().join("user-dirs.dirs")).unwrap();
        assert!(text.contains("XDG_DOCUMENTS_DIR=\"$HOME/OneDrive/Documents\""));
        assert!(text.contains("XDG_MUSIC_DIR=\"$HOME/Music\""));
        assert_eq!(xdg_dir("DOCUMENTS"), home.join("OneDrive/Documents"));
    }
}
