//! Discover Minecraft instances the player might update.
//!
//! Three sources are supported automatically — CurseForge app instances (each
//! a folder with a `minecraftinstance.json`), Prism Launcher instances
//! (`instance.cfg` + `mmc-pack.json`) and official-launcher profiles (from
//! `.minecraft/launcher_profiles.json`) — plus a manual folder pick in the UI.
//!
//! The parsing functions ([`parse_curseforge_instance`], [`parse_prism_instance`],
//! [`parse_launcher_profiles`]) are pure and unit-tested; the `discover_*` /
//! `default_*` helpers wrap them with best-effort, platform-specific path
//! guesses. Auto-detection is never trusted blindly: the plan carries an
//! [`InstanceAssessment`](bonegrader_core::diff::InstanceAssessment) and the UI
//! warns before updating an instance that doesn't look like the pack.

use bonegrader_core::launcher::parse_curseforge_instance as parse_cf;
use std::path::{Path, PathBuf};

/// Which launcher an instance belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum LauncherKind {
    CurseForge,
    Prism,
    Vanilla,
    Manual,
}

/// A discovered instance. `path` is the game directory that contains `mods/`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedInstance {
    pub name: String,
    pub path: PathBuf,
    pub launcher: LauncherKind,
    pub mc_version: Option<String>,
    pub loader_type: Option<String>,
    pub loader_version: Option<String>,
    /// Official-launcher profile key (from `launcher_profiles.json`), for
    /// Vanilla instances only. Lets an install target *this* profile in place
    /// instead of spawning a separate one. `None` for CurseForge / manual picks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_key: Option<String>,
}

/// Parse a CurseForge `minecraftinstance.json`. `dir` is the instance folder.
pub fn parse_curseforge_instance(dir: &Path, json: &str) -> Option<DetectedInstance> {
    let cf = parse_cf(json)?;
    Some(DetectedInstance {
        name: cf.name.clone()?,
        path: dir.to_path_buf(),
        launcher: LauncherKind::CurseForge,
        mc_version: cf.mc_version.clone(),
        loader_type: cf.loader_type().map(str::to_string),
        loader_version: cf.loader_version,
        profile_key: None,
    })
}

/// Name, Minecraft version and loader of a Prism Launcher instance, from its
/// `instance.cfg` (INI) and `mmc-pack.json` (component list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrismInstance {
    pub name: String,
    pub mc_version: Option<String>,
    pub loader_type: Option<String>,
    pub loader_version: Option<String>,
}

pub fn parse_prism_instance(instance_cfg: &str, mmc_pack: Option<&str>) -> Option<PrismInstance> {
    let name = instance_cfg
        .lines()
        .find_map(|l| l.trim().strip_prefix("name="))
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())?;
    let mut inst = PrismInstance {
        name,
        mc_version: None,
        loader_type: None,
        loader_version: None,
    };
    let pack: serde_json::Value = mmc_pack
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    for c in pack
        .get("components")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
    {
        let version = c.get("version").and_then(|v| v.as_str()).map(String::from);
        let loader = match c.get("uid").and_then(|u| u.as_str()).unwrap_or("") {
            "net.minecraft" => {
                inst.mc_version = version;
                continue;
            }
            "net.neoforged" => "neoforge",
            "net.minecraftforge" => "forge",
            "net.fabricmc.fabric-loader" => "fabric",
            "org.quiltmc.quilt-loader" => "quilt",
            _ => continue,
        };
        inst.loader_type = Some(loader.to_string());
        inst.loader_version = version;
    }
    Some(inst)
}

