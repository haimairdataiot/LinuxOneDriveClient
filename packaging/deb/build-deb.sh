#!/usr/bin/env bash
# Build dist/linux-onedrive_<version>_<arch>.deb
#
#   packaging/deb/build-deb.sh
#
# The package signs in with the project's Microsoft Entra application ID.
# To build with your own registration (forks, organisations) set
#   LINUX_ONEDRIVE_CLIENT_ID=<your-application-id>
# (register one under "App registrations" at https://entra.microsoft.com).
# DEB_REVISION sets the package revision (default 1).
#
# Requires: cargo, dpkg-dev (dpkg-shlibdeps), and the -dev packages listed in README.md.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
REVISION="${DEB_REVISION:-1}"
ARCH="$(dpkg --print-architecture)"
PKG=linux-onedrive
STAGE="$ROOT/dist/pkgroot"
OUT="$ROOT/dist/${PKG}_${VERSION}-${REVISION}_${ARCH}.deb"
# Contact shown in the package's metadata (override with DEB_MAINTAINER).
MAINTAINER="${DEB_MAINTAINER:-Linux OneDrive contributors <haimairdataiot@users.noreply.github.com>}"

if [ -n "${LINUX_ONEDRIVE_CLIENT_ID:-}" ]; then
  echo "note: building with the application ID from LINUX_ONEDRIVE_CLIENT_ID instead of the project's" >&2
fi

echo "==> building release binaries"
cargo build --release --locked -p linux-onedrived -p linux-onedrive -p linux-onedrivectl

echo "==> staging files"
rm -rf "$STAGE"
install -Dm755 target/release/linux-onedrived    "$STAGE/usr/bin/linux-onedrived"
install -Dm755 target/release/linux-onedrive     "$STAGE/usr/bin/linux-onedrive"
install -Dm755 target/release/linux-onedrivectl  "$STAGE/usr/bin/linux-onedrivectl"
install -Dm644 data/systemd/linux-onedrived.service "$STAGE/usr/lib/systemd/user/linux-onedrived.service"
install -Dm644 data/dbus-1/com.airdataiot.LinuxOneDrive.Daemon.service "$STAGE/usr/share/dbus-1/services/com.airdataiot.LinuxOneDrive.Daemon.service"
install -Dm644 data/applications/com.airdataiot.LinuxOneDrive.desktop "$STAGE/usr/share/applications/com.airdataiot.LinuxOneDrive.desktop"
install -Dm644 data/autostart/com.airdataiot.LinuxOneDrive.desktop "$STAGE/etc/xdg/autostart/com.airdataiot.LinuxOneDrive.desktop"
# Lets the sign-in page's sandbox start on Ubuntu 24.04 and newer (see the file).
install -Dm644 data/apparmor/linux-onedrive "$STAGE/etc/apparmor.d/linux-onedrive"
install -Dm644 data/metainfo/com.airdataiot.LinuxOneDrive.metainfo.xml "$STAGE/usr/share/metainfo/com.airdataiot.LinuxOneDrive.metainfo.xml"
(cd data/icons && find hicolor -name '*.svg' -print0 | while IFS= read -r -d '' f; do
  install -Dm644 "$f" "$STAGE/usr/share/icons/$f"
done)
install -Dm644 integration/nautilus/linux_onedrive.py "$STAGE/usr/share/nautilus-python/extensions/linux_onedrive.py"
install -Dm644 integration/kde/linux-onedrive.desktop "$STAGE/usr/share/kio/servicemenus/linux-onedrive.desktop"
DOC="$STAGE/usr/share/doc/$PKG"
install -Dm644 README.md "$DOC/README.md"
install -Dm644 docs/USER_GUIDE.md "$DOC/USER_GUIDE.md"
cat > "$DOC/copyright" <<COPYRIGHT
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: linux-onedrive
Source: https://github.com/haimairdataiot/LinuxOneDriveClient

Files: *
Copyright: $(date +%Y) Linux OneDrive contributors
License: GPL-3.0-or-later
 On Debian systems, the complete text of the GNU General Public License
 version 3 can be found in "/usr/share/common-licenses/GPL-3".
