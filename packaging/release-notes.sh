#!/usr/bin/env bash
# Print the CHANGELOG.md section for a version, for use as release notes:
#   packaging/release-notes.sh 0.2.0
set -euo pipefail
version="${1:?usage: packaging/release-notes.sh <version>}"
root="$(cd "$(dirname "$0")/.." && pwd)"
notes="$(awk -v v="$version" '
  /^## / { on = ($2 == v || $2 == "[" v "]"); next }
  on { print }
' "$root/CHANGELOG.md" | sed -e '/./,$!d' | tac | sed -e '/./,$!d' | tac)"
if [ -z "$notes" ]; then
  echo "CHANGELOG.md has no section for version $version" >&2
  exit 1
fi
printf '%s\n' "$notes"
