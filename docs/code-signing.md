# Installer signieren (Windows & macOS)

Ohne Signatur warnt Windows (SmartScreen) und macOS (Gatekeeper) beim **ersten**
Start. Updates, die Bonegrader selbst lädt und installiert (siehe
[README → Release bauen](../README.md#release-bauen-installer-für-alle-os)), lösen
diese Warnungen nicht erneut aus – sie betrifft also vor allem die Erstinstallation.
Die Download-Seite erklärt den Spielern, wie sie die Warnung wegklicken.

## Stand im Repository

| System | Heute | Was der Spieler sieht |
| --- | --- | --- |
| macOS | **ad-hoc signiert** ([`app/src-tauri/tauri.macos.conf.json`](../app/src-tauri/tauri.macos.conf.json)) | „Bonegrader kann nicht geöffnet werden“ → Systemeinstellungen → Datenschutz & Sicherheit → **Dennoch öffnen** (kein Terminal-Befehl mehr) |
| Windows | unsigniert | „Der Computer wurde durch Windows geschützt“ → **Weitere Informationen** → **Trotzdem ausführen** |
| Linux | – (nicht üblich) | keine Warnung |

Der Release-Workflow signiert automatisch, sobald die passenden GitHub-Secrets
existieren – ohne sie baut er wie bisher.

> Die ad-hoc-Signatur ist neu und konnte hier nicht auf einem Mac getestet werden:
> Bitte nach dem nächsten Release einmal die `.dmg` auf einem Mac öffnen. Schlägt
> der macOS-Build fehl, `tauri.macos.conf.json` löschen – dann ist alles wie vorher.

## macOS: signieren und notarisieren

Voraussetzung: **Apple Developer Program** (99 USD/Jahr, developer.apple.com/programs).
Danach verschwindet die Warnung ganz.

1. Ein Zertifikat **Developer ID Application** anlegen (developer.apple.com →
   Certificates, oder Xcode → Settings → Accounts → Manage Certificates) und in
   der Schlüsselbundverwaltung als `.p12` mit Passwort exportieren.
2. Für die Apple-ID ein **app-spezifisches Passwort** erstellen
   (account.apple.com → Anmeldung und Sicherheit).
3. Im Repository auf GitHub unter *Settings → Secrets and variables → Actions*
   diese Secrets anlegen:

   | Secret | Inhalt |
   | --- | --- |
   | `APPLE_CERTIFICATE` | `base64 -i zertifikat.p12 \| tr -d '\n'` (eine Zeile) |
   | `APPLE_CERTIFICATE_PASSWORD` | Passwort der `.p12` |
   | `APPLE_SIGNING_IDENTITY` | z. B. `Developer ID Application: Vorname Name (TEAMID)` – siehe `security find-identity -v -p codesigning` |
   | `APPLE_ID` | E-Mail der Apple-ID |
   | `APPLE_PASSWORD` | das app-spezifische Passwort |
   | `APPLE_TEAM_ID` | die Team-ID (10 Zeichen) |

4. Nächstes Release taggen: Der macOS-Build wird signiert und notarisiert (die
   Notarisierung dauert beim ersten Mal manchmal lange).

## Windows: Möglichkeiten

| Weg | Kosten | Passt für Bonegrader? |
| --- | --- | --- |
| **SignPath Foundation** – kostenlose Signatur für Open-Source-Projekte | 0 € | **Ja, empfohlen.** Voraussetzungen laut signpath.org: OSI-Lizenz ohne kommerzielle Doppellizenz (MIT ✓), keine proprietären Teile, aktiv gepflegt, bereits veröffentlicht, automatisierter Build (GitHub Actions ✓), eine veröffentlichte Code-Signing-Policy auf der Projektseite. Als Herausgeber steht dann „SignPath Foundation“ im Zertifikat. |
| **Azure Artifact Signing** (früher Trusted Signing) | ca. 10 USD/Monat | Laut Microsoft für Privatpersonen nur in den USA und Kanada, in der EU nur für Organisationen. |
| OV-Zertifikat einer Zertifizierungsstelle | ca. 100–300 €/Jahr | Möglich, aber der Schlüssel muss seit 2023 auf Hardware bzw. in einem Cloud-HSM liegen; Signieren in GitHub Actions geht dann nur über den Cloud-Dienst des Anbieters. |

Auch mit Zertifikat verschwindet SmartScreen nicht sofort: Die Warnung hängt
auch an der „Reputation“, die eine signierte Datei erst mit der Zeit sammelt.

**Vorgehen mit SignPath:** auf signpath.org bewerben (Projekt-URL, Beschreibung,
Code-Signing-Policy). Nach der Freischaltung gibt es eine Organisations-ID, ein
Projekt mit Signing-Policy und ein API-Token. Mit diesen Angaben lässt sich der
Signier-Schritt in [`release.yml`](../.github/workflows/release.yml) ergänzen:
Die Windows-Installer werden nach dem Build an SignPath geschickt und signiert
zurück in das Release geladen.

## Linux

Für `.AppImage`/`.deb` erwartet niemand eine Signatur. Die Updates selbst sind
unabhängig davon immer signiert (Updater-Schlüssel, siehe README).
