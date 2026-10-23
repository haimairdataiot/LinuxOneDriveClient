# Changelog

All notable changes to Linux OneDrive. Versions follow `MAJOR.MINOR.PATCH`; each
release is tagged `vX.Y.Z` and published on
[GitHub](https://github.com/haimairdataiot/LinuxOneDriveClient/releases).

## 0.3.1 - 2026-10-22

### Fixed
- **An account can no longer be added twice.** When two sign-ins to the same account
  finished at about the same time (for example one in the web browser and one in the
  app's own sign-in page), both could add it, and the account then appeared twice with two
  folders. Sign-ins now finish one at a time, and signing in to an account that is already
  set up only renews its sign-in. If an account is already set up twice, the service log
  says so; see the user guide for removing the extra one.
- In the activity panel's account list, two accounts that would read the same also show
  their folders, so you can tell them apart.
- **Installing an update no longer waits forever when no password prompt appears.** The
  update window says what it is waiting for: your approval, another software update that
  has to finish first, or the installation itself. While it waits for approval you can
  cancel, and after a while it explains that the prompt may be in another session and how to
  install from a terminal instead.
- Opening the update window again while an update is being installed shows the same window
  instead of starting a second installation.

## 0.3.0 - 2026-10-22

### Added
- **Choose the account in the activity panel.** With more than one account signed in, the
  name at the top of the panel is a dropdown: pick an account to see its status, recent
  activity and storage. *Open folder* and *View online* follow the account you pick.

## 0.2.1 - 2026-10-10

### Fixed
- **Signing in no longer closes the app on Ubuntu 24.04.** The built-in sign-in page needs
  a permission that Ubuntu 24.04 only gives to programs with an AppArmor profile, and the
  package had none, so the app stopped as soon as the page opened. The package now
  installs that profile.
- Where the built-in sign-in page still can't start (for example a copy of the app run
  from outside `/usr/bin`), sign-in continues in your web browser instead.
- Settings opened from the tray icon now fits on small laptop screens (its pages scroll
  instead of making the window as tall as the longest page).
- Settings, Share and other dialogs opened on their own appear under the Linux OneDrive
  icon in the dock and the window switcher, not as a separate unnamed program.

### Documentation
- Screenshots in the README.

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
