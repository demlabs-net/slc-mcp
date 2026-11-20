#!/usr/bin/env bash
# Publish a .deb into the DemLabs apt repo (aptly on zeus, served as
# deb.zeus.demlabs). Replaces an existing package with the same name+version.
# Usage: ci/publish-deb.sh <package-name> <path/to/file.deb>
set -euo pipefail

pkg="${1:?package name}"
file="${2:?deb file}"
aptly="${APTLY_URL:-http://172.17.0.1:8085}"
repo=demlabs

command -v dpkg-deb >/dev/null || { echo "dpkg-deb missing" >&2; exit 1; }
ver=$(dpkg-deb -f "$file" Version)

# drop same name+version if re-publishing
old=$(curl -sf "$aptly/api/repos/$repo/packages" \
  | python3 -c '
import sys, json
name, ver = sys.argv[1], sys.argv[2]
# aptly package key format: "P<arch> <name> <version> <hash>"
for key in json.load(sys.stdin):
    parts = key.split(" ")
    if len(parts) >= 3 and parts[1] == name and parts[2] == ver:
        print(key)
' "$pkg" "$ver")
if [ -n "${old:-}" ]; then
  enc=$(python3 -c 'import sys,urllib.parse;print(urllib.parse.quote(sys.argv[1]))' "$old")
  curl -sf -X DELETE "$aptly/api/repos/$repo/packages/$enc" >/dev/null
fi

key=$(curl -sf -F "file=@$file" "$aptly/api/package/upload" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)[0]["Package"]["Key"])')
curl -sf -X POST -H "Content-Type: application/json" \
  -d "{\"PackageFiles\": [\"$key\"]}" "$aptly/api/repos/$repo/packages" >/dev/null
curl -sf -X PUT -H "Content-Type: application/json" \
  -d '{"Distribution":"stable","SourceKind":"local","skip_signing":true}' \
  "$aptly/api/repos/$repo/publish" >/dev/null

echo "published $pkg $ver to $repo (re-publish stable/main)"
