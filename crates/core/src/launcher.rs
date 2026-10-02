//! Launcher metadata shared by the client (instance detection) and the
//! publisher (loader auto-detection): reading CurseForge's
//! `minecraftinstance.json` and naming mod loaders.

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

#[cfg(test)]
mod tests {
    use super::*;

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
