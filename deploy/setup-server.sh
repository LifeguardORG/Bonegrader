#!/usr/bin/env bash
#
# One-time server setup for Bonegrader deploys. Run as root ON the server.
#
#  1. creates the `deploy` user (no password, key login only, no sudo)
#  2. installs the admin's public SSH key for it
#  3. hands it the channel folder (default /srv/bonegrader), readable for the
#     web server
#  4. installs rsync if it is missing
#  5. optional (--harden-ssh): no password logins any more, root only with a
#     key — checked with `sshd -t` before ssh is reloaded
#
# Safe to run again: existing users, keys and settings are kept.
#
# Usage (from the admin's machine):
#   scp deploy/setup-server.sh ~/.ssh/id_ed25519.pub root@<server>:/tmp/
#   ssh root@<server> 'bash /tmp/setup-server.sh --key-file /tmp/id_ed25519.pub'
#   ssh root@<server> 'bash /tmp/setup-server.sh --key-file /tmp/id_ed25519.pub --harden-ssh'
#
# Options:
#   --key-file FILE   public key to allow (one line, e.g. ~/.ssh/id_ed25519.pub)
#   --key "LINE"      the same, given directly
#   --user NAME       deploy user (default: deploy)
#   --base DIR        folder holding the channels (default: /srv/bonegrader)
#   --harden-ssh      also disable password logins (see above)
#
set -euo pipefail

USER_NAME="deploy"
BASE="/srv/bonegrader"
KEY=""
HARDEN=0

die() { echo "error: $*" >&2; exit 1; }
step() { echo ">> $*"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --key-file) [ -f "${2:-}" ] || die "key file not found: ${2:-}"; KEY="$(head -n 1 "$2")"; shift 2 ;;
    --key) KEY="${2:-}"; shift 2 ;;
    --user) USER_NAME="${2:-}"; shift 2 ;;
    --base) BASE="${2:-}"; shift 2 ;;
    --harden-ssh) HARDEN=1; shift ;;
    -h|--help) sed -n '2,32p' "$0"; exit 0 ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
done

[ "$(id -u)" -eq 0 ] || die "run as root (sudo bash $0 ...)"
[ -n "$KEY" ] || die "no public key given (--key-file ~/.ssh/id_ed25519.pub)"
case "$KEY" in
  ssh-ed25519\ *|ssh-rsa\ *|ecdsa-sha2-*\ *|sk-ssh-ed25519@openssh.com\ *|sk-ecdsa-sha2-*\ *) ;;
  *) die "that does not look like a public SSH key (expected 'ssh-ed25519 AAAA...'); never pass the private key" ;;
esac
[[ "$USER_NAME" =~ ^[a-z_][a-z0-9_-]{0,31}$ ]] || die "invalid user name: $USER_NAME"
# Same rule as the admin app and deploy.sh: nothing a remote shell interprets.
[[ "$BASE" =~ ^/[A-Za-z0-9._/-]+$ ]] || die "base must be an absolute path without spaces or special characters"

# 1. user
if id -u "$USER_NAME" >/dev/null 2>&1; then
  step "user '$USER_NAME' exists"
else
  step "creating user '$USER_NAME'"
  useradd --create-home --shell /bin/bash "$USER_NAME"
fi
passwd -l "$USER_NAME" >/dev/null 2>&1 || true # key login only
HOME_DIR="$(getent passwd "$USER_NAME" | cut -d: -f6)"
GROUP="$(id -gn "$USER_NAME")"

# 2. key
install -d -m 700 -o "$USER_NAME" -g "$GROUP" "$HOME_DIR/.ssh"
AUTH="$HOME_DIR/.ssh/authorized_keys"
touch "$AUTH"
if grep -qxF "$KEY" "$AUTH"; then
  step "key already allowed for '$USER_NAME'"
else
  step "allowing key for '$USER_NAME'"
  printf '%s\n' "$KEY" >> "$AUTH"
fi
chown "$USER_NAME:$GROUP" "$AUTH"
chmod 600 "$AUTH"

# 3. channel folder (earlier deploys as root left root-owned files behind)
step "giving $BASE to '$USER_NAME'"
install -d -m 755 "$BASE"
chown -R "$USER_NAME:$GROUP" "$BASE"
chmod -R u+rwX,go+rX "$BASE" # the web server (Caddy) only reads

# 4. rsync
if command -v rsync >/dev/null 2>&1; then
  step "rsync present"
else
  step "installing rsync"
  if command -v apt-get >/dev/null 2>&1; then
    DEBIAN_FRONTEND=noninteractive apt-get install -y rsync >/dev/null
  elif command -v dnf >/dev/null 2>&1; then
    dnf install -y rsync >/dev/null
  else
    die "please install rsync, then run this again"
  fi
fi

# 5. ssh hardening (optional)
if [ "$HARDEN" -eq 1 ]; then
  command -v sshd >/dev/null 2>&1 || die "sshd not found – cannot harden"
  # Without a key for root, root could no longer log in at all afterwards
  # (the deploy user has no sudo). Refuse instead of locking the admin out.
  if ! grep -qE '^(ssh-|ecdsa-|sk-)' /root/.ssh/authorized_keys 2>/dev/null; then
    die "root has no SSH key yet – first run 'ssh-copy-id root@<server>' from your machine, test it, then use --harden-ssh"
  fi
  if ! grep -qiE '^\s*Include\s+/etc/ssh/sshd_config\.d/\*\.conf' /etc/ssh/sshd_config; then
    die "/etc/ssh/sshd_config has no 'Include /etc/ssh/sshd_config.d/*.conf' – set PasswordAuthentication no and PermitRootLogin prohibit-password there by hand"
  fi
  # sshd uses the first value it reads; "00-" comes before e.g. 50-cloud-init.conf.
  CONF=/etc/ssh/sshd_config.d/00-bonegrader-hardening.conf
  step "writing $CONF"
  cat > "$CONF" <<'EOF'
# Written by Bonegrader's deploy/setup-server.sh: key logins only.
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin prohibit-password
EOF
  if ! sshd -t; then
    rm -f "$CONF"
    die "sshd rejected the configuration – nothing changed"
  fi
  if sshd -T 2>/dev/null | grep -qi '^passwordauthentication yes'; then
    rm -f "$CONF"
    die "another setting re-enables password logins (check /etc/ssh/sshd_config) – nothing changed"
  fi
  step "reloading ssh"
  if ! { systemctl reload ssh || systemctl reload sshd || service ssh reload; } >/dev/null 2>&1; then
    echo "   warning: could not reload ssh – the setting applies from its next restart"
  fi
  echo
  echo "   !! Keep this session open and test in a SECOND terminal first:"
  echo "        ssh $USER_NAME@<server>     and     ssh root@<server>"
  echo "      If something is wrong: rm $CONF && systemctl reload ssh"
fi

echo
echo ">> done. Next, on the admin's machine:"
echo "   1. ssh $USER_NAME@<server> true      (confirm the host key once)"
echo "   2. Admin app: SSH-Ziel '$USER_NAME@<server>', Remote-Basis '$BASE', then 'Verbindung testen'"
