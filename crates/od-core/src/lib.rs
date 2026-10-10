//! Shared building blocks for linux-onedrive: configuration, filesystem paths,
//! status types, OneDrive naming rules and the D-Bus interface contract used
//! between the daemon, the GUI and the CLI.

pub mod config;
pub mod dbus;
pub mod fmt;
pub mod names;
pub mod paths;
pub mod quickxor;
pub mod types;
pub mod update;

/// Reverse-DNS application id of the GUI (also the `.desktop` file basename).
pub const APP_ID: &str = "com.airdataiot.LinuxOneDrive";
/// Human-readable product name shown in the UI.
pub const APP_NAME: &str = "Linux OneDrive";
/// Crate version shared by all binaries.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short host name, used for conflict copies (`report-<host>.docx`) and the
/// device name shown on the account page.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buffer is valid for `len` bytes; gethostname NUL-terminates on success.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let name = String::from_utf8_lossy(&buf[..end]).into_owned();
        let short = name.split('.').next().unwrap_or("").trim().to_string();
        if !short.is_empty() {
            return short;
        }
    }
    "linux".to_string()
}
