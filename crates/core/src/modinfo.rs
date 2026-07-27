//! Extract a stable mod identity from a NeoForge/Forge jar.
//!
//! Identity precedence used across the app: manifest path match → sha1 → modId
//! → file name. This module supplies the `modId` layer. It deliberately reads
//! only `[[mods]]` entries from `neoforge.mods.toml` (or the legacy
//! `mods.toml`) and never the `modId` keys under `[[dependencies.*]]`, which
//! describe *other* mods this one depends on.
//!
//! Real-world cases handled (verified against the BonesAndBees pack):
//! * placeholder versions like `${file.jarVersion}` → resolved from
//!   `META-INF/MANIFEST.MF` (`Implementation-Version`);
//! * jars declaring several `[[mods]]`;
//! * library jars with no mod metadata at all → `None` (caller falls back to
//!   the file name).

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Mod identity extracted from a jar.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModInfo {
    /// All modIds declared in `[[mods]]`, primary (first) first.
    pub mod_ids: Vec<String>,
    /// Best-effort primary mod version (placeholders resolved).
    pub version: Option<String>,
}

impl ModInfo {
    pub fn primary_id(&self) -> Option<&str> {
        self.mod_ids.first().map(String::as_str)
    }
}

/// Read mod identity from a jar. Returns `None` for jars without recognisable
/// mod metadata (e.g. pure libraries).
pub fn read_mod_info(jar_path: &Path) -> Option<ModInfo> {
    let file = File::open(jar_path).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    read_mod_info_from_zip(&mut zip)
}

fn read_mod_info_from_zip(zip: &mut zip::ZipArchive<File>) -> Option<ModInfo> {
    let toml_str = read_zip_text(zip, "META-INF/neoforge.mods.toml")
        .or_else(|| read_zip_text(zip, "META-INF/mods.toml"))?;

    let value: toml::Value = toml::from_str(&toml_str).ok()?;
    let mods = value.get("mods")?.as_array()?;

    let mut mod_ids = Vec::new();
    let mut first_version: Option<String> = None;
    for m in mods {
        if let Some(id) = m.get("modId").and_then(toml::Value::as_str) {
            if id.is_empty() {
                continue;
            }
            mod_ids.push(id.to_string());
            if first_version.is_none() {
                first_version = m
                    .get("version")
                    .and_then(toml::Value::as_str)
                    .map(str::to_string);
            }
        }
    }
    if mod_ids.is_empty() {
        return None;
    }

    // Resolve `${...}` placeholder versions from the jar manifest.
    let version = match first_version {
        Some(v) if v.contains("${") => manifest_impl_version(zip),
        other => other,
    };

    Some(ModInfo { mod_ids, version })
}

fn read_zip_text(zip: &mut zip::ZipArchive<File>, name: &str) -> Option<String> {
    let mut f = zip.by_name(name).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

fn manifest_impl_version(zip: &mut zip::ZipArchive<File>) -> Option<String> {
    let text = read_zip_text(zip, "META-INF/MANIFEST.MF")?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Implementation-Version:") {
            let v = rest.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}
