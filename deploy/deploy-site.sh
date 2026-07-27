#!/usr/bin/env bash
#
# Deploy the download landing page (web/index.html) and any installers staged
# in web/download/ to the Bonegrader static host. Pure file upload — touches
# nothing else on the server.
#
# Usage:
#   deploy/deploy-site.sh <ssh-host> [remote-base]
# Example:
#   deploy/deploy-site.sh root@62.171.170.74 /srv/bonegrader
#
# Installer file names the page looks for (put them in web/download/):
#   bonegrader-windows-setup.exe   (and/or bonegrader-windows.msi)
#   bonegrader-macos.dmg
#   bonegrader-linux.AppImage      (and/or bonegrader-linux.deb)
#
set -euo pipefail

SSH_HOST="${1:?usage: deploy-site.sh <ssh-host> [remote-base]}"
REMOTE_BASE="${2:-/srv/bonegrader}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"

[ -f "$REPO/web/index.html" ] || { echo "error: web/index.html not found" >&2; exit 1; }

# Sync the whole web/ dir (index.html, icons, download/) — no --delete, so the
# channel dirs (e.g. main/) alongside it on the server stay untouched.
echo ">> uploading site  (web/ -> $REMOTE_BASE/)"
ssh "$SSH_HOST" "mkdir -p '$REMOTE_BASE'"
rsync -a "$REPO/web/" "$SSH_HOST:$REMOTE_BASE/"

echo ">> done: https://<host>/ serves the download page"
