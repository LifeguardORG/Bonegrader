#!/usr/bin/env bash
#
# Deploy a built Bonegrader channel to the static host.
#
# Blobs under files/by-hash/ are content-addressed and immutable, so they are
# only ever *added* (never overwritten/deleted here) — a client mid-update can
# still fetch an old blob, and every older manifest stays valid. The live
# manifest (+ signature) is copied to history/ first, so `rollback.sh` can
# switch back. The new signature and manifest are uploaded under temporary
# names and switched with one remote command, signature first: a client caught
# in between sees a mismatched pair once and simply retries.
#
# Usage:
#   deploy/deploy.sh <channel-dir> <ssh-host> <remote-base> [channel-name]
#
# Example:
#   deploy/deploy.sh ./dist/main deploy@bonegrader.example.com /srv/bonegrader main
#
set -euo pipefail

CHANNEL_DIR="${1:?usage: deploy.sh <channel-dir> <ssh-host> <remote-base> [channel]}"
SSH_HOST="${2:?missing ssh-host, e.g. deploy@bonegrader.example.com}"
REMOTE_BASE="${3:?missing remote-base, e.g. /srv/bonegrader}"
CHANNEL="${4:-main}"
DEST="${REMOTE_BASE%/}/$CHANNEL"
STAMP="$(date -u +%Y%m%d-%H%M%S)"

# Nothing a remote shell would interpret (the values end up in ssh commands).
for v in "$SSH_HOST" "$REMOTE_BASE" "$CHANNEL"; do
  if [[ ! "$v" =~ ^[A-Za-z0-9@._/~:-]+$ || "$v" == -* ]]; then
    echo "error: unsupported characters in '$v'" >&2
    exit 1
  fi
done

command -v rsync >/dev/null || { echo "error: rsync not found locally" >&2; exit 1; }
[ -f "$CHANNEL_DIR/manifest.json" ] || { echo "error: no manifest.json in $CHANNEL_DIR" >&2; exit 1; }
[ -d "$CHANNEL_DIR/files/by-hash" ] || { echo "error: no files/by-hash in $CHANNEL_DIR" >&2; exit 1; }

echo ">> ensuring remote directory: $DEST"
ssh "$SSH_HOST" "mkdir -p $DEST/files/by-hash $DEST/history"

echo ">> uploading blobs (immutable, additive)"
# --ignore-existing: never re-send or clobber an existing content-addressed blob.
rsync -av --ignore-existing "$CHANNEL_DIR/files/" "$SSH_HOST:$DEST/files/"

echo ">> uploading manifest"
rsync -av "$CHANNEL_DIR/manifest.json" "$SSH_HOST:$DEST/manifest.json.tmp"
SWITCH=""
if [ -f "$CHANNEL_DIR/manifest.json.sig" ]; then
  rsync -av "$CHANNEL_DIR/manifest.json.sig" "$SSH_HOST:$DEST/manifest.json.sig.tmp"
  SWITCH="mv -f $DEST/manifest.json.sig.tmp $DEST/manifest.json.sig && "
else
  echo "   note: no manifest.json.sig — this channel is unsigned"
fi

echo ">> keeping the previous manifest in history/, switching to the new one"
ssh "$SSH_HOST" "if [ -f $DEST/manifest.json ]; then cp -p $DEST/manifest.json $DEST/history/manifest-$STAMP.json; fi; \
if [ -f $DEST/manifest.json.sig ]; then cp -p $DEST/manifest.json.sig $DEST/history/manifest-$STAMP.json.sig; fi; \
${SWITCH}mv -f $DEST/manifest.json.tmp $DEST/manifest.json"

echo ">> done: channel '$CHANNEL' is live at $DEST"
echo "   client base URL:  https://<your-domain>/$CHANNEL"
echo "   undo this deploy: deploy/rollback.sh $SSH_HOST $REMOTE_BASE $CHANNEL"
