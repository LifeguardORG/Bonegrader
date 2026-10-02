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
   - **macOS:** `.dmg` (Apple Silicon und Intel)
2. Bonegrader öffnen. Die Server-Adresse ist bereits eingetragen → **Weiter**
   (nur bei Bedarf unter „Server ändern“ anpassen).
3. **Instanz wählen**: CurseForge-, Prism- und Profile des offiziellen
   Launchers werden erkannt, sonst **Ordner wählen …** oder **Eigene Instanz
   anlegen** (eigener Spielordner fürs Pack im offiziellen Launcher).
4. **Prüfen** → Änderungen ansehen (mit Mod-Namen und Versionen, z. B.
   „Create 6.0.10 → 6.0.11“) → **Aktualisieren**.

**Ab dem zweiten Start** prüft Bonegrader von selbst und zeigt sofort
„… ist aktuell“ oder die anstehenden Änderungen – ein Klick genügt.

- Während des Downloads siehst du Tempo und Restzeit und kannst
  **abbrechen**: Es wird nichts verändert, bereits geladene Dateien werden beim
  nächsten Mal wiederverwendet. Schließt du das Fenster mitten im Update, fragt
  Bonegrader vorher nach.
- Eigene Mods bleiben erhalten (auf Wunsch einzeln oder alle entfernen); bei
  Konflikten mit dem Pack fragt Bonegrader nach. Sieht die Instanz nicht nach
  dem Pack aus (z. B. ein anderes Modpack), musst du das erst bestätigen.
- Ersetzte/entfernte Dateien landen in `.bonegrader-backup/` („Backup-Ordner
  öffnen“); **„Update vom … rückgängig“** stellt den vorherigen Stand wieder her.
- Fehlt NeoForge (offizieller Launcher), richtet Bonegrader es auf Knopfdruck
  ein; der Server wird einmalig in die Mehrspieler-Liste eingetragen.
- Fehler werden in Klartext erklärt („Keine Verbindung zum Update-Server“,
  „Minecraft läuft noch“ …), mit „Erneut versuchen“ und kopierbaren Details.
  Die App folgt dem hellen bzw. dunklen Design des Systems.

**Warnung beim ersten Start?** Die Installer sind (noch) nicht code-signiert:

- *Windows (SmartScreen):* „Weitere Informationen“ → „Trotzdem ausführen“.
- *macOS:* Rechtsklick auf die App → „Öffnen“ → „Öffnen“; falls macOS meldet,
  die App sei beschädigt: `xattr -dr com.apple.quarantine /Applications/Bonegrader.app`.

## Wie es funktioniert

- Der Server hält pro Channel ein `manifest.json` (optional signiert,
  `manifest.json.sig`) + **content-addressed** Blobs (`files/by-hash/<sha1>`,
  unveränderlich). Geladen wird nur über HTTPS.
- Der Client scannt die Instanz, gleicht per SHA-1/`modId` gegen das Manifest ab
  und lädt nur Abweichungen. Das Manifest enthält die Mod-Namen und -Versionen
  aus den `mods.toml`, damit Spieler lesbare Änderungen sehen. Verwaltete Dateien werden getrackt, alles andere
  gilt als Eigen-Mod und bleibt. Identische Doppel (z. B. `jei (1).jar`) werden
  erkannt und entfernt — zwei gleiche Mods würden das Spiel abstürzen lassen.
- Downloads werden gestreamt, parallel geladen und vor jeder Änderung geprüft
  (Größe, SHA-1, SHA-256); bricht die Leitung ab, wird beim nächsten Versuch
  fortgesetzt. Danach werden alle Änderungen in einem Rutsch angewendet: Schlägt
  dabei etwas fehl (z. B. weil Minecraft noch läuft), wird alles zurückgenommen.
- Jedes Update protokolliert, was es getan hat, und lässt sich rückgängig machen;
  die letzten 5 Backups bleiben erhalten.
