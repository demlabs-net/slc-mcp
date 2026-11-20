#!/usr/bin/env bash
# Publish a .deb into the DemLabs apt repo (aptly 1.6 on zeus, served as
# deb.zeus.demlabs). Replaces an existing package with the same name+version,
# then re-publishes stable/main.
# Usage: ci/publish-deb.sh <package-name> <path/to/file.deb>
set -euo pipefail

pkg="${1:?package name}"
file="${2:?deb file}"
aptly="${APTLY_URL:-http://172.17.0.1:8085}"
repo=demlabs

command -v dpkg-deb >/dev/null || { echo "dpkg-deb missing" >&2; exit 1; }
ver=$(dpkg-deb -f "$file" Version)

# 1) drop same name+version if re-publishing (aptly key: "P<arch> <name> <ver> <hash>")
old=$(curl -sf "$aptly/api/repos/$repo/packages" \
  | python3 -c '
import sys, json
name, ver = sys.argv[1], sys.argv[2]
for key in json.load(sys.stdin):
    parts = key.split(" ")
    if len(parts) >= 3 and parts[1] == name and parts[2] == ver:
        print(key)
' "$pkg" "$ver")
if [ -n "${old:-}" ]; then
  curl -sf -X DELETE -H "Content-Type: application/json" \
    -d "{\"PackageRefs\": [\"$old\"]}" "$aptly/api/repos/$repo/packages" >/dev/null
fi

# 2) upload into the files pool; response is ["pool/<basename>"]
up=$(curl -sf -F "file=@$file" "$aptly/api/files/pool" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)[0].split("/", 1)[1])')

# 3) add the uploaded file to the repo — a FailedFiles entry is a hard error
curl -sf -X POST "$aptly/api/repos/$repo/file/pool/$up" \
  | python3 -c 'import sys,json;d=json.load(sys.stdin);sys.exit("failed: %s" % d["FailedFiles"] if d["FailedFiles"] else 0)'

# 4) re-publish; the published repo has prefix "." which the API routes as an
#    empty segment, hence the double slash
curl -sf -X POST -H "Content-Type: application/json" -d '{"Signing": {"Skip": true}}' "$aptly/api/publish//stable/update" >/dev/null

# 5) tidy the upload pool
curl -sf -X DELETE "$aptly/api/files/pool/$up" >/dev/null

echo "published $pkg $ver to $repo (re-published stable/main)"
