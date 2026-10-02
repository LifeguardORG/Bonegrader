#!/usr/bin/env bash
#
# Switch a channel back to an earlier manifest from its history/ folder
# (written by deploy.sh and the admin app before every deploy). Blobs are
# never deleted remotely, so every earlier manifest is still complete.
#
# Usage:
#   deploy/rollback.sh <ssh-host> <remote-base> [channel] [history-file]
#
# Without a history file the newest one is used; the current manifest is
# itself saved to history/ first, so a rollback can be rolled back.
#
# Examples:
#   deploy/rollback.sh deploy@bonegrader.example.com /srv/bonegrader main
#   deploy/rollback.sh deploy@bonegrader.example.com /srv/bonegrader main manifest-20261002-164403.json
#
set -euo pipefail

SSH_HOST="${1:?usage: rollback.sh <ssh-host> <remote-base> [channel] [history-file]}"
REMOTE_BASE="${2:?missing remote-base, e.g. /srv/bonegrader}"
CHANNEL="${3:-main}"
PICK="${4:-}"
DEST="${REMOTE_BASE%/}/$CHANNEL"
STAMP="$(date -u +%Y%m%d-%H%M%S)"

for v in "$SSH_HOST" "$REMOTE_BASE" "$CHANNEL" "$PICK"; do
  if [[ -n "$v" && ( ! "$v" =~ ^[A-Za-z0-9@._/~:-]+$ || "$v" == -* ) ]]; then
    echo "error: unsupported characters in '$v'" >&2
    exit 1
  fi
done

if [ -z "$PICK" ]; then
  PICK="$(ssh "$SSH_HOST" "ls -1 $DEST/history/ 2>/dev/null | grep -E '^manifest-[0-9-]+\.json$' | sort | tail -n 1")"
  [ -n "$PICK" ] || { echo "error: no history in $DEST/history" >&2; exit 1; }
fi
echo ">> rolling '$CHANNEL' back to $PICK"

ssh "$SSH_HOST" "set -e; cd $DEST; \
[ -f history/$PICK ] || { echo 'no such history entry: $PICK' >&2; exit 1; }; \
cp -p manifest.json history/manifest-$STAMP.json; \
if [ -f manifest.json.sig ]; then cp -p manifest.json.sig history/manifest-$STAMP.json.sig; fi; \
cp -p history/$PICK manifest.json.tmp; \
if [ -f history/$PICK.sig ]; then cp -p history/$PICK.sig manifest.json.sig.tmp && mv -f manifest.json.sig.tmp manifest.json.sig; \
else rm -f manifest.json.sig; fi; \
mv -f manifest.json.tmp manifest.json"

echo ">> done (the replaced manifest was saved as history/manifest-$STAMP.json)"
