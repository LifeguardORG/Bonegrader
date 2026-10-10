# Hosting a Bonegrader channel

The client only needs a plain static HTTPS host that serves, per channel:

```
<channel>/manifest.json
<channel>/manifest.json.sig          # optional: Ed25519 signature (see 7.)
<channel>/files/by-hash/<sha1>       # immutable, content-addressed blobs
<channel>/history/                   # earlier manifests, for rollback (see 10.)
```

The client base URL is then `https://<domain>/<channel>` (e.g. `.../main`).
Clients only talk HTTPS (plain HTTP is accepted for `localhost` testing only).

Below assumes a dockerized Caddy on the host (Caddy + PHP + MariaDB + PMA in
`/var/www/rescue-compete`). Adjust paths to match.

---

## 1. Build a channel locally

```bash
cargo run --release -p bonegrader-publish -- build \
  --instance ~/curseforge/minecraft/Instances/BonesAndBees \
  --out ./dist/main --channel main \
  --sign-key ~/.config/bonegrader/manifest-signing.key   # see 7.
# review the printed diff before shipping; add --gc to prune local orphan blobs
```

This writes `./dist/main/manifest.json` (+ `.sig`) and `./dist/main/files/by-hash/*`.
Manifest URLs are **relative**, so the same build works under any domain.

The build refuses packs that would break every client — e.g. two jars that
declare the same modId (the game would not start) — and warns about identical
files under several names.

Useful options (all optional):

| Option | Effect on the players' side |
| --- | --- |
| `--server-name X --server-address play.example.com` | adds the server to the in-game multiplayer list (once) |
| `--seed config/jei` / `--seed options.txt` | default configs, created only if the player has none (allowed: `options.txt`, `config/`, `defaultconfigs/`, `kubejs/`) |
| `--latest-client-version 1.2.0 --client-download-url https://…/` | "Bonegrader 1.2.0 ist verfügbar" with a download link |
| `--min-client-version 1.2.0` | older Bonegrader versions refuse to update and point to the download |
| `--ignore mods/dev-only.jar` | leave a file out of the pack |

The admin app (`admin/`) offers the same options in a form.

## 2. A deploy user (instead of root)

Deploys only need write access to `/srv/bonegrader`. Use a dedicated user with
key-only login rather than `root`. [`setup-server.sh`](./setup-server.sh) does
it in one go (run it again any time; it keeps what exists):

```bash
# from the admin's machine (no SSH key yet? ssh-keygen -t ed25519)
scp deploy/setup-server.sh ~/.ssh/id_ed25519.pub root@<server>:/tmp/
ssh root@<server> 'bash /tmp/setup-server.sh --key-file /tmp/id_ed25519.pub'
```

It creates the `deploy` user (no password, no sudo), allows your key, hands
`/srv/bonegrader` to it (files from earlier root deploys included; everything
stays readable for Caddy) and installs rsync if needed.

Then switch off password logins — `--harden-ssh` writes
`/etc/ssh/sshd_config.d/00-bonegrader-hardening.conf` (`PasswordAuthentication
no`, `PermitRootLogin prohibit-password`), checks it with `sshd -t` and reloads
ssh. It refuses while root has no SSH key, so you cannot lock yourself out:

```bash
ssh-copy-id root@<server>        # if root still logs in with a password
ssh root@<server> 'bash /tmp/setup-server.sh --key-file /tmp/id_ed25519.pub --harden-ssh'
```

Test `ssh deploy@<server>` and `ssh root@<server>` in a second terminal before
closing the first. To undo: delete that file and `systemctl reload ssh`.

The admin app never prompts (it has no terminal): it needs key login, and the
server's host key must be known — connect once with `ssh deploy@<server>` and
confirm it. In the app, set **SSH-Ziel** `deploy@<server>` and **Remote-Basis**
`/srv/bonegrader`; **Verbindung testen** then checks login, write access and
rsync on both ends.

## 3. DNS

Point a record at the server, e.g.

```
bonegrader.<your-domain>.   A   <server-ip>
```

## 4. Caddy site block

Add [`Caddyfile.bonegrader`](./Caddyfile.bonegrader) to your Caddyfile (replace
the domain). For the **dockerized** Caddy you also need the channel dir mounted
into the container. In the Caddy service of your compose file:

