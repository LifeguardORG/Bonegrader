//! Launcher metadata shared by the client (instance detection) and the
//! publisher (loader auto-detection): CurseForge's `minecraftinstance.json`,
//! Prism/MultiMC's `instance.cfg` + `mmc-pack.json`, the game's own
//! `logs/latest.log` (written by every launcher), and naming mod loaders.

/// Normalised loader type for a loader name such as `neoforge-21.1.234`.
/// Unknown names default to `neoforge` (the loader BonesAndBees uses).
pub fn loader_type_from_name(name: &str) -> &'static str {
    let n = name.to_ascii_lowercase();
    // Order matters: "neoforge" also contains "forge".
    if n.contains("neoforge") {
        "neoforge"
    } else if n.contains("fabric") {
        "fabric"
    } else if n.contains("quilt") {
        "quilt"
    } else if n.contains("forge") {
        "forge"
    } else {
        "neoforge"
    }
}

/// The parts of a CurseForge `minecraftinstance.json` Bonegrader uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurseForgeInstance {
    pub name: Option<String>,
    pub mc_version: Option<String>,
    /// Raw loader name, e.g. `neoforge-21.1.234`.
    pub loader_name: Option<String>,
    pub loader_version: Option<String>,
}

impl CurseForgeInstance {
    /// Normalised loader type, if the instance names a loader.
    pub fn loader_type(&self) -> Option<&'static str> {
        self.loader_name.as_deref().map(loader_type_from_name)
    }
}

/// Parse a CurseForge `minecraftinstance.json`. `None` if it is not JSON.
pub fn parse_curseforge_instance(json: &str) -> Option<CurseForgeInstance> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let text = |x: Option<&serde_json::Value>| x.and_then(|x| x.as_str()).map(String::from);
    let bml = v.get("baseModLoader");
    Some(CurseForgeInstance {
        name: text(v.get("name")),
        mc_version: text(v.get("gameVersion"))
            .or_else(|| text(bml.and_then(|b| b.get("minecraftVersion")))),
        loader_name: text(bml.and_then(|b| b.get("name"))),
        loader_version: text(bml.and_then(|b| b.get("forgeVersion"))),
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
    let (mc_version, loader) = mmc_pack.map(mmc_pack_components).unwrap_or_default();
    let (loader_type, loader_version) = loader.unzip();
    Some(PrismInstance {
        name,
        mc_version,
        loader_type,
        loader_version: loader_version.flatten(),
    })
}

/// Loader and Minecraft version from a Prism/MultiMC `mmc-pack.json`; `None`
/// for vanilla instances or when a version is missing.
pub fn loader_from_mmc_pack(mmc_pack: &str) -> Option<LoaderInfo> {
    let (mc_version, loader) = mmc_pack_components(mmc_pack);
    let (loader_type, loader_version) = loader?;
    Some(LoaderInfo {
        loader_type,
        loader_version: loader_version?,
        mc_version: mc_version?,
    })
}

/// Minecraft version and (loader type, loader version) from `mmc-pack.json`.
type PackComponents = (Option<String>, Option<(String, Option<String>)>);

fn mmc_pack_components(mmc_pack: &str) -> PackComponents {
    let pack: serde_json::Value = serde_json::from_str(mmc_pack).unwrap_or_default();
    let mut mc_version = None;
    let mut loader = None;
    for c in pack
        .get("components")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
    {
        let version = c.get("version").and_then(|v| v.as_str()).map(String::from);
        let kind = match c.get("uid").and_then(|u| u.as_str()).unwrap_or("") {
            "net.minecraft" => {
                mc_version = version;
                continue;
            }
            "net.neoforged" => "neoforge",
            "net.minecraftforge" => "forge",
            "net.fabricmc.fabric-loader" => "fabric",
            "org.quiltmc.quilt-loader" => "quilt",
            _ => continue,
        };
        loader = Some((kind.to_string(), version));
    }
    (mc_version, loader)
}

/// Mod loader and Minecraft version, as one source reported them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderInfo {
    pub loader_type: String,
    pub loader_version: String,
    pub mc_version: String,
}

