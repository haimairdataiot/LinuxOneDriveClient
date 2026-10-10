# Changelog

All notable changes to Linux OneDrive. Versions follow `MAJOR.MINOR.PATCH`; each
release is tagged `vX.Y.Z` and published on
[GitHub](https://github.com/haimairdataiot/LinuxOneDriveClient/releases).

## 0.2.0 - 2026-10-10

### Added
- **Updates from inside the app.** The sync service looks for a new release on GitHub
  once a day and tells you with a notification. *Settings > Advanced > Updates*, the tray
  menu and `linux-onedrivectl update` show what's new and install it. The package is
  checked against the release's checksums, and installing always asks for your
  administrator password. Nothing is installed silently.
- Releases are built and published automatically when a version is tagged.
- Sign-in works out of the box: packages now include the project's Microsoft app
  registration. Forks and organisations can still use their own.

### Changed
- Sign-in with a code (`linux-onedrivectl login --device`) is only offered with your own
  app registration, not with the built-in one.
- The command-line tool shows items in shared folders with the path you typed.

### Fixed
- No em dashes in command-line output (they broke some terminals and scripts).

## 0.1.0 - 2026-10-09

First release. Packages `0.1.0-1` to `0.1.0-4`.

### Added
- **Files On-Demand:** the whole OneDrive appears in a folder at once; files download
  when opened. *Always keep on this device* and *Free up space* per file and folder.
- **Sync engine** with crash-safe change tracking, verified downloads and uploads,
  keep-both conflict handling, a hold on mass deletions, a 7-day safety copy of files
  deleted online, and recovery of unsynced work after a crash.
- **App and tray icon:** activity panel, embedded Microsoft sign-in, settings, sync
  issues, version history, folder backup (Desktop, Documents, Pictures, Music, Videos).
- **Sharing:** share with people, links with expiry and password, manage access.
- **Shared folders and SharePoint libraries:** shortcuts added with *Add shortcut to My
  files* appear in the OneDrive folder; libraries sync from Settings or the browser's
  Sync button.
- **File-manager integration:** status badges and a OneDrive menu in Files (Nautilus),
  service menu in Dolphin.
- Command-line tool `linux-onedrivectl`, real-time change notifications, previews of
  online-only photos and videos, streaming of large files, `.deb` package.

### Fixed (packages 0.1.0-3 and 0.1.0-4, from the first run against a real work account)
- Shared folders use change tracking instead of listing every folder, which was slow
  and got throttled on large folders.
- *Open OneDrive Folder* and *Settings* did nothing in some desktop sessions.
- The service could hang at shutdown when a folder was in use.
- The account status now includes shared folders and libraries.
- Works with libadwaita 1.5 and GTK 4.14 (Ubuntu 24.04).
