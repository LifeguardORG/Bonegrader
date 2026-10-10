#!/usr/bin/env bash
#
# Set up Bonegrader's self-updates. Run once on the admin's machine, inside
# the repository checkout:
#
#  1. creates the updater signing key ~/.tauri/bonegrader-updater.key with a
#     random password next to it (both owner-only) — BACK BOTH UP: without
#     them, installed apps can never be updated automatically again
#  2. writes the public key into app/src-tauri/tauri.conf.json and switches on
#     the signed update packages (bundle > createUpdaterArtifacts)
#  3. stores key and password as the GitHub Actions secrets
#     TAURI_SIGNING_PRIVATE_KEY and TAURI_SIGNING_PRIVATE_KEY_PASSWORD (with the
#     GitHub CLI `gh`; otherwise it tells you what to paste where)
#
# Afterwards: commit tauri.conf.json, then deploy/release.sh. Apps
# from that release on update themselves when a newer release is published.
# Running it again reuses the existing key.
#
# Usage: deploy/setup-updater.sh [--key FILE] [--no-gh]
#
set -euo pipefail

KEY="$HOME/.tauri/bonegrader-updater.key"
USE_GH=1

die() { echo "error: $*" >&2; exit 1; }
step() { echo ">> $*"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --key) KEY="${2:?--key needs a file}"; shift 2 ;;
    --no-gh) USE_GH=0; shift ;;
    -h|--help) sed -n '2,23p' "$0"; exit 0 ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CONF="$ROOT/app/src-tauri/tauri.conf.json"
[ -f "$CONF" ] || die "run this from the Bonegrader repository ($CONF missing)"
command -v python3 >/dev/null || die "python3 is needed to edit $CONF"
PWFILE="$KEY.password"

# 1. key
if [ -f "$KEY" ] && [ -f "$KEY.pub" ]; then
  [ -f "$PWFILE" ] || die "$KEY exists, but its password file $PWFILE is missing"
  step "using the existing key $KEY"
  PW="$(cat "$PWFILE")"
else
  cargo tauri --version >/dev/null 2>&1 \
    || die "the Tauri CLI is missing: cargo install tauri-cli --version '^2' --locked"
  step "creating the updater key $KEY"
  mkdir -p "$(dirname "$KEY")"
  chmod 700 "$(dirname "$KEY")"
  PW="$(python3 -c 'import secrets; print(secrets.token_urlsafe(32))')"
  (umask 077; printf '%s' "$PW" > "$PWFILE")
  cargo tauri signer generate --ci -p "$PW" -w "$KEY" >/dev/null
  chmod 600 "$KEY" "$PWFILE"
fi

# 2. app config
step "writing the public key into $CONF"
python3 - "$CONF" "$KEY.pub" <<'PY'
import json, sys
conf, pub = sys.argv[1], sys.argv[2]
with open(conf, encoding="utf-8") as f:
    c = json.load(f)
c.setdefault("bundle", {})["createUpdaterArtifacts"] = True
updater = c.setdefault("plugins", {}).setdefault("updater", {})
updater["pubkey"] = open(pub, encoding="utf-8").read().strip()
updater.setdefault("endpoints", ["https://github.com/LifeguardORG/Bonegrader/releases/latest/download/latest.json"])
with open(conf, "w", encoding="utf-8") as f:
    f.write(json.dumps(c, indent=2, ensure_ascii=False) + "\n")
PY

# 3. GitHub secrets
if [ "$USE_GH" -eq 1 ] && command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
  step "storing the GitHub secrets (gh)"
  (cd "$ROOT" && gh secret set TAURI_SIGNING_PRIVATE_KEY < "$KEY")
  (cd "$ROOT" && printf '%s' "$PW" | gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD)
else
  echo
  echo "   Add two secrets on GitHub (repository > Settings > Secrets and variables >"
  echo "   Actions > New repository secret):"
  echo "     TAURI_SIGNING_PRIVATE_KEY           = the content of $KEY"
  echo "     TAURI_SIGNING_PRIVATE_KEY_PASSWORD  = the content of $PWFILE"
fi

echo
echo ">> done. Next:"
echo "   1. Back up $KEY and $PWFILE (password manager, USB stick)."
echo "   2. git add app/src-tauri/tauri.conf.json && git commit -m 'Enable self-updates'"
echo "   3. git push, then deploy/release.sh (checks everything and tags the release)."