- Ist das Manifest signiert und der Client mit dem Schlüssel gebaut
  (`keys/manifest-signing.pub`), nimmt er nichts anderes an — ein übernommener
  Webserver kann dann keine Mods an die Spieler verteilen.

## Entwicklung

Voraussetzungen: Rust (stable) + Tauri-Systempakete
(`webkit2gtk-4.1`, `librsvg` …; siehe `.github/workflows/ci.yml`).

```bash
cargo test --workspace          # alles (Diff-Engine inkl. Property-Test, Apply, Signaturen, …)
cargo test -p bonegrader-core -p bonegrader-client -p bonegrader-publish   # ohne GUI-Abhängigkeiten
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check
cargo run -p bonegrader-app     # GUI starten (Frontend ist statisch eingebettet)
cargo run --example detect -p bonegrader-client   # erkannte Instanzen anzeigen
```

Die CI (`.github/workflows/ci.yml`) prüft Format, Clippy und Tests auf Linux und
die Kernlogik zusätzlich auf Windows und macOS.

Headless-Updater (dieselbe Pipeline wie die GUI):

```bash
cargo run -p bonegrader-cli -- plan   --instance <pfad> --base-url https://bonegrader.rescue-compete.de/main
cargo run -p bonegrader-cli -- update --instance <pfad> --base-url https://bonegrader.rescue-compete.de/main
cargo run -p bonegrader-cli -- undo   --instance <pfad>
```

## Channel veröffentlichen (nach Mod-Fixes)

```bash
cargo run --release -p bonegrader-publish -- build \
  --instance <instanz> --out ./dist/main --channel main \
  --sign-key ~/.config/bonegrader/manifest-signing.key   # zeigt Diff, --gc räumt auf
deploy/deploy.sh ./dist/main deploy@<server> /srv/bonegrader main
```

Oder per **Admin-App** (`cargo run -p bonegrader-admin`): Vorschau gegen den
Live-Stand (mit Mod-Namen und Versionen), **Verbindung testen**, dann
Veröffentlichen – mit Bestätigung und Zusammenfassung. Die App prüft, ob die
Spieler-App den Stand annehmen würde (Signatur, Schlüssel), und sperrt das
Veröffentlichen sonst; das Formular wird schon beim Tippen geprüft. Unter
**„Verlauf & Rollback“** lässt sich ein früherer Stand per Klick zurückholen.
Hosting, Deploy-Nutzer, Signatur einrichten, Beta-Channel und Rollback: siehe
[`deploy/README.md`](deploy/README.md).

## Release bauen (Installer für alle OS)

```bash
git tag v1.2.0 && git push --tags     # -> GitHub Actions testet, baut & veröffentlicht die Installer
```

Die Version steht nur in `Cargo.toml` (`[workspace.package]`). Der Workflow
[`.github/workflows/release.yml`](.github/workflows/release.yml) baut auf
Windows/Linux/macOS-Runnern (macOS als Universal-Binary) und hängt die Pakete an
ein (Draft-)Release. Lokal (nur das eigene OS): `cargo tauri build`. Danach mit
`--latest-client-version` veröffentlichen, damit die Spieler den Hinweis sehen.

## Projektstruktur

```
crates/core      # Manifest, Hashing, Signaturen, jar-modId-Parser, Diff-Engine
crates/client    # Pipeline (session), Downloads, transaktionales Apply + Undo,
                 # Instanz-Erkennung, NeoForge-Installer, servers.dat
crates/publish   # Publisher: Instanz -> geprüftes Manifest + Content-Store, Signieren,
                 # Upload, Verlauf/Rollback (remote)
crates/cli       # headless Updater (plan/update/undo)
app/             # Tauri-v2-Desktop-App (src-tauri + statisches ui/)
admin/           # Tauri-v2-Admin-App zum Veröffentlichen
keys/            # öffentliche Signaturschlüssel, die der Client akzeptiert
deploy/          # Hosting: Caddy-Block, Deploy-/Rollback-Skripte
web/             # Download-Seite
```
