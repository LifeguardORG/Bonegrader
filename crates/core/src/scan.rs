//! Scan the managed folders of a local instance into [`LocalFile`]s that the
//! diff engine can consume.
//!
//! Hashing every jar on every check is the slow part (hundreds of MB), so a
//! [`ScanCache`] remembers each file's hash and mod metadata keyed by a
//! fingerprint (size, modification time and — where the OS offers them — the
//! change time and inode). Any change to a file changes its fingerprint and
//! forces a re-read.

use crate::hash::sha1_file;
use crate::manifest::Category;
use crate::modinfo::{read_mod_info, ModInfo};
use crate::paths::{category_dir, enabled_path, DISABLED_SUFFIX};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::Metadata;
use std::io;
use std::path::Path;

/// A file found in a managed folder on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFile {
    pub category: Category,
    /// Client-relative path, e.g. `mods/create-1.21.1-6.0.10.jar`.
    pub path: String,
    pub file_name: String,
    pub size: u64,
    pub sha1: String,
    /// Mod ids (mods only; empty otherwise or for metadata-less libs).
    pub mod_ids: Vec<String>,
    pub mod_version: Option<String>,
    /// The primary mod's display name, if the jar declares one.
    pub mod_name: Option<String>,
    /// modIds the jar needs to load on a client (see [`ModInfo::dependencies`]).
    pub dependencies: Vec<String>,
    /// modIds bundled as jar-in-jar (see [`ModInfo::provides`]).
    pub provides: Vec<String>,
    /// Switched off with the launchers' `.disabled` suffix; `path` is then the
    /// real name (`mods/x.jar.disabled`), see [`LocalFile::enabled_path`].
    pub disabled: bool,
}

impl LocalFile {
    /// The path the file has while switched on (`mods/x.jar` for
    /// `mods/x.jar.disabled`) — what manifest paths and the client state use.
    pub fn enabled_path(&self) -> &str {
        enabled_path(&self.path).unwrap_or(&self.path)
    }
}

/// Whether a scan also reads switched-off mods (`*.jar.disabled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisabledMods {
    /// Only what the game loads (what a pack is built from).
    Skip,
    /// Also the mods the player switched off (what the client manages).
    Include,
}

/// Bumped whenever [`CachedFile`] gains mod metadata older entries lack: the
/// metadata of their files is read again once (cheap), their hashes are kept.
const CACHE_VERSION: u32 = 3;

/// Hashes and mod metadata of previously scanned files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanCache {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub entries: BTreeMap<String, CachedFile>,
}

