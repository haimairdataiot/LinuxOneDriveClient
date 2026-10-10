# Linux OneDrive: user guide

Linux OneDrive puts your whole OneDrive in a folder on your computer (normally
`~/OneDrive`). Your files show up right away and download only when you open them, the
same way Files On-Demand works on Windows.

> Unofficial client. It isn't affiliated with or endorsed by Microsoft.

## Install

Download the latest `.deb` from the
[releases page](https://github.com/haimairdataiot/LinuxOneDriveClient/releases/latest), then:

```bash
sudo apt install ./linux-onedrive_*_amd64.deb
```

Recommended extras. The package suggests them, and they're preinstalled on Ubuntu:
- **Tray icon on GNOME:** the *AppIndicator* extension (`gnome-shell-extension-appindicator`,
  enabled by default on Ubuntu).
- **Status icons and menu in Files (Nautilus):** `sudo apt install python3-nautilus`, then
  `nautilus -q`.

## Set up
1. Open **Linux OneDrive** from the app grid. After installation it also starts by itself
   the next time you log in.
2. Enter your email and choose **Sign In**. Microsoft's sign-in page opens inside the
   window, and you sign in there as usual (password, Authenticator app, passkey).
3. Check where your OneDrive folder goes (default `~/OneDrive`; work or school accounts
   use `~/OneDrive - <Organization>`) and choose **Next**.
4. Choose **Open My OneDrive Folder**. Done.

If the sign-in page can't finish (for example with a hardware security key), use **Sign in
with your web browser instead**.

## Files On-Demand (on by default)

| Icon | Meaning |
|------|---------|
| ☁ cloud | **Online-only.** Takes no space on this device; it downloads when you open it. Needs an internet connection. |
| ✓ outline check | **Available on this device.** You opened it, so it stays here. You can free up its space at any time. |
| ✓ solid check | **Always available.** You chose *Always keep on this device*. It downloads now and stays, even offline. |
| ⇅ arrows | **Syncing.** |
| ✕ red | **Can't sync.** See *Sync issues*. |

In Files, right-click a file or folder and open **OneDrive** to choose **Always Keep on
This Device** or **Free Up Space**. Settings → *Sync and back up* → **Download all
files** keeps everything on this device instead.

Thumbnailers and desktop search never download online-only files. Previews of online
photos come from OneDrive.

Status icons appear in Files once the *Files* app has been restarted after installing
(log out and in, or run `nautilus -q`). The *OneDrive status* column is available in list
view under *Visible Columns*.

## Day to day
- **Tray icon:** a cloud in the top bar shows whether everything is synced, syncing,
  paused, offline or needs attention. Click it for the menu; double-click (or *Show
  Activity*) for the activity panel.
- **Activity panel:** the status, a progress bar, recently synced files (click one to open
  it; ⋮ for share, copy link, version history) and how much storage you use.
- **Pause syncing:** choose 2, 8 or 24 hours from the tray or the ⚙ menu. Syncing also
  pauses on metered networks (and in power-saver mode, if you turn that option on).
- **Share:** right-click → OneDrive → *Share…*, *Copy Link* or *Manage Access* (see
  [Sharing](#sharing)).
- **Version history:** right-click → OneDrive → *Version History* to restore or download an
  older version.
- **Trash:** files you move to the Trash are deleted from OneDrive too (into the OneDrive
  recycle bin). Restoring them from the Trash uploads them again.
- **Back up important folders:** Settings → *Sync and back up* → **Manage Back Up** moves
  Desktop, Documents, Pictures, Music or Videos into OneDrive. Each file is copied and
  checked before the original is removed, and the folder keeps working from the same place.
- **Recently removed files:** ⚙ → *Recently Removed Files* brings back downloaded files
  that were deleted online in the last 7 days.

## Sharing
Right-click a file or folder → **OneDrive** → **Share…** opens the same choices as on
Windows:
- **Share with people:** type names or email addresses (suggestions come from people
  you've shared with, and, once you allow it, from Microsoft 365), choose *Can edit* or
  *Can view*, add a message and choose **Send**. Turn off *Notify people by email* to give
  access without sending an email.
- **Copy link:** creates a link and copies it. ⚙ changes who it works for (*Anyone*,
  *People in your organization*, *People with existing access*, *Specific people*), the
  permission, an expiration date and a password. Expiration dates and passwords need
  Microsoft 365 on personal accounts.
- **Manage access** (the people icon): every link and person with access. Copy a link,
  change someone's permission, remove access, or **Stop Sharing** altogether.

**Copy Link** in the right-click menu copies the default link straight away (*Anyone with
the link can edit* on personal accounts, *People in your organization* on work or school
accounts; change it in Settings → *Advanced* → *Sharing*).

OneDrive keeps one link of each kind per item. If one already exists with different
settings, you're asked whether to copy it as it is or replace it. Replacing it means
people who have the old link lose access.

## Shared folders and SharePoint libraries
- **Shared folders:** when someone shares a folder with you, open OneDrive on the web,
  go to *Shared*, and choose **Add shortcut to My files**. The folder then appears in your
  OneDrive folder with a link arrow, and syncs like the rest (Files On-Demand included).
  Deleting the shortcut on this device removes the shortcut only. The shared folder and
  its files aren't affected, for you or anyone else. *Deleting the files inside it* deletes
  them for everyone, as on Windows.
- **SharePoint libraries** (work or school accounts): Settings → *Account* → *Shared
  folders and libraries* → **Sync a SharePoint Library…**, pick a site and a library. Or
  choose **Sync** on the library in your browser. It appears in
  `~/<Organization>/<Site> - <Library>`.
- **Stop syncing** a library or **remove** a shortcut from the same Settings list.

## When things go wrong, your files stay safe
- **Edited in two places:** both versions are kept. Yours is saved as
  `name-<computer>.ext`, and the online version keeps the name.
- **Many files deleted at once:** if more than 200 files are deleted on this device, they
  aren't removed online until you choose **Remove** or **Restore Files**.
- **Deleted online while you changed it here:** your version is kept and uploaded again.
- **Crashes, power cuts, full disks:** every change is recorded before it's confirmed, and
  every download is verified (size and hash) before it replaces anything. Unsynced
  changes that can't be matched after a crash are saved to `Recovered files (<date>)`.
- **Files deleted online:** this device keeps its downloaded copy for 7 days in
  `~/.local/share/linux-onedrive/accounts/<id>/trash/`.

## Command line

```bash
linux-onedrivectl status                 # what's happening
linux-onedrivectl login                  # sign in with your web browser
linux-onedrivectl update                 # is there a new version? (--install to install it)
linux-onedrivectl pin ~/OneDrive/Photos  # always keep on this device
linux-onedrivectl free ~/OneDrive/Videos # free up space
linux-onedrivectl pause 2h               # pause syncing
linux-onedrivectl share ~/OneDrive/plan.docx            # copy-link default
linux-onedrivectl share --view --expires 7 ~/OneDrive/plan.docx
linux-onedrivectl invite ~/OneDrive/plan.docx ana@contoso.com --edit -m "Have a look"
linux-onedrivectl access ~/OneDrive/plan.docx            # who has access
linux-onedrivectl unshare ~/OneDrive/plan.docx --all     # stop sharing
linux-onedrivectl locations                              # shared folders and libraries
linux-onedrivectl sites marketing                        # find a SharePoint site
linux-onedrivectl issues --retry
linux-onedrivectl mass-delete --restore
linux-onedrivectl settings network.upload_limit_kbps=500
```

KDE Dolphin users get the same pin, free, share, copy link, manage access and version
actions under right-click → **OneDrive**.

## Updating
Linux OneDrive looks for a new version once a day and shows a notification when there is
one. To see what's new and install it, use **Settings > Advanced > Updates**, the tray menu's
**Install Update**, or:

```bash
linux-onedrivectl update --install
```

The download is checked against the release's checksums before anything is installed, and
installing asks for your administrator password. Your files, settings and sign-in are kept.
Turn the daily check off with the switch in the same Settings section. Releases and their
notes are on [GitHub](https://github.com/haimairdataiot/LinuxOneDriveClient/releases).

## Troubleshooting
- **Logs:**
  ```bash
  journalctl --user -u linux-onedrived -f
  ```
- **Restart the service:**
  ```bash
  systemctl --user restart linux-onedrived
  ```
- **"Transport endpoint is not connected" on `~/OneDrive`:** the service stopped
  unexpectedly. Restarting it cleans this up automatically.
- **No tray icon on GNOME:** enable the AppIndicator extension in *Extensions*.
- **No emblems in Files:** install `python3-nautilus` and run `nautilus -q`.
- **Sign-in with a code** (`login --device`) is only available when you use your own
  Microsoft app registration (`client_id` under `[general]` in the settings file).
- **Your organisation needs admin approval:** the sign-in window says so and shows the
  link to send to your IT administrator, who can approve the Linux OneDrive app for the
  organisation.

## Where things live

| What | Where |
|------|-------|
| Settings | `~/.config/linux-onedrive/config.toml` |
| Sync database, downloaded content | `~/.local/share/linux-onedrive/accounts/<id>/` |
| Sign-in tokens | your login keyring (or a private file if no keyring is available) |
| Shared folders (where their links point) | `~/.local/share/linux-onedrive/shortcuts/<id>/` |
| Unlinked accounts' leftover data | `~/.local/share/linux-onedrive/unlinked/` |
