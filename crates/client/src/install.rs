//! Optional NeoForge client install for the vanilla (official launcher) path.
//!
//! Flags verified against `neoforge-21.1.234-installer.jar --help`:
//!   `--install-client [dir]`  (dir defaults to ~/.minecraft)
//! The installer downloads the required libraries and registers a launcher
//! profile itself; on top of that we upsert a dedicated profile so the pack's
//! mods live in their own game directory instead of the shared `.minecraft`.

use crate::fetch::{is_not_found, Fetcher};
use anyhow::{bail, Context, Result};
use bonegrader_core::hash::{sha1_bytes, sha256_bytes};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NEOFORGE_MAVEN: &str = "https://maven.neoforged.net/releases/net/neoforged/neoforge";
/// The real installer is a few MB.
const INSTALLER_LIMIT: u64 = 64 * 1024 * 1024;

/// NeoForge versions look like `21.1.234` or `20.4.80-beta`. Only such plain
/// strings are accepted, so a manipulated manifest cannot steer the download
/// URL or the launcher profile anywhere else.
pub fn is_valid_loader_version(v: &str) -> bool {
    v.len() <= 64
        && v.starts_with(|c: char| c.is_ascii_alphanumeric())
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
        && !v.contains("..")
}

pub fn installer_url(loader_version: &str) -> String {
    format!("{NEOFORGE_MAVEN}/{loader_version}/neoforge-{loader_version}-installer.jar")
}

fn version_id(loader_version: &str) -> String {
    format!("neoforge-{loader_version}")
}

/// True if the launcher already has this NeoForge version installed.
pub fn is_installed(dotmc: &Path, loader_version: &str) -> bool {
    let id = version_id(loader_version);
    dotmc
        .join("versions")
        .join(&id)
        .join(format!("{id}.json"))
        .is_file()
}

const fn java_exe() -> &'static str {
    if cfg!(windows) {
        "java.exe"
    } else {
        "java"
    }
}

/// A command that opens no console window on Windows (the app is a GUI).
fn hidden_command(program: &Path) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

fn java_runs(exe: &Path) -> bool {
    hidden_command(exe)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Locate a usable Java: `JAVA_HOME`, then `PATH`, then a launcher-bundled
/// runtime (classic `.minecraft/runtime`, or the Microsoft Store launcher's).
pub fn find_java(dotmc: &Path) -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        let exe = Path::new(&home).join("bin").join(java_exe());
        if java_runs(&exe) {
            return Some(exe);
        }
    }
    let on_path = PathBuf::from(java_exe());
    if java_runs(&on_path) {
        return Some(on_path);
    }
    let mut roots = vec![dotmc.join("runtime")];
    if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            roots.push(
                PathBuf::from(local).join(
                    "Packages/Microsoft.4297127D64EC6_8wekyb3d8bbwe/LocalCache/Local/runtime",
                ),
            );
        }
    }
    roots.iter().find_map(|r| find_bundled_java(r, 6))
}

// Mojang stores runtimes at runtime/<name>/<os>/<name>/bin/java(.exe).
fn find_bundled_java(dir: &Path, depth: u8) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let candidate = p.join("bin").join(java_exe());
        if candidate.is_file() && java_runs(&candidate) {
            return Some(candidate);
        }
        if let Some(found) = find_bundled_java(&p, depth - 1) {
            return Some(found);
        }
    }
    None
}

