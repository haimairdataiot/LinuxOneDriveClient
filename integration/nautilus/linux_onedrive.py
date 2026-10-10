"""Linux OneDrive integration for GNOME Files (Nautilus 43-50, nautilus-python 4.x).

* Status emblems next to every file in a OneDrive folder (online-only,
  available on this device, always kept, syncing, can't sync).
* A "OneDrive status" column for the list view.
* Right-click menu: Always keep on this device / Free up space / Share /
  Copy link / Manage access / View online / Version history.

Status is read with asynchronous GIO xattr queries (answered from the
daemon's database, no network) and refreshed when the daemon signals that
items changed. Install to /usr/share/nautilus-python/extensions/ (packaged)
or ~/.local/share/nautilus-python/extensions/, then run `nautilus -q`.
Requires python3-nautilus.
"""

import os
import subprocess

import gi

try:
    gi.require_version("Nautilus", "4.1")
except ValueError:
    gi.require_version("Nautilus", "4.0")
from gi.repository import Gio, GLib, GObject, Nautilus  # noqa: E402

BUS = "com.airdataiot.LinuxOneDrive.Daemon"
MANAGER_PATH = "/com/airdataiot/LinuxOneDrive"
MANAGER_IFACE = "com.airdataiot.LinuxOneDrive.Manager"
ACCOUNT_IFACE = "com.airdataiot.LinuxOneDrive.Account"

STATUS_ATTR = "xattr::onedrive.status"  # -> getxattr("user.onedrive.status")
PIN_ATTR = "xattr::onedrive.pin"
SHARED_ATTR = "xattr::onedrive.shared"

EMBLEMS = {
    "online": "linux-onedrive-online",
    "local": "linux-onedrive-local",
    "pinned": "linux-onedrive-pinned",
    "syncing": "linux-onedrive-syncing",
    "error": "linux-onedrive-error",
}
LABELS = {
    "online": "Available when online",
    "local": "Available on this device",
    "pinned": "Always available on this device",
    "syncing": "Syncing",
    "error": "Can't sync",
    "excluded": "Not synced",
}


def _gui(*args):
    try:
        subprocess.Popen(["linux-onedrive", *args], start_new_session=True)
    except OSError:
        pass


