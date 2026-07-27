//! The server-side manifest: the canonical description of a channel's client
//! files. It is the single source of truth the client diffs against.

use serde::{Deserialize, Serialize};

/// Which managed folder a file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Mod,
    Resourcepack,
    Shaderpack,
}

/// The mod loader a pack requires. `loader_version` drives the optional
/// NeoForge install on the vanilla-launcher path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Loader {
    #[serde(rename = "type")]
    pub loader_type: String,
    pub mc_version: String,
    pub loader_version: String,
}

/// One managed file. `path` is the client-relative target (e.g.
/// `mods/create-1.21.1-6.0.10.jar`); `url` points at the content-addressed
/// blob on the server (`files/by-hash/<sha1>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub category: Category,
    pub path: String,
    pub file_name: String,
    pub size: u64,
    pub sha1: String,
    /// Stable mod identity from `neoforge.mods.toml`. `None` for library jars
    /// without mod metadata and for resource/shader packs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_version: Option<String>,
    pub url: String,
}

/// The whole manifest for one channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    pub pack_name: String,
    pub channel: String,
    pub generated_at: String,
    pub loader: Loader,
    pub files: Vec<FileEntry>,
}

impl Manifest {
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Iterator over just the mod entries.
    pub fn mods(&self) -> impl Iterator<Item = &FileEntry> {
        self.files.iter().filter(|f| f.category == Category::Mod)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_json_with_camel_case() {
        let m = Manifest {
            schema_version: 1,
            pack_name: "BonesAndBees".into(),
            channel: "main".into(),
            generated_at: "2026-07-26T18:42:00Z".into(),
            loader: Loader {
                loader_type: "neoforge".into(),
                mc_version: "1.21.1".into(),
                loader_version: "21.1.222".into(),
            },
            files: vec![FileEntry {
                category: Category::Mod,
                path: "mods/create-1.21.1-6.0.10.jar".into(),
                file_name: "create-1.21.1-6.0.10.jar".into(),
                size: 123,
                sha1: "abc".into(),
                mod_id: Some("create".into()),
                mod_version: Some("6.0.10".into()),
                url: "files/by-hash/abc".into(),
            }],
        };
        let json = m.to_json_pretty().unwrap();
        assert!(json.contains("\"schemaVersion\""));
        assert!(json.contains("\"loaderVersion\""));
        assert!(json.contains("\"modId\""));
        assert!(json.contains("\"category\": \"mod\""));
        let back = Manifest::from_json(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn optional_mod_fields_omitted_for_packs() {
        let e = FileEntry {
            category: Category::Resourcepack,
            path: "resourcepacks/x.zip".into(),
            file_name: "x.zip".into(),
            size: 1,
            sha1: "d".into(),
            mod_id: None,
            mod_version: None,
            url: "files/by-hash/d".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains("modId"));
        assert!(!json.contains("modVersion"));
    }
}