/// Add or update a launcher profile pointing at the NeoForge version and game
/// directory. Operates on the raw `launcher_profiles.json` text and preserves
/// every other profile and top-level key.
pub fn upsert_profile(
    profiles_json: &str,
    profile_key: &str,
    name: &str,
    loader_version: &str,
    game_dir: &Path,
    created: &str,
) -> Result<String> {
    let mut root: Value =
        serde_json::from_str(profiles_json).context("parsing launcher_profiles.json")?;
    let obj = root
        .as_object_mut()
        .context("launcher_profiles.json root is not an object")?;
    let profiles = obj
        .entry("profiles")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .context("'profiles' is not an object")?;

    let existing_created = profiles
        .get(profile_key)
        .and_then(|p| p.get("created"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let mut profile = Map::new();
    profile.insert("name".into(), Value::String(name.into()));
    profile.insert("type".into(), Value::String("custom".into()));
    profile.insert(
        "created".into(),
        Value::String(existing_created.unwrap_or_else(|| created.into())),
    );
    profile.insert(
        "lastVersionId".into(),
        Value::String(version_id(loader_version)),
    );
    profile.insert(
        "gameDir".into(),
        Value::String(game_dir.to_string_lossy().into_owned()),
    );
    profiles.insert(profile_key.into(), Value::Object(profile));

    serde_json::to_string_pretty(&root).context("serializing launcher_profiles.json")
}

/// After binding the loader to a *real* launcher profile, drop the separate
/// auto-created `bonegrader` profile if it points at the same game directory —
/// otherwise the launcher lists the pack twice. No-op when we *are* the
/// `bonegrader` profile (manual / CurseForge path) or when no such redundant
/// profile exists.
fn prune_duplicate_auto_profile(
    profiles_json: &str,
    active_key: &str,
    game_dir: &Path,
) -> Result<String> {
    const AUTO_KEY: &str = "bonegrader";
    if active_key == AUTO_KEY {
        return Ok(profiles_json.to_string());
    }
    let mut root: Value =
        serde_json::from_str(profiles_json).context("parsing launcher_profiles.json")?;
    let Some(profiles) = root.get_mut("profiles").and_then(Value::as_object_mut) else {
        return Ok(profiles_json.to_string());
    };
    let redundant = profiles
        .get(AUTO_KEY)
        .and_then(|p| p.get("gameDir"))
        .and_then(Value::as_str)
        .is_some_and(|g| Path::new(g) == game_dir);
    if !redundant {
        return Ok(profiles_json.to_string());
    }
    profiles.remove(AUTO_KEY);
    serde_json::to_string_pretty(&root).context("serializing launcher_profiles.json")
}

/// Check a downloaded installer: it must be a jar, and it must match the
/// checksum the NeoForge Maven publishes next to it (`.sha256`, else `.sha1`).
/// Without a published checksum, HTTPS and the jar check have to do.
pub fn verify_installer(fetcher: &dyn Fetcher, url: &str, bytes: &[u8]) -> Result<()> {
    if !bytes.starts_with(b"PK\x03\x04") {
        bail!("{url} ist kein Jar – Download fehlerhaft");
    }
    type Digest = fn(&[u8]) -> String;
    let checks: [(&str, Digest); 2] = [("sha256", sha256_bytes), ("sha1", sha1_bytes)];
    for (ext, digest) in checks {
        match fetcher.get_limited(&format!("{url}.{ext}"), 1024) {
            Ok(body) => {
                let text = String::from_utf8_lossy(&body);
                let want = text
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if want != digest(bytes) {
                    bail!("Prüfsumme des NeoForge-Installers stimmt nicht ({ext}) – Download beschädigt");
                }
                return Ok(());
            }
            Err(e) if is_not_found(&e) => continue,
            Err(e) => return Err(e.context("Prüfsumme des NeoForge-Installers laden")),
        }
    }
    Ok(())
}

/// Download the installer and run a headless client install into `dotmc`.
pub fn run_installer(
    dotmc: &Path,
    loader_version: &str,
    fetcher: &dyn Fetcher,
    java: &Path,
) -> Result<()> {
    if !is_valid_loader_version(loader_version) {
        bail!("ungültige NeoForge-Version im Manifest: {loader_version}");
    }
    let url = installer_url(loader_version);
    let bytes = fetcher
        .get_limited(&url, INSTALLER_LIMIT)
        .with_context(|| format!("NeoForge-Installer laden ({url})"))?;
    verify_installer(fetcher, &url, &bytes)?;

    // A private, unpredictable temp folder (a fixed name in a shared /tmp
    // could be swapped by another local user).
    let dir = tempfile::Builder::new()
        .prefix("bonegrader-neoforge-")
        .tempdir()
        .context("Temp-Ordner anlegen")?;
    let jar = dir
        .path()
        .join(format!("neoforge-{loader_version}-installer.jar"));
    std::fs::write(&jar, &bytes).context("Installer speichern")?;

    let output = hidden_command(java)
        .arg("-jar")
        .arg(&jar)
        .arg("--install-client")
        .arg(dotmc)
        .output()
        .with_context(|| format!("Installer mit {} starten", java.display()))?;
    if !output.status.success() {
        bail!(
            "NeoForge-Installer fehlgeschlagen (Exit-Code {:?}). Letzte Ausgabe:\n{}",
            output.status.code(),
            output_tail(&output, 15)
        );
    }
    Ok(())
}

/// The last `n` lines of a process's stdout + stderr.
fn output_tail(output: &Output, n: usize) -> String {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Ensure the NeoForge client is installed *and* that a dedicated launcher
/// profile points at it for this pack.
///
/// The installer download/run is skipped when the version is already present on
/// disk, but the launcher profile is **always** upserted. This matters: a
/// player can have the right NeoForge version installed yet still launch a
/// *vanilla* profile (its `lastVersionId` is a bare `1.21.1`), so the game
/// starts without the loader and the server rejects the join. Rewriting the
/// profile guarantees there is a `neoforge-<version>` profile bound to the
/// pack's game directory that the player can select.
///
/// Returns `true` if the installer actually ran, `false` if only the profile
/// was (re)written.
pub fn ensure_client(
    dotmc: &Path,
    loader_version: &str,
    fetcher: &dyn Fetcher,
    profile_key: &str,
    profile_name: &str,
    game_dir: &Path,
    created: &str,
) -> Result<bool> {
    if !is_valid_loader_version(loader_version) {
        bail!("ungültige NeoForge-Version im Manifest: {loader_version}");
    }
    let ran_installer = if is_installed(dotmc, loader_version) {
        false
    } else {
        let java = find_java(dotmc).context(
            "no Java found — install Java or launch Minecraft once so its bundled runtime exists, then retry",
        )?;
        run_installer(dotmc, loader_version, fetcher, &java)?;
        true
    };

    // Tolerate a missing profiles file by starting from an empty object.
    let profiles_path = dotmc.join("launcher_profiles.json");
    let text = std::fs::read_to_string(&profiles_path).unwrap_or_else(|_| "{}".to_string());
    let updated = upsert_profile(
        &text,
        profile_key,
        profile_name,
        loader_version,
        game_dir,
        created,
    )?;
    // When we bound the loader to a real profile, drop any leftover auto-created
    // `bonegrader` duplicate for the same game dir so the launcher shows it once.
    let updated = prune_duplicate_auto_profile(&updated, profile_key, game_dir)?;
    let tmp = profiles_path.with_extension("json.tmp");
    std::fs::write(&tmp, updated).context("writing launcher_profiles.json")?;
    std::fs::rename(&tmp, &profiles_path).context("swapping launcher_profiles.json")?;
    Ok(ran_installer)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Maven(std::collections::HashMap<String, Vec<u8>>);
    impl Fetcher for Maven {
        fn open(&self, url: &str) -> Result<Box<dyn std::io::Read + Send>> {
            match self.0.get(url) {
                Some(b) => Ok(Box::new(std::io::Cursor::new(b.clone()))),
                None => Err(crate::fetch::HttpStatus {
                    code: 404,
                    url: url.into(),
                }
                .into()),
            }
        }
    }

    #[test]
    fn validates_loader_versions() {
        assert!(is_valid_loader_version("21.1.234"));
        assert!(is_valid_loader_version("20.4.80-beta"));
        assert!(!is_valid_loader_version(""));
        assert!(!is_valid_loader_version("../../evil"));
        assert!(!is_valid_loader_version("21.1.234/../../x"));
        assert!(!is_valid_loader_version("21.1 234"));
        assert!(!is_valid_loader_version("-21"));
    }

    #[test]
    fn verifies_installer_checksums() {
        let url = "https://maven/x/installer.jar";
        let jar = b"PK\x03\x04 installer bytes".to_vec();
        let mut files = std::collections::HashMap::new();

        // No checksum published: the jar check alone passes.
        assert!(verify_installer(&Maven(files.clone()), url, &jar).is_ok());
        assert!(verify_installer(&Maven(files.clone()), url, b"<html>404</html>").is_err());

        // Matching .sha1 (no .sha256).
        files.insert(
            format!("{url}.sha1"),
            format!("{}  installer.jar\n", sha1_bytes(&jar)).into_bytes(),
        );
        assert!(verify_installer(&Maven(files.clone()), url, &jar).is_ok());

        // .sha256 takes precedence and must match.
        files.insert(format!("{url}.sha256"), b"0000".to_vec());
        let err = verify_installer(&Maven(files.clone()), url, &jar).unwrap_err();
        assert!(format!("{err:#}").contains("sha256"), "{err:#}");
        files.insert(format!("{url}.sha256"), sha256_bytes(&jar).into_bytes());
        assert!(verify_installer(&Maven(files), url, &jar).is_ok());
    }

    #[test]
    fn builds_installer_url() {
        assert_eq!(
            installer_url("21.1.234"),
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.234/neoforge-21.1.234-installer.jar"
        );
    }

    #[test]
    fn detects_installed_version() {
        let dir = std::env::temp_dir().join(format!("bg-nf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!is_installed(&dir, "21.1.234"));
        let vdir = dir.join("versions/neoforge-21.1.234");
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::write(vdir.join("neoforge-21.1.234.json"), "{}").unwrap();
        assert!(is_installed(&dir, "21.1.234"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_adds_profile_and_keeps_others() {
        let input = r#"{
            "profiles": { "vanilla": { "name": "Latest", "lastVersionId": "1.21.1" } },
            "settings": { "enableSnapshots": false },
            "version": 3
        }"#;
        let out = upsert_profile(
            input,
            "bonegrader",
            "BonesAndBees",
            "21.1.234",
            Path::new("/home/p/.minecraft/bab"),
            "2026-07-27T00:00:00Z",
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["profiles"]["vanilla"]["name"], "Latest",
            "existing profile kept"
        );
        assert_eq!(
            v["settings"]["enableSnapshots"], false,
            "top-level keys kept"
        );
        assert_eq!(
            v["profiles"]["bonegrader"]["lastVersionId"],
            "neoforge-21.1.234"
        );
        assert_eq!(
            v["profiles"]["bonegrader"]["gameDir"],
            "/home/p/.minecraft/bab"
        );
        assert_eq!(v["profiles"]["bonegrader"]["name"], "BonesAndBees");
    }

    #[test]
    fn upsert_preserves_created_on_update() {
        let first = upsert_profile(
            "{}",
            "bonegrader",
            "BonesAndBees",
            "21.1.234",
            Path::new("/a"),
            "FIRST",
        )
        .unwrap();
        let second = upsert_profile(
            &first,
            "bonegrader",
            "BonesAndBees",
            "21.1.238",
            Path::new("/b"),
            "SECOND",
        )
        .unwrap();
        let v: Value = serde_json::from_str(&second).unwrap();
        assert_eq!(
            v["profiles"]["bonegrader"]["created"], "FIRST",
            "created timestamp kept across updates"
        );
        assert_eq!(
            v["profiles"]["bonegrader"]["lastVersionId"], "neoforge-21.1.238",
            "version updated"
        );
        assert_eq!(v["profiles"]["bonegrader"]["gameDir"], "/b");
    }

    #[test]
    fn prune_removes_redundant_auto_profile_only_for_other_keys() {
        let input = r#"{"profiles":{
            "52fdc":{"name":"BonesAndBees","lastVersionId":"neoforge-21.1.234","gameDir":"/mc"},
            "bonegrader":{"name":"BonesAndBees","lastVersionId":"neoforge-21.1.234","gameDir":"/mc"},
            "latest-release":{"name":"","lastVersionId":"latest-release"}
        }}"#;

        // Converting the real profile drops the duplicate auto-profile.
        let out = prune_duplicate_auto_profile(input, "52fdc", Path::new("/mc")).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["profiles"]["bonegrader"].is_null(), "duplicate removed");
        assert!(!v["profiles"]["52fdc"].is_null(), "real profile kept");
        assert!(
            !v["profiles"]["latest-release"].is_null(),
            "unrelated profiles kept"
        );

        // On the manual/CurseForge path we *are* the auto profile — keep it.
        let keep = prune_duplicate_auto_profile(input, "bonegrader", Path::new("/mc")).unwrap();
        let vk: Value = serde_json::from_str(&keep).unwrap();
        assert!(
            !vk["profiles"]["bonegrader"].is_null(),
            "auto profile kept on manual path"
        );

        // A `bonegrader` profile for a different game dir is not our duplicate.
        let other = prune_duplicate_auto_profile(input, "52fdc", Path::new("/elsewhere")).unwrap();
        let vo: Value = serde_json::from_str(&other).unwrap();
        assert!(
            !vo["profiles"]["bonegrader"].is_null(),
            "different gameDir not pruned"
        );
    }
}
