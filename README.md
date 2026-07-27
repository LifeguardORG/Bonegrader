# Bonegrader

> Update-Software für den Minecraft-Server (BonesAndBees).

Bonegrader hält das lokale Mod-Verzeichnis der Mitspieler automatisch mit dem
Server-Stand synchron: Der Client vergleicht die lokalen Dateien mit einem
Manifest auf dem Server und lädt **nur die Unterschiede** nach — **eigene
Client-Mods der Spieler bleiben dabei unangetastet**. Kein ZIP-Entpacken,
kein Dateien-Verschieben mehr.

## Für Spieler — Installation & Nutzung

1. Installer aus den [Releases](../../releases) laden:
   - **Windows:** `.msi` oder `.exe`
   - **Linux:** `.AppImage` oder `.deb`
   - **macOS:** `.dmg`
2. Bonegrader öffnen. Die Server-Adresse ist bereits vorbelegt.
3. **Instanz wählen** (CurseForge/offizieller Launcher werden erkannt; sonst
   Ordner manuell angeben) → **Prüfen** → **Aktualisieren**.

Bei „Zusatz-Mods" bzw. Konflikten fragt Bonegrader nach, bevor etwas passiert.
Ersetzte/entfernte Dateien landen sicherheitshalber in `.bonegrader-backup/`.

## Wie es funktioniert

- Der Server hält pro Channel ein `manifest.json` + **content-addressed** Blobs
  (`files/by-hash/<sha1>`, unveränderlich).
- Der Client scannt die Instanz, gleicht per SHA-1/`modId` gegen das Manifest ab
  und lädt nur Abweichungen. Verwaltete Dateien werden getrackt, alles andere
  gilt als Eigen-Mod und bleibt.
- Downloads werden erst vollständig verifiziert, dann atomar angewandt
  (verify-before-destroy, Backup, Rollback-fähig).

## Entwicklung

Voraussetzungen: Rust (stable) + Tauri-Systempakete
(`webkit2gtk-4.1`, `librsvg`, `libappindicator` …).

```bash
cargo test                      # alle Crates (Diff-Engine, Apply, Detection, …)
cargo run -p bonegrader-app     # GUI starten (Frontend ist statisch eingebettet)
cargo run --example detect -p bonegrader-client   # erkannte Instanzen anzeigen
```

Headless-Updater (dieselbe Logik ohne GUI):

```bash
cargo run -p bonegrader-cli -- plan   --instance <pfad> --base-url https://bonegrader.rescue-compete.de/main
cargo run -p bonegrader-cli -- update --instance <pfad> --base-url https://bonegrader.rescue-compete.de/main
```

## Channel veröffentlichen (nach Mod-Fixes)

```bash
cargo run --release -p bonegrader-publish -- build \
  --instance <instanz> --out ./dist/main --channel main   # zeigt Diff, --gc räumt auf
deploy/deploy.sh ./dist/main root@<server> /srv/bonegrader main
```

Hosting-Setup (Caddy, DNS, Rollback): siehe [`deploy/README.md`](deploy/README.md).

## Release bauen (Installer für alle OS)

```bash
git tag v0.1.0 && git push --tags     # -> GitHub Actions baut & veröffentlicht die Installer
```

Der Workflow [`.github/workflows/release.yml`](.github/workflows/release.yml) baut
auf Windows/Linux/macOS-Runnern und hängt die Pakete an ein (Draft-)Release.
Lokal (nur das eigene OS): `cargo tauri build`.

## Projektstruktur

```
crates/core      # Manifest, Hashing, jar-modId-Parser, Diff-Engine  (Unit-Tests)
crates/publish   # Dev-CLI: Instanz -> Manifest + Content-Store
crates/client    # Apply-Engine, Instanz-Erkennung, HTTP-Fetcher      (E2E-Tests)
crates/cli       # headless Updater (plan/update)
app/             # Tauri-v2-Desktop-App (src-tauri + statisches ui/)
deploy/          # Hosting: Caddy-Block + Deploy-Skript
```
