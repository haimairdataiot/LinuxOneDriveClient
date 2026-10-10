//! OneDrive naming rules, case folding, ignore patterns and conflict names.
//!
//! Linux accepts almost any byte sequence as a file name; OneDrive does not.
//! Names that OneDrive rejects are still allowed inside the mount, but they
//! are kept local-only and reported as sync issues, as the official client
//! does. Ignored patterns (editor lock files, trash folders, partial
//! downloads) are local-only by design and never reported.

use crate::types::DriveType;

/// Characters OneDrive and SharePoint reject in file and folder names.
pub const INVALID_CHARS: &[char] = &['"', '*', ':', '<', '>', '?', '/', '\\', '|'];

/// Whole names reserved by OneDrive/SharePoint (compared case-insensitively;
/// the device names are also reserved with any extension, e.g. `CON.txt`).
const RESERVED_DEVICE_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com0", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt0",
    "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];
const RESERVED_NAMES: &[&str] = &[".lock", "desktop.ini"];

/// Maximum length of one path component, in UTF-16 code units.
pub const MAX_NAME_LEN: usize = 255;
/// Maximum length of the full decoded path relative to the drive root.
pub const MAX_PATH_LEN: usize = 400;
/// Largest file OneDrive accepts (250 GB).
pub const MAX_FILE_SIZE: u64 = 250 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameIssue {
    #[error("The name is empty")]
    Empty,
    #[error("The name contains a character OneDrive doesn't allow ({0})")]
    InvalidChar(char),
    #[error("\"{0}\" is a reserved name in OneDrive")]
    Reserved(String),
    #[error("Names can't start with \"~$\" in OneDrive")]
    TildeDollar,
    #[error("Names can't contain \"_vti_\" in OneDrive")]
    Vti,
    #[error("Names can't begin or end with a space")]
    EdgeSpace,
    #[error("Names can't end with a period")]
    TrailingPeriod,
    #[error("The name is longer than {MAX_NAME_LEN} characters")]
    TooLong,
    #[error("The full path is longer than {MAX_PATH_LEN} characters")]
    PathTooLong,
    #[error("The name contains control characters")]
    ControlChar,
}

impl NameIssue {
    /// Stable error code stored in the database and exposed over D-Bus.
    pub fn code(&self) -> &'static str {
        match self {
            NameIssue::Empty => "name-empty",
            NameIssue::InvalidChar(_) => "name-invalid-char",
            NameIssue::Reserved(_) => "name-reserved",
            NameIssue::TildeDollar => "name-tilde-dollar",
            NameIssue::Vti => "name-vti",
            NameIssue::EdgeSpace => "name-edge-space",
            NameIssue::TrailingPeriod => "name-trailing-period",
            NameIssue::TooLong => "name-too-long",
            NameIssue::PathTooLong => "path-too-long",
            NameIssue::ControlChar => "name-control-char",
        }
    }
}

/// Check one path component against OneDrive's rules.
pub fn validate_name(name: &str, drive: DriveType) -> Result<(), NameIssue> {
    if name.is_empty() {
        return Err(NameIssue::Empty);
    }
    if let Some(c) = name.chars().find(|c| INVALID_CHARS.contains(c)) {
        return Err(NameIssue::InvalidChar(c));
    }
    if name.chars().any(|c| (c as u32) < 0x20 || c == '\u{7f}') {
        return Err(NameIssue::ControlChar);
    }
    if name.starts_with(' ') || name.ends_with(' ') {
        return Err(NameIssue::EdgeSpace);
    }
    if name.ends_with('.') {
        return Err(NameIssue::TrailingPeriod);
    }
    let lower = name.to_lowercase();
    if RESERVED_NAMES.contains(&lower.as_str()) {
        return Err(NameIssue::Reserved(name.to_string()));
    }
    let stem = lower.split('.').next().unwrap_or("");
    if RESERVED_DEVICE_NAMES.contains(&stem) {
        return Err(NameIssue::Reserved(name.to_string()));
    }
    if name.starts_with("~$") {
        return Err(NameIssue::TildeDollar);
    }
    if drive != DriveType::Personal && lower.contains("_vti_") {
        return Err(NameIssue::Vti);
    }
    if name.encode_utf16().count() > MAX_NAME_LEN {
        return Err(NameIssue::TooLong);
    }
    Ok(())
}

/// Check a full path (relative to the drive root, `/`-separated).
pub fn validate_path_len(rel_path: &str) -> Result<(), NameIssue> {
    if rel_path.trim_start_matches('/').encode_utf16().count() > MAX_PATH_LEN {
        Err(NameIssue::PathTooLong)
    } else {
        Ok(())
    }
}

/// Case-fold a name for OneDrive's case-insensitive uniqueness check.
pub fn fold(name: &str) -> String {
    name.to_lowercase()
}

