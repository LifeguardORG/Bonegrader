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
key-only login rather than `root`:

```bash
# on the server, once
adduser --disabled-password --gecos "" deploy
install -d -m 700 -o deploy -g deploy /home/deploy/.ssh
cat your_key.pub >> /home/deploy/.ssh/authorized_keys   # the admin's public SSH key
chown deploy:deploy /home/deploy/.ssh/authorized_keys && chmod 600 /home/deploy/.ssh/authorized_keys
install -d -o deploy -g deploy /srv/bonegrader
```

Then, in `/etc/ssh/sshd_config`: `PasswordAuthentication no` and
`PermitRootLogin prohibit-password` (or `no`), and `systemctl reload ssh`.
Test `ssh deploy@<server>` in a second terminal before closing the first.

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
and simply retries.

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
   commit it):

   ```bash
   cargo run -p bonegrader-publish -- keygen \
     --secret-out ~/.config/bonegrader/manifest-signing.key \
     --public-out keys/manifest-signing.pub
   ```

2. Publish **signed** (`--sign-key …` or the field in the admin app) and deploy.
   Clients without a key ignore the `.sig`, so nothing changes for them yet.
3. Commit `keys/manifest-signing.pub`, tag a release, hand out the new client.
   From then on these clients refuse unsigned or tampered manifests (with
   `--min-client-version` you can make the old, non-verifying clients update).

Key rotation: add the new public key to `keys/manifest-signing.pub` (one per
line), release, switch `--sign-key` to the new key, remove the old line later.
A lost secret key means: new key, new client release.

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
  rolled back. Players can also undo the last update locally
  ("Letztes Update rückgängig" in the app, `bonegrader undo` in the CLI).
* The web app is untouched; to remove Bonegrader just delete the Caddy block +
  mount and reload.
