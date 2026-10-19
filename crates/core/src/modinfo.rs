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
//!   the file name);
//! * jar-in-jar (`META-INF/jarjar/*.jar`): the bundled mods are reported in
//!   [`ModInfo::provides`], never as the jar's identity — several jars may
//!   bundle the same library (Kotlin for Forge's "all" jar is nothing but one).
//!
//! It also reads the `required` dependencies a client needs (so switching a
//! mod off can warn about the mods that stop working).

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Cursor, Read, Seek};
use std::path::Path;

/// How deep jar-in-jar is followed (NeoForge nests; real packs go 2 deep).
const MAX_NESTING: usize = 3;
/// Bundled jars bigger than this are skipped (no zip bombs in memory).
const MAX_NESTED_BYTES: u64 = 64 * 1024 * 1024;

/// Mod identity extracted from a jar.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModInfo {
    /// All modIds declared in `[[mods]]`, primary (first) first.
    pub mod_ids: Vec<String>,
    /// Best-effort primary mod version (placeholders resolved).
    pub version: Option<String>,
    /// Human-readable name of the primary mod (`displayName`), if declared.
    pub display_name: Option<String>,
    /// modIds the jar needs on a client to load (`required` dependencies of
    /// its own and its bundled mods that are not server-only), minus what it
    /// brings itself. Sorted.
    pub dependencies: Vec<String>,
    /// modIds bundled as jar-in-jar, at any depth. Sorted.
    pub provides: Vec<String>,
}

impl ModInfo {
    pub fn primary_id(&self) -> Option<&str> {
        self.mod_ids.first().map(String::as_str)
    }
}

/// Read mod identity from a jar. Returns `None` for jars without recognisable
/// mod metadata (e.g. pure libraries).
pub fn read_mod_info(jar_path: &Path) -> Option<ModInfo> {
    read_mod_info_from(File::open(jar_path).ok()?)
}

/// Like [`read_mod_info`], for any seekable jar (e.g. an in-memory buffer).
pub fn read_mod_info_from<R: Read + Seek>(jar: R) -> Option<ModInfo> {
    let mut zip = zip::ZipArchive::new(jar).ok()?;
    read_mod_info_from_zip(&mut zip)
}

fn read_mod_info_from_zip<R: Read + Seek>(zip: &mut zip::ZipArchive<R>) -> Option<ModInfo> {
    read_at_depth(zip, 0)
}

fn read_at_depth<R: Read + Seek>(zip: &mut zip::ZipArchive<R>, depth: usize) -> Option<ModInfo> {
    let mut info = own_mods(zip).unwrap_or_default();
    let mut provides = BTreeSet::new();
    let mut needs: BTreeSet<String> = info.dependencies.drain(..).collect();
    if depth < MAX_NESTING {
        for mut nested in nested_jars(zip).into_iter().flatten() {
            if let Some(n) = read_at_depth(&mut nested, depth + 1) {
                provides.extend(n.mod_ids);
                provides.extend(n.provides);
                needs.extend(n.dependencies);
            }
        }
    }
    if info.mod_ids.is_empty() && provides.is_empty() {
        return None;
    }
    for id in info.mod_ids.iter().chain(&provides) {
        needs.remove(id);
    }
    info.dependencies = needs.into_iter().collect();
    info.provides = provides.into_iter().collect();
    Some(info)
}

/// The bundled jars (`META-INF/jarjar/*.jar`), each opened from memory.
fn nested_jars<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Vec<Option<zip::ZipArchive<Cursor<Vec<u8>>>>> {
    let names: Vec<String> = zip
        .file_names()
        .filter(|n| n.starts_with("META-INF/jarjar/") && n.ends_with(".jar"))
        .map(str::to_string)
        .collect();
    names
        .into_iter()
        .map(|name| {
            let f = zip.by_name(&name).ok()?;
            if f.size() > MAX_NESTED_BYTES {
                return None;
            }
            let mut bytes = Vec::new();
            f.take(MAX_NESTED_BYTES).read_to_end(&mut bytes).ok()?;
            zip::ZipArchive::new(Cursor::new(bytes)).ok()
        })
        .collect()
}

