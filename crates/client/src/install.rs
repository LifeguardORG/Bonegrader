//! Optional NeoForge client install for the vanilla (official launcher) path.
//!
//! Flags verified against `neoforge-21.1.234-installer.jar --help`:
//!   `--install-client [dir]`  (dir defaults to ~/.minecraft)
//! The installer downloads the required libraries and registers a launcher
//! profile itself; on top of that we upsert a dedicated profile so the pack's
//! mods live in their own game directory instead of the shared `.minecraft`.

use crate::apply::Fetcher;
use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

const NEOFORGE_MAVEN: &str = "https://maven.neoforged.net/releases/net/neoforged/neoforge";

pub fn installer_url(loader_version: &str) -> String {
    format!("{NEOFORGE_MAVEN}/{loader_version}/neoforge-{loader_version}-installer.jar")
}

fn version_id(loader_version: &str) -> String {
    format!("neoforge-{loader_version}")
}

/// True if the launcher already has this NeoForge version installed.
pub fn is_installed(dotmc: &Path, loader_version: &str) -> bool {
    let id = version_id(loader_version);
    dotmc.join("versions").join(&id).join(format!("{id}.json")).is_file()
}

const fn java_exe() -> &'static str {
    if cfg!(windows) { "java.exe" } else { "java" }
}

fn java_runs(exe: &Path) -> bool {
    Command::new(exe).arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// Locate a usable Java: `JAVA_HOME`, then `PATH`, then a launcher-bundled runtime.
pub fn find_java(dotmc: &Path) -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        let exe = Path::new(&home).join("bin").join(java_exe());
        if java_runs(&exe) { return Some(exe); }
    }
    let on_path = PathBuf::from(java_exe());
    if java_runs(&on_path) { return Some(on_path); }
    find_bundled_java(&dotmc.join("runtime"), 6)
}

// Mojang stores runtimes at runtime/<name>/<os>/<name>/bin/java(.exe).
fn find_bundled_java(dir: &Path, depth: u8) -> Option<PathBuf> {
    if depth == 0 { return None; }
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if !p.is_dir() { continue; }
        let candidate = p.join("bin").join(java_exe());
        if candidate.is_file() && java_runs(&candidate) { return Some(candidate); }
        if let Some(found) = find_bundled_java(&p, depth - 1) { return Some(found); }
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
    let obj = root.as_object_mut().context("launcher_profiles.json root is not an object")?;
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
    profile.insert("created".into(), Value::String(existing_created.unwrap_or_else(|| created.into())));
    profile.insert("lastVersionId".into(), Value::String(version_id(loader_version)));
    profile.insert("gameDir".into(), Value::String(game_dir.to_string_lossy().into_owned()));
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

/// Download the installer and run a headless client install into `dotmc`.
pub fn run_installer(
    dotmc: &Path,
    loader_version: &str,
    fetcher: &dyn Fetcher,
    java: &Path,
) -> Result<()> {
    let url = installer_url(loader_version);
    let bytes = fetcher.get(&url).with_context(|| format!("downloading {url}"))?;
    let jar = std::env::temp_dir().join(format!("neoforge-{loader_version}-installer.jar"));
    std::fs::write(&jar, &bytes).context("writing installer to temp")?;

    let status = Command::new(java)
        .arg("-jar")
        .arg(&jar)
        .arg("--install-client")
        .arg(dotmc)
        .status()
        .with_context(|| format!("running the installer with {}", java.display()))?;
    let _ = std::fs::remove_file(&jar);

    if !status.success() {
        bail!("NeoForge installer failed (exit {:?})", status.code());
    }
    Ok(())
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
    let updated = upsert_profile(&text, profile_key, profile_name, loader_version, game_dir, created)?;
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
        let out = upsert_profile(input, "bonegrader", "BonesAndBees", "21.1.234", Path::new("/home/p/.minecraft/bab"), "2026-07-27T00:00:00Z").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["profiles"]["vanilla"]["name"], "Latest", "existing profile kept");
        assert_eq!(v["settings"]["enableSnapshots"], false, "top-level keys kept");
        assert_eq!(v["profiles"]["bonegrader"]["lastVersionId"], "neoforge-21.1.234");
        assert_eq!(v["profiles"]["bonegrader"]["gameDir"], "/home/p/.minecraft/bab");
        assert_eq!(v["profiles"]["bonegrader"]["name"], "BonesAndBees");
    }

    #[test]
    fn upsert_preserves_created_on_update() {
        let first = upsert_profile("{}", "bonegrader", "BonesAndBees", "21.1.234", Path::new("/a"), "FIRST").unwrap();
        let second = upsert_profile(&first, "bonegrader", "BonesAndBees", "21.1.238", Path::new("/b"), "SECOND").unwrap();
        let v: Value = serde_json::from_str(&second).unwrap();
        assert_eq!(v["profiles"]["bonegrader"]["created"], "FIRST", "created timestamp kept across updates");
        assert_eq!(v["profiles"]["bonegrader"]["lastVersionId"], "neoforge-21.1.238", "version updated");
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
        assert!(!v["profiles"]["latest-release"].is_null(), "unrelated profiles kept");

        // On the manual/CurseForge path we *are* the auto profile — keep it.
        let keep = prune_duplicate_auto_profile(input, "bonegrader", Path::new("/mc")).unwrap();
        let vk: Value = serde_json::from_str(&keep).unwrap();
        assert!(!vk["profiles"]["bonegrader"].is_null(), "auto profile kept on manual path");

        // A `bonegrader` profile for a different game dir is not our duplicate.
        let other = prune_duplicate_auto_profile(input, "52fdc", Path::new("/elsewhere")).unwrap();
        let vo: Value = serde_json::from_str(&other).unwrap();
        assert!(!vo["profiles"]["bonegrader"].is_null(), "different gameDir not pruned");
    }
}