COPYRIGHT
printf '%s (%s-%s) unstable; urgency=medium\n\n  * Release %s.\n\n -- %s  %s\n' \
  "$PKG" "$VERSION" "$REVISION" "$VERSION" "$MAINTAINER" "$(date -R)" | gzip -9n > "$DOC/changelog.Debian.gz"

echo "==> computing dependencies"
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/debian"
printf 'Source: %s\n\nPackage: %s\nArchitecture: any\n' "$PKG" "$PKG" > "$WORK/debian/control"
SHLIBS="$(cd "$WORK" && dpkg-shlibdeps -O "$STAGE/usr/bin/linux-onedrived" "$STAGE/usr/bin/linux-onedrive" "$STAGE/usr/bin/linux-onedrivectl" 2>/dev/null | sed -n 's/^shlibs:Depends=//p')"
INSTALLED_SIZE="$(du -sk "$STAGE" | cut -f1)"

mkdir -p "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<CONTROL
Package: $PKG
Version: $VERSION-$REVISION
Architecture: $ARCH
Maintainer: $MAINTAINER
Installed-Size: $INSTALLED_SIZE
Depends: ${SHLIBS}, fuse3, dbus-user-session
Recommends: python3-nautilus, gnome-shell-extension-appindicator | gnome-shell-extension-ubuntu-appindicators, gnome-keyring, xdg-utils
Section: net
Priority: optional
Homepage: https://github.com/haimairdataiot/LinuxOneDriveClient
Description: OneDrive client with Files On-Demand (unofficial)
 Linux OneDrive brings your whole Microsoft OneDrive to a folder on your
 computer. Files appear instantly and are downloaded only when you open
 them, like Files On-Demand on Windows. A background service syncs changes
 both ways, keeps both versions when a file changed in two places, and
 holds accidental mass deletions until you confirm.
 .
 Includes a GNOME app with a tray icon, sign-in, settings and activity
 view, file-manager integration for Files (Nautilus) and Dolphin, and a
 command-line tool.
 .
 Not affiliated with or endorsed by Microsoft.
CONTROL

cat > "$STAGE/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    # Start the sync service in every graphical session from now on.
    systemctl --global enable linux-onedrived.service >/dev/null 2>&1 || true
    # Load the AppArmor profile now, so sign-in works without a restart.
    if command -v apparmor_parser >/dev/null 2>&1 && aa-enabled --quiet 2>/dev/null; then
        apparmor_parser -r -T -W /etc/apparmor.d/linux-onedrive >/dev/null 2>&1 || true
    fi
fi
exit 0
POSTINST
cat > "$STAGE/DEBIAN/prerm" <<'PRERM'
#!/bin/sh
set -e
if [ "$1" = "remove" ]; then
    systemctl --global disable linux-onedrived.service >/dev/null 2>&1 || true
    if command -v apparmor_parser >/dev/null 2>&1 && aa-enabled --quiet 2>/dev/null; then
        apparmor_parser -R /etc/apparmor.d/linux-onedrive >/dev/null 2>&1 || true
    fi
fi
exit 0
PRERM
cat > "$STAGE/DEBIAN/postrm" <<'POSTRM'
#!/bin/sh
set -e
if [ "$1" = "purge" ]; then
    echo "linux-onedrive: per-user data in ~/.local/share/linux-onedrive and settings in ~/.config/linux-onedrive were left in place." >&2
fi
exit 0
POSTRM
chmod 755 "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/prerm" "$STAGE/DEBIAN/postrm"

echo "==> building package"
dpkg-deb --root-owner-group -Zxz --build "$STAGE" "$OUT" >/dev/null
echo "built $OUT"
# Checksums for the release (the in-app updater verifies downloads against this file).
(cd "$ROOT/dist" && sha256sum "$(basename "$OUT")" > SHA256SUMS)
echo "wrote $ROOT/dist/SHA256SUMS"
if command -v lintian >/dev/null; then lintian --no-tag-display-limit "$OUT" || true; fi