impl Default for ScanCache {
    fn default() -> Self {
        Self {
            version: CACHE_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedFile {
    /// Opaque; compared for equality only.
    pub fingerprint: String,
    pub sha1: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mod_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<String>,
}

impl CachedFile {
    fn info(&self) -> ModInfo {
        ModInfo {
            mod_ids: self.mod_ids.clone(),
            version: self.mod_version.clone(),
            display_name: self.mod_name.clone(),
            dependencies: self.dependencies.clone(),
            provides: self.provides.clone(),
        }
    }
}

/// Scan the given categories under `instance_dir` without a cache. Mods
/// switched off (`*.jar.disabled`) are skipped.
pub fn scan_instance(instance_dir: &Path, categories: &[Category]) -> io::Result<Vec<LocalFile>> {
    scan_instance_cached(
        instance_dir,
        categories,
        DisabledMods::Skip,
        &mut ScanCache::default(),
    )
}

/// Scan the given categories under `instance_dir`. Missing folders are skipped;
/// the result is sorted by path. `cache` is consulted and updated: entries of
/// files that disappeared from a scanned folder are dropped.
///
/// Only files that can actually be managed are returned: `.jar` for mods,
/// `.zip` for resource/shader packs, and — with [`DisabledMods::Include`] —
/// switched-off mods (`*.jar.disabled`). Switched-off packs (`*.zip.txt`) are
/// never returned. I/O errors name the file they concern.
pub fn scan_instance_cached(
    instance_dir: &Path,
    categories: &[Category],
    disabled_mods: DisabledMods,
    cache: &mut ScanCache,
) -> io::Result<Vec<LocalFile>> {
    // An older cache still knows the hashes; only the mod metadata is read again.
    let stale_metadata = cache.version != CACHE_VERSION;
    cache.version = CACHE_VERSION;
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for &cat in categories {
        let dir = instance_dir.join(category_dir(cat));
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&dir).map_err(|e| with_path(e, &dir))? {
            let entry = entry.map_err(|e| with_path(e, &dir))?;
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let file_name = match p.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let Some(disabled) = relevance(cat, &file_name, disabled_mods) else {
                continue;
            };
            let rel = format!("{}/{}", category_dir(cat), file_name);
            let meta = std::fs::metadata(&p).map_err(|e| with_path(e, &p))?;
            let fp = fingerprint(&meta);
            let hit = fp
                .as_ref()
                .and_then(|fp| cache.entries.get(&rel).filter(|c| &c.fingerprint == fp))
                .cloned();
            let read_info = || {
                if cat == Category::Mod {
                    read_mod_info(&p).unwrap_or_default()
                } else {
                    ModInfo::default()
                }
            };
            let (sha1, info) = match hit {
                Some(c) if !stale_metadata => {
                    let info = c.info();
                    (c.sha1, info)
                }
                Some(c) => (c.sha1, read_info()),
                None => (sha1_file(&p).map_err(|e| with_path(e, &p))?, read_info()),
            };
            if let Some(fingerprint) = fp {
                cache.entries.insert(
                    rel.clone(),
                    CachedFile {
                        fingerprint,
                        sha1: sha1.clone(),
                        mod_ids: info.mod_ids.clone(),
                        mod_version: info.version.clone(),
                        mod_name: info.display_name.clone(),
                        dependencies: info.dependencies.clone(),
                        provides: info.provides.clone(),
                    },
                );
            }
            seen.insert(rel.clone());
            out.push(LocalFile {
                category: cat,
                path: rel,
                file_name,
                size: meta.len(),
                sha1,
                mod_ids: info.mod_ids,
                mod_version: info.version,
                mod_name: info.display_name,
                dependencies: info.dependencies,
                provides: info.provides,
                disabled,
            });
        }
    }
    let scanned: Vec<String> = categories
        .iter()
        .map(|c| format!("{}/", category_dir(*c)))
        .collect();
    cache.entries.retain(|path, _| {
        seen.contains(path) || !scanned.iter().any(|d| path.starts_with(d.as_str()))
    });
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// `Some(disabled)` for a file the scan returns, `None` for one it skips.
fn relevance(cat: Category, name: &str, disabled_mods: DisabledMods) -> Option<bool> {
    let lower = name.to_ascii_lowercase();
    match cat {
        Category::Mod if lower.ends_with(".jar") => Some(false),
        Category::Mod
            if disabled_mods == DisabledMods::Include
                && lower.ends_with(&format!(".jar{DISABLED_SUFFIX}")) =>
        {
            Some(true)
        }
        Category::Resourcepack | Category::Shaderpack if lower.ends_with(".zip") => Some(false),
        _ => None,
    }
}

/// Prefix an I/O error with the path it concerns (`io::Error` carries none).
pub fn with_path(e: io::Error, p: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", p.display()))
}

fn fingerprint(meta: &Metadata) -> Option<String> {
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!(
            "{}:{mtime}:{}.{}:{}",
            meta.len(),
            meta.ctime(),
            meta.ctime_nsec(),
            meta.ino()
        ))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Some(format!("{}:{mtime}:{}", meta.len(), meta.creation_time()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Some(format!("{}:{mtime}", meta.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::sha1_bytes;
    use crate::modinfo::tests::jar;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("bonegrader-scan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(p: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    const ALL: [Category; 3] = [Category::Mod, Category::Resourcepack, Category::Shaderpack];

    #[test]
    fn scans_only_manageable_files_and_reads_mod_ids() {
        let inst = tmp("basic");
        let create = jar(&[(
            "META-INF/neoforge.mods.toml",
            "[[mods]]\nmodId=\"create\"\nversion=\"6.0.10\"\ndisplayName=\"Create\"\n",
        )]);
        write(&inst.join("mods/create.jar"), &create);
        write(&inst.join("mods/lib.jar"), &jar(&[("a/B.class", "x")]));
        write(&inst.join("mods/off.jar.disabled"), b"disabled");
        write(&inst.join("mods/readme.txt"), b"no");
        std::fs::create_dir_all(inst.join("mods/sub")).unwrap();
        write(&inst.join("resourcepacks/pack.zip"), b"zip");
        write(&inst.join("resourcepacks/pack.zip.txt"), b"disabled");
        // No shaderpacks folder at all: skipped, not an error.

        let found = scan_instance(&inst, &ALL).unwrap();
        let paths: Vec<&str> = found.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["mods/create.jar", "mods/lib.jar", "resourcepacks/pack.zip"]
        );
        let c = &found[0];
        assert_eq!(c.mod_ids, vec!["create".to_string()]);
        assert_eq!(c.mod_version.as_deref(), Some("6.0.10"));
        assert_eq!(c.mod_name.as_deref(), Some("Create"));
        assert_eq!(c.sha1, sha1_bytes(&create));
        assert_eq!(c.size, create.len() as u64);
        assert!(found[1].mod_ids.is_empty(), "library jar without metadata");
        assert!(found.iter().all(|f| !f.disabled));
        let _ = std::fs::remove_dir_all(&inst);
    }

    #[test]
    fn switched_off_mods_are_read_only_on_request() {
        let inst = tmp("disabled");
        let jei = jar(&[(
            "META-INF/neoforge.mods.toml",
            "[[mods]]\nmodId=\"jei\"\nversion=\"19.5\"\n",
        )]);
        write(&inst.join("mods/jei.jar.disabled"), &jei);
        write(&inst.join("mods/On.JAR.DISABLED"), b"upper case");
        write(&inst.join("mods/a.jar"), b"A");
        write(&inst.join("mods/x.zip.disabled"), b"not a mod");
        write(&inst.join("resourcepacks/pack.zip.disabled"), b"not a pack");

        let mut cache = ScanCache::default();
        let found = scan_instance_cached(&inst, &ALL, DisabledMods::Include, &mut cache).unwrap();
        let got: Vec<(&str, &str, bool)> = found
            .iter()
            .map(|f| (f.path.as_str(), f.enabled_path(), f.disabled))
            .collect();
        assert_eq!(
            got,
            vec![
                ("mods/On.JAR.DISABLED", "mods/On.JAR", true),
                ("mods/a.jar", "mods/a.jar", false),
                ("mods/jei.jar.disabled", "mods/jei.jar", true),
            ]
        );
        assert_eq!(found[2].mod_ids, vec!["jei".to_string()]);
        assert_eq!(found[2].file_name, "jei.jar.disabled");

        let skipped = scan_instance(&inst, &ALL).unwrap();
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        let _ = std::fs::remove_dir_all(&inst);
    }

    #[test]
    fn cache_is_used_while_files_are_unchanged() {
        let inst = tmp("cache");
        write(&inst.join("mods/a.jar"), b"AAAA");
        let mut cache = ScanCache::default();
        let first = scan_instance_cached(&inst, &ALL, DisabledMods::Include, &mut cache).unwrap();
        assert_eq!(first[0].sha1, sha1_bytes(b"AAAA"));

        // Poison the cached hash: an unchanged file must be served from cache.
        cache.entries.get_mut("mods/a.jar").unwrap().sha1 = "from-cache".into();
        let second = scan_instance_cached(&inst, &ALL, DisabledMods::Include, &mut cache).unwrap();
        assert_eq!(second[0].sha1, "from-cache");

        // A changed file (size differs) is re-hashed.
        write(&inst.join("mods/a.jar"), b"BBBBBB");
        let third = scan_instance_cached(&inst, &ALL, DisabledMods::Include, &mut cache).unwrap();
        assert_eq!(third[0].sha1, sha1_bytes(b"BBBBBB"));

        // Vanished files leave the cache; other categories' entries stay.
        cache.entries.insert(
            "shaderpacks/x.zip".into(),
            CachedFile {
                fingerprint: "f".into(),
                sha1: "s".into(),
                mod_ids: vec![],
                mod_version: None,
                mod_name: None,
                dependencies: vec![],
                provides: vec![],
            },
        );
        std::fs::remove_file(inst.join("mods/a.jar")).unwrap();
        scan_instance_cached(&inst, &[Category::Mod], DisabledMods::Include, &mut cache).unwrap();
        assert!(!cache.entries.contains_key("mods/a.jar"));
        assert!(cache.entries.contains_key("shaderpacks/x.zip"));
        let _ = std::fs::remove_dir_all(&inst);
    }

    #[test]
    fn older_caches_keep_their_hashes_but_read_metadata_again() {
        let inst = tmp("cache-version");
        let addon = jar(&[(
            "META-INF/neoforge.mods.toml",
            "[[mods]]\nmodId=\"addon\"\ndisplayName=\"Addon\"\n[[dependencies.addon]]\nmodId=\"create\"\n",
        )]);
        write(&inst.join("mods/a.jar"), &addon);
        let mut cache = ScanCache::default();
        scan_instance_cached(&inst, &ALL, DisabledMods::Include, &mut cache).unwrap();

        // A cache from before dependencies (and display names) were kept:
        // no version field, no metadata. Its hash is poisoned to show that
        // it is still used — nothing is hashed again.
        let entry = cache.entries.get_mut("mods/a.jar").unwrap();
        entry.sha1 = "kept".into();
        entry.mod_name = None;
        entry.dependencies.clear();
        let mut json = serde_json::to_value(&cache).unwrap();
        json.as_object_mut().unwrap().remove("version");
        let mut old: ScanCache = serde_json::from_value(json).unwrap();
        assert_eq!(old.version, 0);

        let found = scan_instance_cached(&inst, &ALL, DisabledMods::Include, &mut old).unwrap();
        assert_eq!(found[0].sha1, "kept", "hash from the cache");
        assert_eq!(
            found[0].mod_name.as_deref(),
            Some("Addon"),
            "metadata read again"
        );
        assert_eq!(found[0].dependencies, vec!["create".to_string()]);
        assert_eq!(old.version, CACHE_VERSION);
        let again = scan_instance_cached(&inst, &ALL, DisabledMods::Include, &mut old).unwrap();
        assert_eq!(again, found, "served from the upgraded cache");
        let _ = std::fs::remove_dir_all(&inst);
    }

    #[test]
    fn errors_name_the_file() {
        let e = with_path(
            io::Error::other("Zugriff verweigert"),
            Path::new("mods/x.jar"),
        );
        assert!(e.to_string().contains("mods/x.jar"), "{e}");
    }
}