class LinuxOneDriveExtension(
    GObject.GObject,
    Nautilus.InfoProvider,
    Nautilus.MenuProvider,
    Nautilus.ColumnProvider,
):
    def __init__(self):
        super().__init__()
        self.mounts = {}  # mount point -> account object path
        self.bus = None
        try:
            self.bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        except GLib.Error:
            return
        # Refresh emblems when the daemon reports changes.
        self.bus.signal_subscribe(
            BUS, ACCOUNT_IFACE, "ItemsChanged", None, None,
            Gio.DBusSignalFlags.NONE, self._on_items_changed,
        )
        # Re-read the list of mounts whenever accounts change.
        self.bus.signal_subscribe(
            BUS, "org.freedesktop.DBus.Properties", "PropertiesChanged", MANAGER_PATH, None,
            Gio.DBusSignalFlags.NONE, lambda *a: self._load_mounts(),
        )
        self._load_mounts()

    # ------------------------------------------------------------------ mounts

    def _get_property(self, path, iface, name, callback):
        def done(conn, res):
            try:
                value = conn.call_finish(res).unpack()[0]
            except GLib.Error:
                value = None
            callback(value)

        self.bus.call(
            BUS, path, "org.freedesktop.DBus.Properties", "Get",
            GLib.Variant("(ss)", (iface, name)), GLib.VariantType("(v)"),
            Gio.DBusCallFlags.NO_AUTO_START, 2000, None, done,
        )

    def _load_mounts(self):
        if self.bus is None:
            return

        def got_accounts(paths):
            if not paths:
                return
            for p in paths:
                self._get_property(p, ACCOUNT_IFACE, "MountPoint",
                                   lambda mp, p=p: mp and self.mounts.__setitem__(mp, p))

        self._get_property(MANAGER_PATH, MANAGER_IFACE, "Accounts", got_accounts)

    def _account_for(self, path):
        if not path:
            return None
        for mount, account in self.mounts.items():
            if path == mount or path.startswith(mount + "/"):
                return account
        return None

    @staticmethod
    def _path(fi):
        loc = fi.get_location()
        return loc.get_path() if loc is not None else None

    # ------------------------------------------------------------------ emblems

    def _on_items_changed(self, conn, sender, path, iface, signal, params):
        for p in params.unpack()[0]:
            fi = Nautilus.FileInfo.lookup(Gio.File.new_for_path(p))
            if fi is not None:
                fi.invalidate_extension_info()

    def update_file_info_full(self, provider, handle, closure, fi):
        path = self._path(fi)
        if self._account_for(path) is None:
            return Nautilus.OperationResult.COMPLETE

        def done(gfile, res):
            result = Nautilus.OperationResult.COMPLETE
            try:
                info = gfile.query_info_finish(res)
                status = info.get_attribute_string(STATUS_ATTR) or ""
                shared = info.get_attribute_string(SHARED_ATTR) == "1"
                if status in EMBLEMS:
                    fi.add_emblem(EMBLEMS[status])
                if shared:
                    fi.add_emblem("linux-onedrive-shared")
                fi.add_string_attribute("linux_onedrive_status", LABELS.get(status, ""))
            except GLib.Error:
                result = Nautilus.OperationResult.FAILED
            Nautilus.info_provider_update_complete_invoke(closure, provider, handle, result)

        fi.get_location().query_info_async(
            f"{STATUS_ATTR},{SHARED_ATTR}", Gio.FileQueryInfoFlags.NOFOLLOW_SYMLINKS,
            GLib.PRIORITY_LOW, None, done,
        )
        return Nautilus.OperationResult.IN_PROGRESS

    def cancel_update(self, provider, handle):
        pass

    def get_columns(self):
        return [
            Nautilus.Column(
                name="LinuxOneDrive::status",
                attribute="linux_onedrive_status",
                label="OneDrive status",
                description="Whether the file is online-only or available on this device",
            )
        ]

    # ------------------------------------------------------------------ menus

    def _call(self, account, method, params, on_done=None):
        def done(conn, res):
            try:
                out = conn.call_finish(res)
            except GLib.Error as e:
                self._notify_error(e.message)
                return
            if on_done is not None:
                on_done(out)

        self.bus.call(BUS, account, ACCOUNT_IFACE, method, params, None,
                      Gio.DBusCallFlags.NONE, 60000, None, done)

    def _notify_error(self, message):
        msg = message.split(":", 1)[-1].strip() if message else "Something went wrong"
        n = Gio.Notification.new("OneDrive")
        n.set_body(msg)
        app = Gio.Application.get_default()
        if app is not None:
            app.send_notification(None, n)

    def _set_pin(self, files, state):
        paths = [self._path(f) for f in files]
        account = self._account_for(paths[0])
        if account:
            self._call(account, "SetPinState", GLib.Variant("(ass)", (paths, state)),
                       lambda _o: [f.invalidate_extension_info() for f in files])

    def _copy_link(self, path):
        account = self._account_for(path)

        def got(out):
            url = out.unpack()[0]
            try:
                gi.require_version("Gdk", "4.0")
                from gi.repository import Gdk

                clipboard = Gdk.Display.get_default().get_clipboard()
                clipboard.set_content(Gdk.ContentProvider.new_for_value(url))
            except (ValueError, ImportError, AttributeError, TypeError):
                # No GTK 4 clipboard in this process: let the app copy it.
                _gui("--show", f"copy-link:{path}")

        # The default link from Settings (like "Copy link" on Windows).
        self._call(account, "CopyLink", GLib.Variant("(s)", (path,)), got)

    def _view_online(self, path):
        account = self._account_for(path)

        def got(out):
            Gio.AppInfo.launch_default_for_uri(out.unpack()[0], None)

        self._call(account, "WebUrl", GLib.Variant("(s)", (path,)), got)

    def _item(self, name, label, callback):
        item = Nautilus.MenuItem(name=f"LinuxOneDrive::{name}", label=label)
        item.connect("activate", lambda *_: callback())
        return item

    def get_file_items(self, files):
        if not files or self.bus is None:
            return []
        paths = [self._path(f) for f in files]
        accounts = {self._account_for(p) for p in paths}
        if None in accounts or len(accounts) != 1:
            return []
        top = Nautilus.MenuItem(name="LinuxOneDrive::Menu", label="OneDrive")
        sub = Nautilus.Menu()
        top.set_submenu(sub)
        sub.append_item(self._item("Keep", "Always Keep on This Device", lambda: self._set_pin(files, "pinned")))
        sub.append_item(self._item("Free", "Free Up Space", lambda: self._set_pin(files, "unpinned")))
        if len(files) == 1:
            p = paths[0]
            sub.append_item(self._item("Share", "Share…", lambda: _gui("--show", f"share:{p}")))
            sub.append_item(self._item("CopyLink", "Copy Link", lambda: self._copy_link(p)))
            sub.append_item(self._item("Access", "Manage Access", lambda: _gui("--show", f"manage-access:{p}")))
            sub.append_item(self._item("Web", "View Online", lambda: self._view_online(p)))
            if not files[0].is_directory():
                sub.append_item(self._item("Versions", "Version History", lambda: _gui("--show", f"versions:{p}")))
        return [top]

    def get_background_items(self, folder):
        if self.bus is None:
            return []
        p = self._path(folder)
        if self._account_for(p) is None:
            return []
        top = Nautilus.MenuItem(name="LinuxOneDrive::BgMenu", label="OneDrive")
        sub = Nautilus.Menu()
        top.set_submenu(sub)
        sub.append_item(self._item("BgWeb", "View Online", lambda: self._view_online(p)))
        sub.append_item(self._item("BgKeep", "Always Keep This Folder on This Device", lambda: self._set_pin([folder], "pinned")))
        sub.append_item(self._item("BgFree", "Free Up Space in This Folder", lambda: self._set_pin([folder], "unpinned")))
        sub.append_item(self._item("BgOpen", "Show Linux OneDrive", lambda: _gui("--show", "activity")))
        return [top]
