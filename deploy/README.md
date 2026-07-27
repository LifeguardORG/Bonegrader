# Hosting a Bonegrader channel

The client only needs a plain static HTTPS host that serves, per channel:

```
<channel>/manifest.json
<channel>/files/by-hash/<sha1>       # immutable, content-addressed blobs
```

The client base URL is then `https://<domain>/<channel>` (e.g. `.../main`).

Below assumes the existing dockerized Caddy on the Contabo host
(`/var/www/rescue-compete`, Caddy + PHP + MariaDB + PMA). Adjust paths to match.

---

## 1. Build a channel locally

```bash
cargo run -p bonegrader-publish -- build \
  --instance /home/jonas/Documents/curseforge/minecraft/Instances/BonesAndBees \
  --out ./dist/main --channel main
# review the printed diff before shipping; add --gc to prune local orphan blobs
```

This writes `./dist/main/manifest.json` + `./dist/main/files/by-hash/*`.
Manifest URLs are **relative**, so the same build works under any domain.

## 2. DNS

Point a record at the server, e.g.

```
bonegrader.<your-domain>.   A   5.189.166.122
```

## 3. Caddy site block

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

## 4. Deploy the built channel

```bash
deploy/deploy.sh ./dist/main root@5.189.166.122 /srv/bonegrader main
```

Blobs are uploaded additively (`--ignore-existing`); the manifest is swapped in
last, atomically.

## 5. Verify

```bash
curl -fsSL https://bonegrader.<your-domain>/main/manifest.json | head -c 300
# then in the app, set Channel-URL = https://bonegrader.<your-domain>/main
```

## Rollback

* The web app is untouched; to remove Bonegrader just delete the Caddy block +
  mount and reload.
* A bad channel push: re-run the publish/deploy for the previous state, or keep
  a copy of the prior `manifest.json` and `mv` it back (old blobs are still
  present because deploys never delete them).