```yaml
    volumes:
      - /srv/bonegrader:/srv/bonegrader:ro   # host path : container path (read-only)
```

Reload without downtime:

```bash
# from the compose project dir:
docker compose up -d            # if the volume/compose changed
docker compose exec caddy caddy reload --config /etc/caddy/Caddyfile
```

> Safety: only *adds* a new site block + a read-only mount. It does not touch the
> existing web app. Verify the app still responds after `caddy reload`.

## 5. Deploy the built channel

```bash
deploy/deploy.sh ./dist/main deploy@<server> /srv/bonegrader main
```

Blobs are uploaded additively (`--ignore-existing`, never deleted remotely, so
every earlier manifest stays complete). The live manifest is copied to
`history/`, then signature and manifest are switched with one remote command —
signature first; a client caught exactly in between sees a mismatched pair once
and simply retries. An unsigned upload removes the old signature (it would not
match the new manifest anyway).

## 6. Verify

```bash
curl -fsSL https://bonegrader.<your-domain>/main/manifest.json | head -c 300
# or the full client pipeline, without changing anything:
cargo run -p bonegrader-cli -- plan --instance <instanz> --base-url https://bonegrader.<your-domain>/main
```

## 7. Signing the manifest

Bonegrader installs executable code (mods) on the players' machines. With a
signature, taking over the web server is no longer enough to push code to them:
clients only accept a `manifest.json` signed by a key they were built with.

Set it up once, **in this order**:

1. Create the key on the admin's machine (the secret key never leaves it, never
   commit it) — in the admin app under **Erweitert → Neuen Schlüssel
   erzeugen** (run from the checkout, it also adds the public key to
   `keys/manifest-signing.pub`), or on the command line:

   ```bash
   cargo run -p bonegrader-publish -- keygen \
     --secret-out ~/.config/bonegrader/manifest-signing.key \
     --public-out keys/manifest-signing.pub
   ```

   Back up the secret key file (password manager, USB stick).

2. Publish **signed** (`--sign-key …` or the field in the admin app) and deploy.
   Clients without a key ignore the `.sig`, so nothing changes for them yet.
3. Commit `keys/manifest-signing.pub`, raise the version, tag a release, hand
   out the new client (the key is compiled in: clients built before it don't
   check).
   From then on these clients refuse unsigned or tampered manifests (with
   `--min-client-version` you can make the old, non-verifying clients update).

Key rotation: add the new public key to `keys/manifest-signing.pub` (one per
line), release, switch `--sign-key` to the new key, remove the old line later.
A lost secret key means: new key, new client release.

The admin app checks this for you: built from the same version as the player
app, it knows which keys the players trust and refuses to publish a manifest
they would reject (unsigned while a signature is required, or signed with a
key that is not in `keys/manifest-signing.pub`). Publishing unsigned while
clients do not check yet is allowed, with a clear warning.

## 8. Beta channel

Channels are independent folders, so testing before everyone gets it is just a
second channel:

```bash
cargo run --release -p bonegrader-publish -- build --instance <instanz> --out ./dist/beta --channel beta --sign-key …
deploy/deploy.sh ./dist/beta deploy@<server> /srv/bonegrader beta
# testers use https://bonegrader.<your-domain>/beta — when it works:
cargo run --release -p bonegrader-publish -- build --instance <instanz> --out ./dist/main --channel main --sign-key …
deploy/deploy.sh ./dist/main deploy@<server> /srv/bonegrader main
```

## 9. Installers on the download page

```bash
deploy/publish-installers.sh deploy@<server> /srv/bonegrader   # after publishing the GitHub release
deploy/deploy-site.sh deploy@<server> /srv/bonegrader          # landing page (web/)
```

## 10. Rollback

* A bad channel push:

  ```bash
  deploy/rollback.sh deploy@<server> /srv/bonegrader main            # newest history entry
  deploy/rollback.sh deploy@<server> /srv/bonegrader main manifest-20261002-164403.json
  ```

  The replaced manifest is itself saved to `history/`, so a rollback can be
  rolled back. The admin app does the same under **Verlauf & Rollback**: it
  lists the live manifest and every earlier one (build date, files,
  signature) with a button to switch back. Players can also undo the last
  update locally ("Update vom … rückgängig" in the app, `bonegrader undo` in
  the CLI).
* The web app is untouched; to remove Bonegrader just delete the Caddy block +
  mount and reload.
