//! The server-side manifest: the canonical description of a channel's client
//! files. It is the single source of truth the client diffs against.
//!
//! Compatibility rule: new fields are optional and old clients ignore unknown
//! fields, so additive changes keep [`SCHEMA_VERSION`]. A breaking change bumps
//! it, and clients refuse manifests newer than they understand (instead of
//! silently misreading them).

use crate::paths::{is_safe_managed_path, is_safe_seed_path, path_matches_category};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Manifest format version this build writes and understands.
pub const SCHEMA_VERSION: u32 = 1;

/// Which managed folder a file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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
    /// SHA-256 of the content. Optional so manifests built before it existed
    /// stay valid; verified on download whenever present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Stable mod identity from `neoforge.mods.toml`. `None` for library jars
    /// without mod metadata and for resource/shader packs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_version: Option<String>,
    /// Human-readable mod name (`displayName`), shown to players instead of
    /// the file name. Optional: older manifests and libraries have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_name: Option<String>,
    pub url: String,
}

/// Which Bonegrader versions a manifest is meant for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    /// Oldest Bonegrader version allowed to apply this manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_version: Option<String>,
    /// Newest released Bonegrader version (shown to players as an update hint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    /// Where players download Bonegrader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
}

/// The Minecraft server the pack belongs to. Clients add it to the in-game
/// multiplayer list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub name: String,
    pub address: String,
}

/// A file installed only if it is missing (e.g. a default config). Seeds are
/// never overwritten or removed, so the player's own changes always win.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedEntry {
    /// Instance-relative path, e.g. `config/xaerominimap.txt`.
    pub path: String,
    pub size: u64,
    pub sha1: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seeds: Vec<SeedEntry>,
}

/// Structural problems that make a manifest unusable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error(
        "Das Manifest nutzt Format-Version {found}, diese Bonegrader-Version kennt nur \
         bis Version {supported} – bitte Bonegrader aktualisieren."
    )]
    UnsupportedSchema { found: u32, supported: u32 },
    #[error("unsicherer oder ungültiger Pfad im Manifest: {0}")]
    BadPath(String),
    #[error("{0}: Kategorie passt nicht zum Ordner")]
    CategoryMismatch(String),
    #[error("{0}: Dateiname passt nicht zum Pfad")]
    FileNameMismatch(String),
    #[error("{0}: ungültige Prüfsumme")]
    BadChecksum(String),
    #[error("Pfad doppelt im Manifest: {0}")]
    DuplicatePath(String),
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

    /// Check everything a client relies on before acting on the manifest:
    /// supported schema, safe paths that match their category, well-formed
    /// checksums and no duplicate targets.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.schema_version > SCHEMA_VERSION {
            return Err(ManifestError::UnsupportedSchema {
                found: self.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        let mut seen = BTreeSet::new();
        for f in &self.files {
            if !is_safe_managed_path(&f.path) {
                return Err(ManifestError::BadPath(f.path.clone()));
            }
            if !path_matches_category(&f.path, f.category) {
                return Err(ManifestError::CategoryMismatch(f.path.clone()));
            }
            if f.path.rsplit('/').next() != Some(f.file_name.as_str()) {
                return Err(ManifestError::FileNameMismatch(f.path.clone()));
            }
            check_checksums(&f.path, &f.sha1, f.sha256.as_deref())?;
            if !seen.insert(f.path.as_str()) {
                return Err(ManifestError::DuplicatePath(f.path.clone()));
            }
        }
        for s in &self.seeds {
            if !is_safe_seed_path(&s.path) {
                return Err(ManifestError::BadPath(s.path.clone()));
            }
            check_checksums(&s.path, &s.sha1, s.sha256.as_deref())?;
            if !seen.insert(s.path.as_str()) {
                return Err(ManifestError::DuplicatePath(s.path.clone()));
            }
        }
        Ok(())
    }
}

fn check_checksums(path: &str, sha1: &str, sha256: Option<&str>) -> Result<(), ManifestError> {
    let ok = is_lower_hex(sha1, 40) && sha256.is_none_or(|s| is_lower_hex(s, 64));
    if ok {
        Ok(())
    } else {
        Err(ManifestError::BadChecksum(path.to_string()))
    }
}