/// Built-in patterns for files that are never uploaded.
pub const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    ".Trash-*",         // per-mount trash created by GIO/Nautilus
    ".~lock.*#",        // LibreOffice lock files
    ".goutputstream-*", // GIO atomic-save temporaries
    "*.swp",            // vim swap files
    "*.swx",
    "*.swo",
    ".*.kate-swp", // Kate swap files
    "*.part",      // in-progress downloads (Firefox, wget)
    "*.partial",
    "*.crdownload",  // Chromium in-progress downloads
    ".directory",    // KDE Dolphin folder settings
    ".fuse_hidden*", // kernel FUSE silly-renames
    ".nfs*",
    "Thumbs.db",
    ".DS_Store",
    "*.tmp",
];

/// A compiled set of ignore patterns (simple globs: `*`, `?`, literal text).
#[derive(Debug, Clone)]
pub struct IgnoreSet {
    patterns: Vec<String>,
}

impl Default for IgnoreSet {
    fn default() -> Self {
        Self::new(std::iter::empty::<&str>())
    }
}

impl IgnoreSet {
    /// Default patterns plus user-supplied ones.
    pub fn new<I, S>(extra: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut patterns: Vec<String> = DEFAULT_IGNORE_PATTERNS.iter().map(|s| s.to_string()).collect();
        for p in extra {
            let p = p.as_ref().trim();
            if !p.is_empty() && !patterns.iter().any(|q| q == p) {
                patterns.push(p.to_string());
            }
        }
        Self { patterns }
    }

    pub fn is_ignored(&self, name: &str) -> bool {
        self.patterns.iter().any(|p| glob_match(p, name))
    }

    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }
}

/// Case-sensitive glob match supporting `*` (any run) and `?` (one char).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Name for the local copy kept when both sides changed:
/// `report.docx` → `report-HOST.docx`, then `report-HOST-2.docx`, …
pub fn conflict_name(name: &str, host: &str, attempt: u32) -> String {
    let (stem, ext) = split_ext(name);
    let suffix = if attempt <= 1 { format!("-{host}") } else { format!("-{host}-{attempt}") };
    format!("{stem}{suffix}{ext}")
}

/// Split `archive.tar.gz` → (`archive.tar`, `.gz`); dotfiles keep their name as stem.
pub fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(0) | None => (name, ""),
        Some(i) => (&name[..i], &name[i..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_names() {
        let p = DriveType::Personal;
        assert!(validate_name("report.docx", p).is_ok());
        assert!(validate_name("Ünïcødé 🎉.txt", p).is_ok());
        assert_eq!(validate_name("a:b", p), Err(NameIssue::InvalidChar(':')));
        assert_eq!(validate_name("what?", p), Err(NameIssue::InvalidChar('?')));
        assert!(matches!(validate_name("CON", p), Err(NameIssue::Reserved(_))));
        assert!(matches!(validate_name("com1.txt", p), Err(NameIssue::Reserved(_))));
        assert!(validate_name("console.txt", p).is_ok());
        assert!(matches!(validate_name("desktop.ini", p), Err(NameIssue::Reserved(_))));
        assert_eq!(validate_name("~$doc.docx", p), Err(NameIssue::TildeDollar));
        assert_eq!(validate_name("name.", p), Err(NameIssue::TrailingPeriod));
        assert_eq!(validate_name(" lead", p), Err(NameIssue::EdgeSpace));
        assert_eq!(validate_name("trail ", p), Err(NameIssue::EdgeSpace));
        assert_eq!(validate_name("x_vti_y", DriveType::Business), Err(NameIssue::Vti));
        assert!(validate_name("x_vti_y", p).is_ok());
        assert_eq!(validate_name(&"a".repeat(256), p), Err(NameIssue::TooLong));
        assert_eq!(validate_name("tab\there", p), Err(NameIssue::ControlChar));
    }

    #[test]
    fn globbing() {
        assert!(glob_match("*.swp", ".notes.txt.swp"));
        assert!(glob_match(".~lock.*#", ".~lock.budget.ods#"));
        assert!(!glob_match(".~lock.*#", ".~lock.budget.ods"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("*", ""));
        assert!(glob_match("*a*b*", "xxaYYbzz"));
        assert!(!glob_match("*a*b", "xxaYYbzz"));
        let set = IgnoreSet::new(["*.bak"]);
        assert!(set.is_ignored(".Trash-1000"));
        assert!(set.is_ignored("old.bak"));
        assert!(set.is_ignored(".goutputstream-ABC123"));
        assert!(!set.is_ignored("notes.txt"));
    }

    #[test]
    fn conflict_names() {
        assert_eq!(conflict_name("report.docx", "laptop", 1), "report-laptop.docx");
        assert_eq!(conflict_name("report.docx", "laptop", 3), "report-laptop-3.docx");
        assert_eq!(conflict_name("Makefile", "pc", 1), "Makefile-pc");
        assert_eq!(conflict_name(".bashrc", "pc", 1), ".bashrc-pc");
        assert_eq!(split_ext("a.tar.gz"), ("a.tar", ".gz"));
    }
}