/// Read loader and Minecraft version from a game log (`logs/latest.log`),
/// which every launcher leaves behind after the game ran once:
/// NeoForge/Forge log their launch arguments (`--fml.neoForgeVersion 21.1.234`,
/// `--fml.mcVersion 1.21.1`), Fabric/Quilt say "Loading Minecraft 1.21.1 with
/// Fabric Loader 0.16.5".
pub fn loader_from_game_log(log: &str) -> Option<LoaderInfo> {
    let plain = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-+_".contains(c))
    };
    let tokens: Vec<&str> = log
        .split(|c: char| c.is_whitespace() || ",[]".contains(c))
        .filter(|t| !t.is_empty())
        .collect();
    let arg = |name: &str| {
        tokens
            .windows(2)
            .rev()
            .find(|w| w[0] == name && plain(w[1]))
            .map(|w| w[1].to_string())
    };
    if let Some(mc) = arg("--fml.mcVersion") {
        for (flag, kind) in [
            ("--fml.neoForgeVersion", "neoforge"),
            ("--fml.forgeVersion", "forge"),
        ] {
            if let Some(v) = arg(flag) {
                return Some(LoaderInfo {
                    loader_type: kind.into(),
                    loader_version: v,
                    mc_version: mc,
                });
            }
        }
    }
    for (marker, kind) in [("Fabric Loader", "fabric"), ("Quilt Loader", "quilt")] {
        let found = log.lines().rev().find_map(|line| {
            let rest = &line[line.find("Loading Minecraft ")? + "Loading Minecraft ".len()..];
            let (mc, loader) = rest.split_once(" with ")?;
            let version = loader.trim().strip_prefix(marker)?.trim();
            let version = version.split_whitespace().next()?;
            (plain(mc) && plain(version)).then(|| LoaderInfo {
                loader_type: kind.into(),
                loader_version: version.into(),
                mc_version: mc.into(),
            })
        });
        if found.is_some() {
            return found;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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

        assert_eq!(
            loader_from_mmc_pack(pack),
            Some(LoaderInfo {
                loader_type: "neoforge".into(),
                loader_version: "21.1.234".into(),
                mc_version: "1.21.1".into()
            })
        );
        assert_eq!(
            loader_from_mmc_pack(r#"{"components":[{"uid":"net.minecraft","version":"1.21.1"}]}"#),
            None,
            "vanilla has no loader"
        );

        let vanilla = parse_prism_instance("name=Plain", None).unwrap();
        assert_eq!(vanilla.loader_type, None);
        assert!(
            parse_prism_instance("InstanceType=OneSix", None).is_none(),
            "needs a name"
        );
    }

    #[test]
    fn game_logs_tell_the_loader() {
        let neo = "[10Oct2026 18:00:01.123] [main/INFO] [cpw.mods.modlauncher.Launcher/MODLAUNCHER]: \
                   ModLauncher running: args [--username, Steve, --version, neoforge-21.1.234, --gameDir, \
                   /home/p/.minecraft, --accessToken, ❄❄❄❄❄❄❄❄, --fml.neoForgeVersion, 21.1.234, \
                   --fml.fmlVersion, 4.0.31, --fml.mcVersion, 1.21.1, --fml.neoFormVersion, 20240808.144430, \
                   --launchTarget, forgeclient]";
        assert_eq!(
            loader_from_game_log(neo),
            Some(LoaderInfo {
                loader_type: "neoforge".into(),
                loader_version: "21.1.234".into(),
                mc_version: "1.21.1".into()
            })
        );
        let forge = "args [--fml.forgeVersion, 47.3.0, --fml.mcVersion, 1.20.1]";
        assert_eq!(loader_from_game_log(forge).unwrap().loader_type, "forge");
        let fabric = "[18:00:00] [main/INFO]: Loading Minecraft 1.21.1 with Fabric Loader 0.16.5\n";
        let f = loader_from_game_log(fabric).unwrap();
        assert_eq!(
            (
                f.loader_type.as_str(),
                f.loader_version.as_str(),
                f.mc_version.as_str()
            ),
            ("fabric", "0.16.5", "1.21.1")
        );
        assert_eq!(loader_from_game_log("[main/INFO]: Hello"), None);
        assert_eq!(
            loader_from_game_log("--fml.neoForgeVersion, 21.1.234"),
            None,
            "the Minecraft version is needed too"
        );
    }

    #[test]
    fn loader_names() {
        assert_eq!(loader_type_from_name("neoforge-21.1.234"), "neoforge");
        assert_eq!(loader_type_from_name("forge-47.4.0"), "forge");
        assert_eq!(
            loader_type_from_name("fabric-loader-0.16.5-1.21.1"),
            "fabric"
        );
        assert_eq!(loader_type_from_name("quilt-loader-0.26"), "quilt");
        assert_eq!(loader_type_from_name(""), "neoforge");
    }

    #[test]
    fn parses_curseforge_instance() {
        let json = r#"{
            "name": "BonesAndBees",
            "gameVersion": "1.21.1",
            "baseModLoader": { "name": "neoforge-21.1.234", "forgeVersion": "21.1.234", "minecraftVersion": "1.21.1" }
        }"#;
        let cf = parse_curseforge_instance(json).unwrap();
        assert_eq!(cf.name.as_deref(), Some("BonesAndBees"));
        assert_eq!(cf.mc_version.as_deref(), Some("1.21.1"));
        assert_eq!(cf.loader_type(), Some("neoforge"));
        assert_eq!(cf.loader_version.as_deref(), Some("21.1.234"));
    }

    #[test]
    fn falls_back_to_loader_minecraft_version_and_tolerates_gaps() {
        let cf = parse_curseforge_instance(r#"{"baseModLoader":{"minecraftVersion":"1.20.1"}}"#)
            .unwrap();
        assert_eq!(cf.mc_version.as_deref(), Some("1.20.1"));
        assert_eq!(cf.name, None);
        assert_eq!(cf.loader_type(), None);
        assert!(parse_curseforge_instance("not json").is_none());
    }
}
