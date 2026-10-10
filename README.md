# Linux OneDrive

An unofficial, open-source OneDrive client for Linux desktops. Your whole OneDrive shows up in a
folder straight away, and files download only when you open them, the way Files On-Demand works
on Windows.

> [!WARNING]
> **Use at your own risk.** This is early software (version 0.2) from an independent project,
> not from Microsoft. It reads, changes and deletes files in your OneDrive on your behalf. It has had
> limited testing against real accounts, so bugs may cause data loss or unwanted changes. Keep
> a separate backup of anything you can't afford to lose. The software comes with **no
> warranty** of any kind, as set out in the [licence](LICENSE).

Not affiliated with, endorsed by or supported by Microsoft. "OneDrive", "SharePoint" and
"Microsoft 365" are trademarks of Microsoft Corporation.

## What it does

- **Files On-Demand.** Every file is visible at once; content downloads when opened. *Always
  keep on this device* and *Free up space* work per file and per folder.
- **Two-way sync in the background**, as a systemd user service. Changes are verified, conflicts
  keep both versions, mass deletions wait for your confirmation, and files deleted online stay
  recoverable on this device for 7 days.
- **Tray icon and app** (GTK 4, libadwaita): activity, Microsoft sign-in inside the app,
  settings, sync issues, version history, and backup of Desktop, Documents and Pictures.
- **Sharing**: share with people, links with expiry and password, manage access.
- **Shared folders and SharePoint libraries.** Folders added with *Add shortcut to My files*
  appear in your OneDrive folder; libraries sync from Settings or the browser's Sync button.
- **File-manager integration**: status badges and a OneDrive menu in Files (Nautilus), a
  service menu in Dolphin.
- **Updates from inside the app**, from this repository's releases.
- A command-line tool, `linux-onedrivectl`.

Personal, work and school accounts are supported.

## Install

Download the latest `.deb` from the
[releases page](https://github.com/haimairdataiot/LinuxOneDriveClient/releases/latest), then:

```bash
sudo apt install ./linux-onedrive_*_amd64.deb
```

Open **Linux OneDrive** from the app grid and sign in. The [user guide](docs/USER_GUIDE.md)
covers setup, everyday use and troubleshooting.

Requirements: Ubuntu 24.04 or newer (or a distribution with GTK 4.14, libadwaita 1.5,
WebKitGTK 6.0 and FUSE 3), on x86-64. For the tray icon on GNOME, the AppIndicator extension;
for badges in Files, `python3-nautilus`.

## Updating

The app checks for a new release once a day and tells you. Install it from
*Settings > Advanced > Updates*, from the tray menu, or with:

```bash
linux-onedrivectl update --install
```

The package is verified against the release's checksums, and installing always asks for your
administrator password.

## Security and privacy

- You sign in on Microsoft's own page, shown inside the app or in your browser. The app does
  not read or store your password. It keeps sign-in tokens in your login keyring.
- After sign-in the app can read, change and delete everything in your OneDrive and in folders
  shared with you. That is what a sync client needs, and it is why the warning above matters.
- The sync service runs as you, not as root, and talks only to Microsoft and, for the update
  check, to GitHub. There is no telemetry.
- Updates come only from this repository's releases over HTTPS, are checked against the
  release's `SHA256SUMS` file, and are installed only when you start the installation and
  approve it with your administrator password.

Please report security problems privately through this repository's
[security advisories](https://github.com/haimairdataiot/LinuxOneDriveClient/security/advisories/new),
not in a public issue.

## Documentation

- [User guide](docs/USER_GUIDE.md): setup, Files On-Demand, sharing, shared folders, updating,
  troubleshooting
- [Changelog](CHANGELOG.md): what changed in each release

## Build from source

```bash
sudo apt install build-essential pkg-config rustc cargo libgtk-4-dev libadwaita-1-dev \
    libwebkitgtk-6.0-dev libsqlite3-dev fuse3 dpkg-dev
packaging/deb/build-deb.sh
sudo apt install ./dist/linux-onedrive_*_amd64.deb
```

Rust 1.92 or newer is needed (use [rustup](https://rustup.rs) if your distribution's is older).

If you publish your own build, register your own application with Microsoft (under *App
registrations* at <https://entra.microsoft.com>, as a public client with the redirect
addresses `https://login.microsoftonline.com/common/oauth2/nativeclient` and
`http://localhost`) and build with `LINUX_ONEDRIVE_CLIENT_ID=<your-id>`, so your users don't
sign in under this project's name.

## Development

```bash
cargo test --workspace
```

The tests run against `od-mockgraph`, an in-memory stand-in for the OneDrive service, so no
Microsoft account is needed. They include end-to-end tests on real FUSE mounts and tests of the
real service on a private D-Bus bus.

Repository layout:

```
crates/
  od-core/            shared types, configuration, naming rules, QuickXorHash, D-Bus interface, update rules
  od-graph/           Microsoft Graph and sign-in client (OAuth with PKCE, delta, transfers, sharing)
  od-engine/          SQLite metadata store, sync engine and the FUSE filesystem for Files On-Demand
  od-mockgraph/       in-memory stand-in for OneDrive, used by the tests
  linux-onedrived/    the sync service (D-Bus API, accounts, shared folders, updates)
  linux-onedrive/     the app and tray icon
  linux-onedrivectl/  command-line tool
integration/          Files (Nautilus) extension, Dolphin service menu
data/                 systemd unit, desktop entries, icons, metainfo
packaging/            package build script, release notes helper
docs/                 user guide
```

## Licence

[GPL-3.0-or-later](LICENSE).
