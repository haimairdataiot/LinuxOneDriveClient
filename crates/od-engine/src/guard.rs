//! Identify the process behind a FUSE request, to stop thumbnailers and
//! desktop-search indexers from downloading every online-only file
//! (GIO treats our mount as local; research doc 03 §5.7, §6.5, §6.6).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `comm` values (15 chars max) of background readers that must not trigger downloads.
const BACKGROUND_COMMS: &[&str] = &[
    // GNOME thumbnailers (run sandboxed by gnome-desktop's bwrap script)
    "glycin-thumbnai",
    "glycin-image-rs",
    "glycin-heif",
    "glycin-svg",
    "glycin-jxl",
    "gst-video-thumb",
    "gst-audio-thumb",
    "papers-thumbnai",
    "gdk-pixbuf-thum",
    "gnome-thumbnail",
    "heif-thumbnaile",
    "totem-video-thu",
    "evince-thumbnai",
    "ffmpegthumbnail",
    "tumblerd",
    // Desktop search indexers
    "localsearch-ext",
    "localsearch-3",
    "tracker-extract",
    "tracker-miner-f",
    "baloo_file_extr",
    "baloo_file",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// A normal application (open → download).
    Interactive,
    /// A thumbnailer or indexer (never downloads); carries its name for logs.
    Background(String),
    /// The kernel itself (pid 0) or a process that already exited.
    Unknown,
}

#[derive(Default)]
pub struct Guard {
    cache: Mutex<HashMap<u32, (Caller, Instant)>>,
}

impl Guard {
    pub fn classify(&self, pid: u32) -> Caller {
        if pid == 0 {
            return Caller::Unknown;
        }
        let tgid = tgid_of(pid).unwrap_or(pid);
        {
            let c = self.cache.lock().unwrap();
            if let Some((v, at)) = c.get(&tgid)
                && at.elapsed() < Duration::from_secs(30)
            {
                return v.clone();
            }
        }
        let v = classify_tgid(tgid);
        let mut c = self.cache.lock().unwrap();
        if c.len() > 4096 {
            c.retain(|_, (_, at)| at.elapsed() < Duration::from_secs(30));
        }
        c.insert(tgid, (v.clone(), Instant::now()));
        v
    }
}

fn tgid_of(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status.lines().find_map(|l| l.strip_prefix("Tgid:")).and_then(|v| v.trim().parse().ok())
}

fn classify_tgid(tgid: u32) -> Caller {
    let comm = match std::fs::read_to_string(format!("/proc/{tgid}/comm")) {
        Ok(c) => c.trim().to_string(),
        Err(_) => return Caller::Unknown,
    };
    if is_background_name(&comm) {
        return Caller::Background(comm);
    }
    if let Ok(exe) = std::fs::read_link(format!("/proc/{tgid}/exe")) {
        let s = exe.to_string_lossy();
        let base = s.rsplit('/').next().unwrap_or("");
        if s.contains("/glycin-loaders/") || base.contains("thumbnail") || is_background_name(base) {
            return Caller::Background(base.to_string());
        }
    }
    Caller::Interactive
}

pub fn is_background_name(name: &str) -> bool {
    BACKGROUND_COMMS.iter().any(|b| name == *b || (b.len() >= 15 && name.starts_with(b)))
        || name.ends_with("-thumbnailer")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(is_background_name("glycin-thumbnai"));
        assert!(is_background_name("localsearch-ext"));
        assert!(is_background_name("evince-thumbnailer"));
        assert!(!is_background_name("nautilus"));
        assert!(!is_background_name("libreoffice"));
    }

    #[test]
    fn classifies_self_as_interactive() {
        let g = Guard::default();
        assert_eq!(g.classify(std::process::id()), Caller::Interactive);
        assert_eq!(g.classify(0), Caller::Unknown);
    }
}
