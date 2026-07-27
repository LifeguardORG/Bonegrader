#!/usr/bin/env bash
#
# Deploy a built Bonegrader channel to the static host.
#
# Blobs under files/by-hash/ are content-addressed and immutable, so they are
# only ever *added* (never overwritten/deleted here) — a client mid-update can
# still fetch an old blob. The manifest is uploaded last and swapped atomically,
# so the channel flips over in one step.
#
# Usage:
#   deploy/deploy.sh <channel-dir> <ssh-host> <remote-base> [channel-name]
#
# Example:
#   deploy/deploy.sh ./dist/main root@5.189.166.122 /srv/bonegrader main
#
set -euo pipefail

CHANNEL_DIR="${1:?usage: deploy.sh <channel-dir> <ssh-host> <remote-base> [channel]}"
SSH_HOST="${2:?missing ssh-host, e.g. root@5.189.166.122}"
REMOTE_BASE="${3:?missing remote-base, e.g. /srv/bonegrader}"
CHANNEL="${4:-main}"
DEST="$REMOTE_BASE/$CHANNEL"

command -v rsync >/dev/null || { echo "error: rsync not found locally" >&2; exit 1; }
[ -f "$CHANNEL_DIR/manifest.json" ] || { echo "error: no manifest.json in $CHANNEL_DIR" >&2; exit 1; }
[ -d "$CHANNEL_DIR/files/by-hash" ] || { echo "error: no files/by-hash in $CHANNEL_DIR" >&2; exit 1; }

echo ">> ensuring remote directory: $DEST"
ssh "$SSH_HOST" "mkdir -p '$DEST/files/by-hash'"

echo ">> uploading blobs (immutable, additive)"
# --ignore-existing: never re-send or clobber an existing content-addressed blob.
rsync -av --ignore-existing "$CHANNEL_DIR/files/" "$SSH_HOST:$DEST/files/"

echo ">> uploading manifest (atomic switch)"
rsync -av "$CHANNEL_DIR/manifest.json" "$SSH_HOST:$DEST/manifest.json.tmp"
ssh "$SSH_HOST" "mv -f '$DEST/manifest.json.tmp' '$DEST/manifest.json'"

echo ">> done: channel '$CHANNEL' is live at $DEST"
echo "   client base URL:  https://<your-domain>/$CHANNEL"
