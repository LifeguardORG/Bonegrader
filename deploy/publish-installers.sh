#!/usr/bin/env bash
#
# Download the installers from the latest PUBLISHED GitHub release and upload
# them to the Bonegrader download host under the stable names the landing page
# probes for. Run this once a release has been published.
#
# Usage: deploy/publish-installers.sh <ssh-host> [remote-base]
# Example: deploy/publish-installers.sh root@62.171.170.74 /srv/bonegrader
#
set -euo pipefail

SSH_HOST="${1:?usage: publish-installers.sh <ssh-host> [remote-base]}"
REMOTE_BASE="${2:-/srv/bonegrader}"
REPO="LifeguardORG/Bonegrader"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo ">> fetching latest release of $REPO"
python3 - "$REPO" "$tmp" <<'PY'
import json, sys, fnmatch, os
from urllib.request import Request, urlopen

def fetch(url):
    return urlopen(Request(url, headers={"User-Agent": "bonegrader-deploy"}))

repo, tmp = sys.argv[1], sys.argv[2]
rel = json.load(fetch(f"https://api.github.com/repos/{repo}/releases/latest"))

# stable target name  <-  first release asset matching one of the patterns
rules = [
    ("bonegrader-windows-setup.exe", ["*-setup.exe"]),
    ("bonegrader-windows.msi",       ["*.msi"]),
    ("bonegrader-macos.dmg",         ["*.dmg"]),
    ("bonegrader-linux.AppImage",    ["*.AppImage"]),
    ("bonegrader-linux.deb",         ["*.deb"]),
]
assets = rel.get("assets", [])
if not assets:
    sys.exit(f"release '{rel.get('tag_name')}' has no assets yet (still building, or still a draft)")

for target, patterns in rules:
    match = next((a for a in assets if any(fnmatch.fnmatch(a["name"], p) for p in patterns)), None)
    if not match:
        print(f"   (skip {target}: no matching asset)")
        continue
    print(f"   {match['name']}  ->  {target}")
    with fetch(match["browser_download_url"]) as r, open(os.path.join(tmp, target), "wb") as f:
        f.write(r.read())
PY

echo ">> uploading to $SSH_HOST:$REMOTE_BASE/download/"
ssh "$SSH_HOST" "mkdir -p '$REMOTE_BASE/download'"
rsync -a "$tmp"/ "$SSH_HOST:$REMOTE_BASE/download/"
echo ">> done — the download page now offers every uploaded system."