/// The jar's own `[[mods]]` entries and dependencies (no jar-in-jar).
fn own_mods<R: Read + Seek>(zip: &mut zip::ZipArchive<R>) -> Option<ModInfo> {
    let toml_str = read_zip_text(zip, "META-INF/neoforge.mods.toml")
        .or_else(|| read_zip_text(zip, "META-INF/mods.toml"))?;

    let value: toml::Value = toml::from_str(&toml_str).ok()?;
    let mods = value.get("mods")?.as_array()?;

    let mut mod_ids = Vec::new();
    let mut first_version: Option<String> = None;
    let mut display_name: Option<String> = None;
    for m in mods {
        if let Some(id) = m.get("modId").and_then(toml::Value::as_str) {
            if id.is_empty() {
                continue;
            }
            if mod_ids.is_empty() {
                first_version = m
                    .get("version")
                    .and_then(toml::Value::as_str)
                    .map(str::to_string);
                display_name = m
                    .get("displayName")
                    .and_then(toml::Value::as_str)
                    .and_then(clean_display_name);
            }
            mod_ids.push(id.to_string());
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

    Some(ModInfo {
        mod_ids,
        version,
        display_name,
        dependencies: required_dependencies(&value),
        provides: Vec::new(),
    })
}

/// `required` dependencies a client must have: NeoForge's `type` (default
/// "required"), legacy Forge's `mandatory`; `side = "SERVER"` ones are left
/// out.
fn required_dependencies(value: &toml::Value) -> Vec<String> {
    let Some(owners) = value.get("dependencies").and_then(toml::Value::as_table) else {
        return Vec::new();
    };
    let mut out = BTreeSet::new();
    for dep in owners.values().filter_map(toml::Value::as_array).flatten() {
        let Some(id) = dep.get("modId").and_then(toml::Value::as_str) else {
            continue;
        };
        let required = match dep.get("type").and_then(toml::Value::as_str) {
            Some(t) => t.eq_ignore_ascii_case("required"),
            None => dep
                .get("mandatory")
                .and_then(toml::Value::as_bool)
                .unwrap_or(true),
        };
        let server_only = dep
            .get("side")
            .and_then(toml::Value::as_str)
            .is_some_and(|s| s.eq_ignore_ascii_case("SERVER"));
        if required && !server_only && !id.is_empty() && !id.contains("${") {
            out.insert(id.to_string());
        }
    }
    out.into_iter().collect()
}

/// A display name fit for one line of UI text: trimmed, single spaces, no
/// control characters or unresolved `${…}` placeholders, at most 64 chars.
fn clean_display_name(raw: &str) -> Option<String> {
    if raw.contains("${") {
        return None;
    }
    let words: Vec<&str> = raw
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect();
    let joined = words.join(" ");
    let name: String = joined.chars().take(64).collect();
    (!name.is_empty()).then_some(name)
}

fn read_zip_text<R: Read + Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Option<String> {
    let mut f = zip.by_name(name).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

fn manifest_impl_version<R: Read + Seek>(zip: &mut zip::ZipArchive<R>) -> Option<String> {
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    /// Build an in-memory jar with the given `(entry name, content)` pairs.
    pub(crate) fn jar(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, content) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn info(entries: &[(&str, &str)]) -> Option<ModInfo> {
        read_mod_info_from(Cursor::new(jar(entries)))
    }

    #[test]
    fn reads_neoforge_mods_toml() {
        let mi = info(&[(
            "META-INF/neoforge.mods.toml",
            "modLoader=\"javafml\"\nloaderVersion=\"[4,)\"\n[[mods]]\nmodId=\"create\"\nversion=\"6.0.10\"\n",
        )])
        .unwrap();
        assert_eq!(mi.mod_ids, vec!["create".to_string()]);
        assert_eq!(mi.primary_id(), Some("create"));
        assert_eq!(mi.version.as_deref(), Some("6.0.10"));
    }

    #[test]
    fn reads_the_display_name_of_the_primary_mod() {
        let mi = info(&[(
            "META-INF/neoforge.mods.toml",
            "[[mods]]\nmodId=\"create\"\nversion=\"6.0.10\"\ndisplayName=\"  Create\\n \"\n\
             [[mods]]\nmodId=\"ponder\"\ndisplayName=\"Ponder\"\n",
        )])
        .unwrap();
        assert_eq!(mi.display_name.as_deref(), Some("Create"));

        let none = info(&[(
            "META-INF/neoforge.mods.toml",
            "[[mods]]\nmodId=\"x\"\ndisplayName=\"${mod_name}\"\n",
        )])
        .unwrap();
        assert_eq!(none.display_name, None, "placeholders are not names");
        assert_eq!(clean_display_name(&"a".repeat(100)).unwrap().len(), 64);
        assert_eq!(clean_display_name(" \t "), None);
    }

    #[test]
    fn falls_back_to_legacy_mods_toml_and_prefers_neoforge() {
        let legacy = info(&[(
            "META-INF/mods.toml",
            "[[mods]]\nmodId=\"jei\"\nversion=\"19.0\"\n",
        )])
        .unwrap();
        assert_eq!(legacy.mod_ids, vec!["jei".to_string()]);

        let both = info(&[
            ("META-INF/mods.toml", "[[mods]]\nmodId=\"old\"\n"),
            ("META-INF/neoforge.mods.toml", "[[mods]]\nmodId=\"new\"\n"),
        ])
        .unwrap();
        assert_eq!(both.mod_ids, vec!["new".to_string()]);
    }

    #[test]
    fn resolves_placeholder_version_from_jar_manifest() {
        let mi = info(&[
            (
                "META-INF/neoforge.mods.toml",
                "[[mods]]\nmodId=\"sodium\"\nversion=\"${file.jarVersion}\"\n",
            ),
            (
                "META-INF/MANIFEST.MF",
                "Manifest-Version: 1.0\r\nImplementation-Version: 0.6.5\r\n",
            ),
        ])
        .unwrap();
        assert_eq!(mi.version.as_deref(), Some("0.6.5"));

        let unresolved = info(&[(
            "META-INF/neoforge.mods.toml",
            "[[mods]]\nmodId=\"x\"\nversion=\"${file.jarVersion}\"\n",
        )])
        .unwrap();
        assert_eq!(
            unresolved.version, None,
            "no MANIFEST.MF -> no made-up version"
        );
    }

    #[test]
    fn collects_every_declared_mod_but_ignores_dependencies() {
        let toml = "[[mods]]\nmodId=\"first\"\nversion=\"1.0\"\n\
                    [[mods]]\nmodId=\"second\"\nversion=\"2.0\"\n\
                    [[mods]]\nmodId=\"\"\n\
                    [[dependencies.first]]\nmodId=\"neoforge\"\ntype=\"required\"\n";
        let mi = info(&[("META-INF/neoforge.mods.toml", toml)]).unwrap();
        assert_eq!(mi.mod_ids, vec!["first".to_string(), "second".to_string()]);
        assert_eq!(
            mi.version.as_deref(),
            Some("1.0"),
            "version of the primary mod"
        );
    }

    /// A jar entry holding another jar.
    fn nested(name: &str, entries: &[(&str, &str)]) -> (String, Vec<u8>) {
        (format!("META-INF/jarjar/{name}"), jar(entries))
    }

    fn jar_with(entries: &[(&str, &str)], nested: &[(String, Vec<u8>)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, content) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }
        for (name, bytes) in nested {
            zip.start_file(name.as_str(), zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn reads_required_client_dependencies() {
        let toml = "[[mods]]\nmodId=\"addon\"\n\
                    [[dependencies.addon]]\nmodId=\"neoforge\"\ntype=\"required\"\n\
                    [[dependencies.addon]]\nmodId=\"create\"\ntype=\"REQUIRED\"\nside=\"BOTH\"\n\
                    [[dependencies.addon]]\nmodId=\"jei\"\ntype=\"optional\"\n\
                    [[dependencies.addon]]\nmodId=\"servercore\"\ntype=\"required\"\nside=\"SERVER\"\n\
                    [[dependencies.addon]]\nmodId=\"oldlib\"\nmandatory=true\n\
                    [[dependencies.addon]]\nmodId=\"maybe\"\nmandatory=false\n\
                    [[dependencies.addon]]\nmodId=\"clientlib\"\nside=\"CLIENT\"\n\
                    [[dependencies.addon]]\nmodId=\"addon\"\n";
        let mi = info(&[("META-INF/neoforge.mods.toml", toml)]).unwrap();
        assert_eq!(
            mi.dependencies,
            vec!["clientlib", "create", "neoforge", "oldlib"],
            "required, not server-only, not itself"
        );
        assert!(mi.provides.is_empty());
    }

    #[test]
    fn bundled_mods_are_provided_not_identity() {
        let ponder = nested(
            "ponder.jar",
            &[(
                "META-INF/neoforge.mods.toml",
                "[[mods]]\nmodId=\"ponder\"\n[[dependencies.ponder]]\nmodId=\"flywheel\"\n",
            )],
        );
        let create = jar_with(
            &[(
                "META-INF/neoforge.mods.toml",
                "[[mods]]\nmodId=\"create\"\n[[dependencies.create]]\nmodId=\"ponder\"\n",
            )],
            &[ponder],
        );
        let mi = read_mod_info_from(Cursor::new(create)).unwrap();
        assert_eq!(mi.mod_ids, vec!["create"]);
        assert_eq!(mi.provides, vec!["ponder"]);
        assert_eq!(
            mi.dependencies,
            vec!["flywheel"],
            "ponder comes along, flywheel not"
        );

        // Kotlin for Forge's "all" jar: no mod of its own, only bundled ones.
        let inner = nested(
            "kff.jar",
            &[(
                "META-INF/neoforge.mods.toml",
                "[[mods]]\nmodId=\"kotlinforforge\"\n",
            )],
        );
        let broken = (
            "META-INF/jarjar/broken.jar".to_string(),
            b"not a zip".to_vec(),
        );
        let kff = read_mod_info_from(Cursor::new(jar_with(&[], &[inner, broken]))).unwrap();
        assert!(kff.mod_ids.is_empty());
        assert_eq!(
            kff.provides,
            vec!["kotlinforforge"],
            "a broken bundle is skipped"
        );
    }

    #[test]
    fn libraries_and_broken_files_have_no_mod_info() {
        assert_eq!(info(&[("com/example/Lib.class", "x")]), None);
        assert_eq!(
            info(&[("META-INF/neoforge.mods.toml", "not = [valid toml")]),
            None
        );
        assert_eq!(
            info(&[("META-INF/neoforge.mods.toml", "[[mods]]\nmodId=\"\"\n")]),
            None
        );
        assert_eq!(
            read_mod_info_from(Cursor::new(b"definitely not a zip".to_vec())),
            None
        );
    }

    #[test]
    fn reads_from_a_file_on_disk() {
        let dir = std::env::temp_dir().join(format!("bonegrader-modinfo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("m.jar");
        std::fs::write(
            &p,
            jar(&[("META-INF/neoforge.mods.toml", "[[mods]]\nmodId=\"m\"\n")]),
        )
        .unwrap();
        assert_eq!(read_mod_info(&p).unwrap().mod_ids, vec!["m".to_string()]);
        assert_eq!(read_mod_info(&dir.join("missing.jar")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
