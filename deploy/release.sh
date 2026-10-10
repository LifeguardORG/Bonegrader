#!/usr/bin/env bash
#
# Release a new Bonegrader version from the admin's checkout: check everything
# the release depends on, then create and push the tag. GitHub Actions then
# builds the installers into a draft release.
#
# Checks (✗ stops, ! asks):
#  - on main, nothing uncommitted, identical to GitHub
#  - the same version in Cargo.toml and web/index.html, higher than every
#    existing tag, and its tag not taken yet
#  - keys/manifest-signing.pub holds a key, and the live channel manifest is
#    signed with it: this version refuses unsigned or foreign-signed
#    manifests, so publish once with the admin app (signature key set) first
#  - self-updates set up: updater key in app/src-tauri/tauri.conf.json and,
#    when the GitHub CLI `gh` is logged in, the two GitHub secrets
#
# Usage: deploy/release.sh [--channel-url URL] [--check]
#   --channel-url URL  live channel whose signature is checked
#                      (default: https://bonegrader.rescue-compete.de/main)
#   --check            only check, tag nothing
#
set -euo pipefail

CHANNEL_URL="https://bonegrader.rescue-compete.de/main"
CHECK_ONLY=0
REPO="LifeguardORG/Bonegrader"

die() { echo "error: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --channel-url) CHANNEL_URL="${2:?--channel-url needs a URL}"; shift 2 ;;
    --check) CHECK_ONLY=1; shift ;;
    -h|--help) sed -n '2,21p' "$0"; exit 0 ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
command -v python3 >/dev/null || die "python3 is needed"

failed=0
doubts=0
ok() { echo "  ✓ $*"; }
bad() { echo "  ✗ $*"; failed=1; }
doubt() { echo "  ! $*"; doubts=1; }

echo ">> Git"
git fetch -q origin main --tags || die "git fetch failed"
branch="$(git rev-parse --abbrev-ref HEAD)"
if [ "$branch" = main ]; then ok "on main"; else bad "on branch $branch: release from main (git switch main && git pull)"; fi
if [ -z "$(git status --porcelain)" ]; then
  ok "nothing uncommitted"
else
  bad "uncommitted changes (git status): commit and push them first"
fi
if [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ]; then
  ok "identical to GitHub"
else
  bad "differs from origin/main: git pull, or git push your commits first"
fi

echo ">> Version"
ver="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"
web="$(sed -n 's/.*const VERSION = "\(.*\)";.*/\1/p' web/index.html)"
tag="v$ver"
if [ "$ver" = "$web" ]; then
  ok "Cargo.toml and web/index.html: $ver"
else
  bad "Cargo.toml says $ver, web/index.html says $web: set both to the new version"
fi
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null \
  || git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1; then
  bad "tag $tag exists already: raise the version, or remove the tag (git push origin --delete $tag; git tag -d $tag)"
else
  ok "tag $tag is free"
fi
newest="$(git tag -l 'v[0-9]*' | sed 's/^v//' | sort -V | tail -n 1)"
if [ -n "$newest" ] && [ "$newest" != "$ver" ] \
  && [ "$(printf '%s\n%s\n' "$newest" "$ver" | sort -V | tail -n 1)" != "$ver" ]; then
  bad "$ver is lower than the existing tag v$newest: installed apps would not update to it"
fi

echo ">> Manifest signature"
if grep -qvE '^[[:space:]]*(#|$)' keys/manifest-signing.pub; then
  ok "keys/manifest-signing.pub holds a key"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  mkdir -p "$tmp/mods"
  echo "     checking $CHANNEL_URL with this version's keys (builds the player CLI once) …"
  if out="$(cargo run -q -p bonegrader-cli -- plan --instance "$tmp" --base-url "$CHANNEL_URL" 2>&1)"; then
    if grep -q "signature verified" <<<"$out"; then
      ok "the live manifest is signed with it"
    else
      bad "could not confirm the live manifest's signature: $(tail -n 1 <<<"$out")"
    fi
  else
    bad "players on $ver would get no pack updates: $(tail -n 1 <<<"$out")"
    echo "     Fix: admin app > Erweitert > signature key set > publish once, then run this again."
  fi
else
  doubt "keys/manifest-signing.pub holds no key: $ver will not check manifest signatures (admin app: Erweitert > Neuen Schlüssel erzeugen, then commit the file)"
fi

echo ">> Self-updates"
read -r pubkey artifacts < <(python3 - app/src-tauri/tauri.conf.json <<'PY'
import json, sys
c = json.load(open(sys.argv[1], encoding="utf-8"))
key = (c.get("plugins", {}).get("updater", {}).get("pubkey") or "").strip()
print("yes" if key else "no", "yes" if c.get("bundle", {}).get("createUpdaterArtifacts") is True else "no")
PY
)
if [ "$pubkey" = yes ] && [ "$artifacts" = yes ]; then
  ok "updater key in app/src-tauri/tauri.conf.json"
  if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    secrets="$(gh secret list --repo "$REPO" 2>/dev/null | awk '{print $1}')"
    for s in TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD; do
      if grep -qx "$s" <<<"$secrets"; then ok "GitHub secret $s"; else bad "GitHub secret $s missing (deploy/setup-updater.sh)"; fi
    done
  else
    echo "     (GitHub secrets not checked: gh is missing or not logged in; the release workflow checks them)"
  fi
else
  doubt "self-updates are not set up (deploy/setup-updater.sh): players on $ver will have to download the next version by hand"
fi

echo
[ "$failed" -eq 0 ] || die "not released: fix the ✗ points above and run this again"
if [ "$CHECK_ONLY" -eq 1 ]; then
  echo ">> all checks done (--check: nothing tagged)"
  exit 0
fi
if [ "$doubts" -ne 0 ]; then
  prompt="Release $tag despite the ! points? [y/N] "
else
  prompt="Create and push the tag $tag? [y/N] "
fi
read -r -p "$prompt" answer
case "$answer" in y|Y|j|J|yes|ja) ;; *) echo "nothing tagged"; exit 1 ;; esac

git tag -a "$tag" -m "Bonegrader $ver"
git push origin "refs/tags/$tag"

cat <<EOF

>> $tag pushed. Next:
   1. Wait for the build (about 15-25 minutes):
      https://github.com/$REPO/actions
   2. Check the draft release (installers for Windows, macOS, Linux and
      latest.json) and click "Publish release":
      https://github.com/$REPO/releases
      Only then do installed apps see the update.
   3. Download page: deploy/publish-installers.sh <ssh-host>
   4. Admin app > Erweitert > "Aktuelle Version der Spieler-App" = $ver, publish.
EOF
