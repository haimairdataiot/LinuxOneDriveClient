//! Human-friendly formatting shared by the GUI and CLI.

use chrono::{DateTime, Utc};

/// `1536` → `1.5 KB`; uses decimal units like the official client.
pub fn size(bytes: u64) -> String {
    const UNITS: &[&str] = &["bytes", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return if bytes == 1 { "1 byte".into() } else { format!("{bytes} bytes") };
    }
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if v >= 100.0 { format!("{v:.0} {}", UNITS[unit]) } else { format!("{v:.1} {}", UNITS[unit]) }
}

/// "just now", "5 minutes ago", "yesterday", "3 days ago", else a date.
pub fn relative_time(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - t).num_seconds();
    if secs < 45 {
        return "just now".into();
    }
    let mins = (secs + 30) / 60;
    if mins < 60 {
        return if mins == 1 { "1 minute ago".into() } else { format!("{mins} minutes ago") };
    }
    let hours = (mins + 30) / 60;
    if hours < 24 {
        return if hours == 1 { "1 hour ago".into() } else { format!("{hours} hours ago") };
    }
    let days = hours / 24;
    match days {
        1 => "yesterday".into(),
        2..=6 => format!("{days} days ago"),
        _ => t.format("%-d %b %Y").to_string(),
    }
}

/// Progress line used by the status card: "12.4 MB of 40.0 MB".
pub fn progress(done: u64, total: u64) -> String {
    format!("{} of {}", size(done), size(total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn sizes() {
        assert_eq!(size(0), "0 bytes");
        assert_eq!(size(1), "1 byte");
        assert_eq!(size(999), "999 bytes");
        assert_eq!(size(1500), "1.5 KB");
        assert_eq!(size(12_400_000), "12.4 MB");
        assert_eq!(size(1_000_000_000_000), "1.0 TB");
        assert_eq!(size(250_000_000), "250 MB");
    }

    #[test]
    fn relative() {
        let now = Utc::now();
        assert_eq!(relative_time(now - Duration::seconds(10), now), "just now");
        assert_eq!(relative_time(now - Duration::minutes(5), now), "5 minutes ago");
        assert_eq!(relative_time(now - Duration::minutes(61), now), "1 hour ago");
        assert_eq!(relative_time(now - Duration::hours(30), now), "yesterday");
        assert_eq!(relative_time(now - Duration::days(3), now), "3 days ago");
    }
}