/// Scan a Prism Launcher `instances` folder.
pub fn scan_prism_instances(root: &Path) -> Vec<DetectedInstance> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(cfg) = std::fs::read_to_string(dir.join("instance.cfg")) else {
            continue;
        };
        let pack = std::fs::read_to_string(dir.join("mmc-pack.json")).ok();
        let Some(p) = parse_prism_instance(&cfg, pack.as_deref()) else {
            continue;
        };
        // The game directory is `.minecraft` or (newer instances) `minecraft`.
        let game_dir = [".minecraft", "minecraft"]
            .iter()
            .map(|d| dir.join(d))
            .find(|d| d.is_dir())
            .unwrap_or_else(|| dir.join(".minecraft"));
        out.push(DetectedInstance {
            name: p.name,
            path: game_dir,
            launcher: LauncherKind::Prism,
            mc_version: p.mc_version,
            loader_type: p.loader_type,
            loader_version: p.loader_version,
            profile_key: None,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Scan a CurseForge Instances root for instance folders.
pub fn scan_curseforge_instances(root: &Path) -> Vec<DetectedInstance> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(dir.join("minecraftinstance.json")) {
            if let Some(inst) = parse_curseforge_instance(&dir, &text) {
                out.push(inst);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Parse `.minecraft/launcher_profiles.json`. `dotmc` is the `.minecraft` dir,
/// used as the default game directory for profiles without an explicit `gameDir`.
pub fn parse_launcher_profiles(json: &str, dotmc: &Path) -> Vec<DetectedInstance> {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let profiles = match v.get("profiles").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for (id, prof) in profiles {
        let last = prof
            .get("lastVersionId")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let name = prof
            .get("name")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(if last.is_empty() { id.as_str() } else { last })
            .to_string();
        let path = prof
            .get("gameDir")
            .and_then(|x| x.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| dotmc.to_path_buf());
        let (loader_type, loader_version, mc_version) = parse_version_id(last);
        out.push(DetectedInstance {
            name,
            path,
            launcher: LauncherKind::Vanilla,
            mc_version,
            loader_type,
            loader_version,
            profile_key: Some(id.clone()),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Discover every instance we can find automatically.
pub fn discover_all() -> Vec<DetectedInstance> {
    let mut out = Vec::new();
    for root in default_curseforge_roots() {
        out.extend(scan_curseforge_instances(&root));
    }
    for root in default_prism_roots() {
        out.extend(scan_prism_instances(&root));
    }
    if let Some(dotmc) = default_dotminecraft() {
        if let Ok(text) = std::fs::read_to_string(dotmc.join("launcher_profiles.json")) {
            out.extend(parse_launcher_profiles(&text, &dotmc));
        }
    }
    out
}

/// Candidate CurseForge Instances roots for the current OS/user.
pub fn default_curseforge_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(h) = home_dir() {
        for sub in [
            "curseforge/minecraft/Instances",
            "Documents/curseforge/minecraft/Instances",
            "Dokumente/curseforge/minecraft/Instances",
            "OneDrive/Documents/curseforge/minecraft/Instances",
        ] {
            roots.push(h.join(sub));
        }
    }
    roots
}

/// Candidate Prism Launcher `instances` folders for the current OS/user
/// (including the Flatpak location on Linux).
pub fn default_prism_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if cfg!(target_os = "windows") {
        if let Some(a) = std::env::var_os("APPDATA") {
            roots.push(PathBuf::from(a).join("PrismLauncher/instances"));
        }
    } else if cfg!(target_os = "macos") {
        if let Some(h) = home_dir() {
            roots.push(h.join("Library/Application Support/PrismLauncher/instances"));
        }
    } else {
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|h| h.join(".local/share")));
        if let Some(d) = data {
            roots.push(d.join("PrismLauncher/instances"));
        }
        if let Some(h) = home_dir() {
            roots.push(
                h.join(".var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher/instances"),
            );
        }
    }
    roots
}

/// The default `.minecraft` directory for the current OS.
pub fn default_dotminecraft() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join(".minecraft"))
    } else if cfg!(target_os = "macos") {
        home_dir().map(|h| h.join("Library/Application Support/minecraft"))
    } else {
        home_dir().map(|h| h.join(".minecraft"))
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Interpret a launcher `lastVersionId` into
/// `(loader_type, loader_version, mc_version)`. Examples:
///   `neoforge-21.1.234`           -> (neoforge, 21.1.234, 1.21.1)
///   `forge-47.4.0`                -> (forge,    47.4.0,   None)
///   `fabric-loader-0.16.5-1.21.1` -> (fabric,   None,     1.21.1)
///   `quilt-loader-0.26-1.21.1`    -> (quilt,    None,     1.21.1)
///   `1.21.1`                      -> (None,     None,     1.21.1)
///
/// A bare Minecraft id (no loader prefix) is a vanilla profile: we surface the
/// MC version so the UI can show it and flag that no mod loader is installed,
/// instead of rendering a bare `?`.
fn parse_version_id(id: &str) -> (Option<String>, Option<String>, Option<String>) {
    if let Some(rest) = id.strip_prefix("neoforge-") {
        (
            Some("neoforge".into()),
            Some(rest.to_string()),
            mc_from_neoforge(rest),
        )
    } else if let Some(rest) = id.strip_prefix("forge-") {
        (Some("forge".into()), Some(rest.to_string()), None)
    } else if id.starts_with("fabric") || id.starts_with("quilt") {
        let loader = if id.starts_with("quilt") {
            "quilt"
        } else {
            "fabric"
        };
        // `fabric-loader-<loaderVer>-<mc>`: the MC id is the trailing segment.
        let mc = id
            .rsplit('-')
            .next()
            .filter(|s| is_mc_version(s))
            .map(str::to_string);
        (Some(loader.into()), None, mc)
    } else if is_mc_version(id) {
        (None, None, Some(id.to_string()))
    } else {
        (None, None, None)
    }
}

/// NeoForge versions mirror the Minecraft version: `21.1.234` -> MC `1.21.1`,
/// `20.4.237` -> MC `1.20.4`. Returns `None` if the shape is unexpected.
fn mc_from_neoforge(v: &str) -> Option<String> {
    let mut parts = v.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (numeric(major) && numeric(minor)).then(|| format!("1.{major}.{minor}"))
}

/// Loose check for a Mojang release id such as `1.21.1` or `1.21`.
fn is_mc_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() >= 2
        && parts[0] == "1"
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_curseforge_instance() {
        let json = r#"{
            "name": "BonesAndBees",
            "gameVersion": "1.21.1",
            "baseModLoader": { "name": "neoforge-21.1.234", "forgeVersion": "21.1.234", "minecraftVersion": "1.21.1" }
        }"#;
        let inst = parse_curseforge_instance(Path::new("/x/BonesAndBees"), json).unwrap();
        assert_eq!(inst.name, "BonesAndBees");
        assert_eq!(inst.mc_version.as_deref(), Some("1.21.1"));
        assert_eq!(inst.loader_type.as_deref(), Some("neoforge"));
        assert_eq!(inst.loader_version.as_deref(), Some("21.1.234"));
        assert_eq!(inst.launcher, LauncherKind::CurseForge);
        assert_eq!(inst.path, Path::new("/x/BonesAndBees"));
    }

    #[test]
    fn curseforge_parse_needs_a_name() {
        assert!(parse_curseforge_instance(Path::new("/x"), "{}").is_none());
        assert!(parse_curseforge_instance(Path::new("/x"), "not json").is_none());
    }

    #[test]
    fn scans_instance_folders_and_ignores_others() {
        let dir = std::env::temp_dir().join(format!("bg-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Alpha")).unwrap();
        std::fs::create_dir_all(dir.join("Beta")).unwrap();
        std::fs::create_dir_all(dir.join("NotAnInstance")).unwrap();
        std::fs::write(
            dir.join("Alpha/minecraftinstance.json"),
            r#"{"name":"Alpha","gameVersion":"1.21.1"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("Beta/minecraftinstance.json"),
            r#"{"name":"Beta","gameVersion":"1.20.1"}"#,
        )
        .unwrap();

        let found = scan_curseforge_instances(&dir);
        let names: Vec<&str> = found.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["Alpha", "Beta"], "sorted, junk folder ignored");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_launcher_profiles_with_and_without_gamedir() {
        let dotmc = Path::new("/home/p/.minecraft");
        let json = r#"{
            "profiles": {
                "aaa": { "name": "BonesAndBees", "gameDir": "/home/p/.minecraft/bab", "lastVersionId": "neoforge-21.1.234" },
                "bbb": { "name": "Vanilla", "lastVersionId": "1.21.1" }
            },
            "version": 3
        }"#;
        let mut found = parse_launcher_profiles(json, dotmc);
        found.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(found.len(), 2);

        let bab = found.iter().find(|i| i.name == "BonesAndBees").unwrap();
        assert_eq!(bab.path, Path::new("/home/p/.minecraft/bab"));
        assert_eq!(bab.loader_type.as_deref(), Some("neoforge"));
        assert_eq!(bab.loader_version.as_deref(), Some("21.1.234"));
        assert_eq!(
            bab.mc_version.as_deref(),
            Some("1.21.1"),
            "MC derived from the NeoForge version"
        );
        assert_eq!(
            bab.profile_key.as_deref(),
            Some("aaa"),
            "profile key exposed for in-place install"
        );

        let van = found.iter().find(|i| i.name == "Vanilla").unwrap();
        assert_eq!(van.path, dotmc, "no gameDir -> defaults to .minecraft");
        assert_eq!(van.loader_type, None);
        assert_eq!(
            van.mc_version.as_deref(),
            Some("1.21.1"),
            "bare version id surfaced as MC (no '?')"
        );
    }

    #[test]
    fn loader_type_distinguishes_forge_from_neoforge() {
        let nf = parse_curseforge_instance(
            Path::new("/x"),
            r#"{"name":"N","baseModLoader":{"name":"neoforge-21.1.234"}}"#,
        )
        .unwrap();
        assert_eq!(nf.loader_type.as_deref(), Some("neoforge"));
        let f = parse_curseforge_instance(
            Path::new("/x"),
            r#"{"name":"F","baseModLoader":{"name":"forge-47.4.0"}}"#,
        )
        .unwrap();
        assert_eq!(f.loader_type.as_deref(), Some("forge"));
    }

    #[test]
    fn parses_prism_instances() {
        let cfg = "[General]\nInstanceType=OneSix\nname=BonesAndBees\niconKey=default\n";
        let pack = r#"{"components":[
            {"uid":"org.lwjgl3","version":"3.3.3"},
            {"uid":"net.minecraft","version":"1.21.1"},
            {"uid":"net.neoforged","version":"21.1.234"}
        ],"formatVersion":1}"#;
        let p = parse_prism_instance(cfg, Some(pack)).unwrap();
        assert_eq!(p.name, "BonesAndBees");
        assert_eq!(p.mc_version.as_deref(), Some("1.21.1"));
        assert_eq!(p.loader_type.as_deref(), Some("neoforge"));
        assert_eq!(p.loader_version.as_deref(), Some("21.1.234"));

        let vanilla = parse_prism_instance("name=Plain", None).unwrap();
        assert_eq!(vanilla.loader_type, None);
        assert!(
            parse_prism_instance("InstanceType=OneSix", None).is_none(),
            "needs a name"
        );
    }

    #[test]
    fn scans_prism_folders_and_finds_the_game_dir() {
        let dir = std::env::temp_dir().join(format!("bg-prism-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("A/minecraft")).unwrap();
        std::fs::write(dir.join("A/instance.cfg"), "name=Alpha\n").unwrap();
        std::fs::create_dir_all(dir.join("B")).unwrap(); // no instance.cfg -> ignored
        let found = scan_prism_instances(&dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].launcher, LauncherKind::Prism);
        assert_eq!(found[0].path, dir.join("A/minecraft"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_id_parsing() {
        assert_eq!(
            parse_version_id("neoforge-21.1.234"),
            (
                Some("neoforge".into()),
                Some("21.1.234".into()),
                Some("1.21.1".into())
            )
        );
        assert_eq!(
            parse_version_id("forge-47.4.0"),
            (Some("forge".into()), Some("47.4.0".into()), None)
        );
        // Bare Minecraft id -> vanilla profile, MC surfaced, no loader.
        assert_eq!(
            parse_version_id("1.21.1"),
            (None, None, Some("1.21.1".into()))
        );
        let fab = parse_version_id("fabric-loader-0.16-1.21.1");
        assert_eq!(fab.0, Some("fabric".into()));
        assert_eq!(fab.1, None);
        assert_eq!(fab.2, Some("1.21.1".into()));
        // Unrecognised -> all None.
        assert_eq!(parse_version_id("latest-release"), (None, None, None));
    }
}
