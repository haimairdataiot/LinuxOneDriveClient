//! Status vocabulary shared by the daemon, GUI, CLI and file-manager extension.
//!
//! Every enum has a stable lowercase string form because it crosses process
//! boundaries (D-Bus, xattrs, JSON).

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

macro_rules! string_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),+ }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];
            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $s),+ }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = UnknownVariant;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s { $($s => Ok($name::$variant),)+ _ => Err(UnknownVariant(s.to_string())) }
            }
        }
    };
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("unknown value {0:?}")]
pub struct UnknownVariant(pub String);

string_enum! {
    /// Per-item state shown as an emblem / status column, mirroring the
    /// official client's File Explorer status icons.
    ItemStatus {
        OnlineOnly => "online",
        Local => "local",
        Pinned => "pinned",
        Syncing => "syncing",
        Error => "error",
        Excluded => "excluded",
    }
}

string_enum! {
    /// User intent for keeping content on this device. Folders propagate it to
    /// descendants that are set to `Inherit`.
    PinState {
        Inherit => "inherit",
        AlwaysKeep => "pinned",
        OnlineOnly => "unpinned",
    }
}

string_enum! {
    /// Aggregate state of an account or of the whole daemon; drives the tray icon.
    SyncState {
        Idle => "idle",
        Syncing => "syncing",
        Paused => "paused",
        Offline => "offline",
        Error => "error",
        SignInRequired => "signin-required",
        SignedOut => "signed-out",
        Starting => "starting",
    }
}

string_enum! {
    /// Why syncing is currently paused (several may apply; the first wins in the UI).
    PauseReason {
        User => "user",
        Metered => "metered",
        PowerSaver => "power-saver",
        MassDelete => "mass-delete",
    }
}

string_enum! {
    DriveType {
        Personal => "personal",
        Business => "business",
        DocumentLibrary => "documentLibrary",
    }
}

string_enum! {
    /// Entries of the activity feed ("Recent" list in the activity panel).
    ActivityKind {
        Uploaded => "uploaded",
        Downloaded => "downloaded",
        Deleted => "deleted",
        DeletedRemote => "deleted-remote",
        Renamed => "renamed",
        Moved => "moved",
        Created => "created",
        Conflict => "conflict",
        Error => "error",
        FreedUp => "freed-up",
        Pinned => "pinned",
    }
}

impl ActivityKind {
    /// Short past-tense phrase used in activity rows: "Uploaded to Documents".
    pub fn verb(self) -> &'static str {
        match self {
            ActivityKind::Uploaded => "Uploaded to",
            ActivityKind::Downloaded => "Downloaded from",
            ActivityKind::Deleted => "Deleted from",
            ActivityKind::DeletedRemote => "Deleted online from",
            ActivityKind::Renamed => "Renamed in",
            ActivityKind::Moved => "Moved to",
            ActivityKind::Created => "Created in",
            ActivityKind::Conflict => "Kept both versions in",
            ActivityKind::Error => "Couldn't sync in",
            ActivityKind::FreedUp => "Freed up space in",
            ActivityKind::Pinned => "Made available offline in",
        }
    }
}

impl SyncState {
    /// Icon name (from our hicolor theme) for the tray and the activity panel.
    pub fn icon_name(self) -> &'static str {
        match self {
            SyncState::Idle => "linux-onedrive-synced-symbolic",
            SyncState::Syncing | SyncState::Starting => "linux-onedrive-syncing-symbolic",
            SyncState::Paused => "linux-onedrive-paused-symbolic",
            SyncState::Offline => "linux-onedrive-offline-symbolic",
            SyncState::Error => "linux-onedrive-error-symbolic",
            SyncState::SignInRequired | SyncState::SignedOut => "linux-onedrive-signedout-symbolic",
        }
    }

    /// Precedence when aggregating several accounts into one tray state.
    pub fn severity(self) -> u8 {
        match self {
            SyncState::Idle => 0,
            SyncState::Starting => 1,
            SyncState::Syncing => 2,
            SyncState::Paused => 3,
            SyncState::Offline => 4,
            SyncState::SignedOut => 5,
            SyncState::SignInRequired => 6,
            SyncState::Error => 7,
        }
    }
}

/// Extended attribute names exposed on every file in the mount.
pub mod xattr {
    pub const STATUS: &str = "user.onedrive.status";
    pub const PIN: &str = "user.onedrive.pin";
    pub const ID: &str = "user.onedrive.id";
    pub const WEB_URL: &str = "user.onedrive.weburl";
    pub const SHARED: &str = "user.onedrive.shared";
    pub const ERROR: &str = "user.onedrive.error";
    pub const ALL: &[&str] = &[STATUS, PIN, ID, WEB_URL, SHARED, ERROR];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_strings() {
        for s in ItemStatus::ALL {
            assert_eq!(s.as_str().parse::<ItemStatus>().unwrap(), *s);
        }
        for s in SyncState::ALL {
            assert_eq!(s.as_str().parse::<SyncState>().unwrap(), *s);
        }
        assert!("bogus".parse::<PinState>().is_err());
        assert_eq!(serde_json::to_string(&SyncState::SignInRequired).unwrap(), "\"signin-required\"");
    }
}