/// True for exactly `len` lowercase hex digits (the form our hashes use; it
/// also makes the value safe to use as a blob file name).
pub fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA_A: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
    const SHA_B: &str = "a9993e364706816aba3e25717850c26c9cd0d89d";

    fn entry(cat: Category, path: &str, sha1: &str) -> FileEntry {
        FileEntry {
            category: cat,
            path: path.into(),
            file_name: path.rsplit('/').next().unwrap().into(),
            size: 1,
            sha1: sha1.into(),
            sha256: None,
            mod_id: None,
            mod_version: None,
            mod_name: None,
            url: format!("files/by-hash/{sha1}"),
        }
    }

    fn manifest(files: Vec<FileEntry>) -> Manifest {
        Manifest {
            schema_version: SCHEMA_VERSION,
            pack_name: "BonesAndBees".into(),
            channel: "main".into(),
            generated_at: "2026-07-26T18:42:00Z".into(),
            loader: Loader {
                loader_type: "neoforge".into(),
                mc_version: "1.21.1".into(),
                loader_version: "21.1.222".into(),
            },
            files,
            client: None,
            server: None,
            seeds: vec![],
        }
    }

    #[test]
    fn roundtrips_through_json_with_camel_case() {
        let mut m = manifest(vec![FileEntry {
            mod_id: Some("create".into()),
            mod_version: Some("6.0.10".into()),
            mod_name: None,
            sha256: Some("ab".repeat(32)),
            ..entry(Category::Mod, "mods/create-1.21.1-6.0.10.jar", SHA_A)
        }]);
        m.client = Some(ClientInfo {
            min_version: Some("1.2.0".into()),
            latest_version: Some("1.2.1".into()),
            download_url: Some("https://example.com/".into()),
        });
        m.server = Some(ServerInfo {
            name: "BonesAndBees".into(),
            address: "play.example.com".into(),
        });
        let json = m.to_json_pretty().unwrap();
        assert!(json.contains("\"schemaVersion\""));
        assert!(json.contains("\"loaderVersion\""));
        assert!(json.contains("\"modId\""));
        assert!(json.contains("\"minVersion\""));
        assert!(json.contains("\"category\": \"mod\""));
        let back = Manifest::from_json(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn optional_fields_are_omitted_and_old_manifests_still_parse() {
        let e = entry(Category::Resourcepack, "resourcepacks/x.zip", SHA_A);
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains("modId"));
        assert!(!json.contains("modVersion"));
        assert!(!json.contains("sha256"));

        let m = manifest(vec![e]);
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("client") && !json.contains("server") && !json.contains("seeds"));

        // A manifest written by an older publisher (no new fields) parses.
        let old = r#"{"schemaVersion":1,"packName":"P","channel":"main","generatedAt":"t",
            "loader":{"type":"neoforge","mcVersion":"1.21.1","loaderVersion":"21.1.1"},"files":[]}"#;
        let parsed = Manifest::from_json(old).unwrap();
        assert!(parsed.client.is_none() && parsed.seeds.is_empty());
    }

    #[test]
    fn unknown_fields_from_newer_publishers_are_ignored() {
        let newer = r#"{"schemaVersion":1,"packName":"P","channel":"main","generatedAt":"t",
            "loader":{"type":"neoforge","mcVersion":"1.21.1","loaderVersion":"21.1.1"},
            "files":[],"somethingNew":{"x":1}}"#;
        assert!(Manifest::from_json(newer).is_ok());
    }

    #[test]
    fn validate_accepts_a_good_manifest() {
        let mut m = manifest(vec![
            entry(Category::Mod, "mods/a.jar", SHA_A),
            entry(Category::Shaderpack, "shaderpacks/b.zip", SHA_B),
        ]);
        m.seeds.push(SeedEntry {
            path: "config/jei/jei-client.ini".into(),
            size: 1,
            sha1: SHA_B.into(),
            sha256: None,
            url: format!("files/by-hash/{SHA_B}"),
        });
        assert_eq!(m.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_newer_schema() {
        let mut m = manifest(vec![]);
        m.schema_version = SCHEMA_VERSION + 1;
        assert!(matches!(
            m.validate(),
            Err(ManifestError::UnsupportedSchema { .. })
        ));
    }

    #[test]
    fn validate_rejects_bad_entries() {
        let bad_path = manifest(vec![entry(Category::Mod, "mods/../evil.jar", SHA_A)]);
        assert!(matches!(
            bad_path.validate(),
            Err(ManifestError::BadPath(_))
        ));

        let wrong_dir = manifest(vec![entry(Category::Mod, "shaderpacks/a.zip", SHA_A)]);
        assert!(matches!(
            wrong_dir.validate(),
            Err(ManifestError::CategoryMismatch(_))
        ));

        let mut wrong_name = entry(Category::Mod, "mods/a.jar", SHA_A);
        wrong_name.file_name = "b.jar".into();
        assert!(matches!(
            manifest(vec![wrong_name]).validate(),
            Err(ManifestError::FileNameMismatch(_))
        ));

        let bad_sha = manifest(vec![entry(Category::Mod, "mods/a.jar", "../../etc")]);
        assert!(matches!(
            bad_sha.validate(),
            Err(ManifestError::BadChecksum(_))
        ));

        let dup = manifest(vec![
            entry(Category::Mod, "mods/a.jar", SHA_A),
            entry(Category::Mod, "mods/a.jar", SHA_B),
        ]);
        assert!(matches!(
            dup.validate(),
            Err(ManifestError::DuplicatePath(_))
        ));

        let mut bad_seed = manifest(vec![]);
        bad_seed.seeds.push(SeedEntry {
            path: "saves/world/level.dat".into(),
            size: 1,
            sha1: SHA_A.into(),
            sha256: None,
            url: "u".into(),
        });
        assert!(matches!(
            bad_seed.validate(),
            Err(ManifestError::BadPath(_))
        ));
    }
}
