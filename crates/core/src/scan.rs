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
use crate::modinfo::read_mod_info;
use crate::paths::category_dir;
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
}

/// Hashes and mod metadata of previously scanned files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanCache {
    #[serde(default)]
    pub entries: BTreeMap<String, CachedFile>,
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
}

/// Scan the given categories under `instance_dir` without a cache.
pub fn scan_instance(instance_dir: &Path, categories: &[Category]) -> io::Result<Vec<LocalFile>> {
    scan_instance_cached(instance_dir, categories, &mut ScanCache::default())
}

/// Scan the given categories under `instance_dir`. Missing folders are skipped;
/// the result is sorted by path. `cache` is consulted and updated: entries of
/// files that disappeared from a scanned folder are dropped.
///
/// Only files that can actually be managed are returned: `.jar` for mods,
/// `.zip` for resource/shader packs. Disabled files (`*.jar.disabled`,
/// `*.zip.txt`) are intentionally ignored so the player's disable choices are
/// respected. I/O errors name the file they concern.
pub fn scan_instance_cached(
    instance_dir: &Path,
    categories: &[Category],
    cache: &mut ScanCache,
) -> io::Result<Vec<LocalFile>> {
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
            if !is_relevant(cat, &file_name) {
                continue;
            }
            let rel = format!("{}/{}", category_dir(cat), file_name);
            let meta = std::fs::metadata(&p).map_err(|e| with_path(e, &p))?;
            let fp = fingerprint(&meta);
            let hit = fp
                .as_ref()
                .and_then(|fp| cache.entries.get(&rel).filter(|c| &c.fingerprint == fp))
                .cloned();
            let (sha1, mod_ids, mod_version) = match hit {
                Some(c) => (c.sha1, c.mod_ids, c.mod_version),
                None => {
                    let sha1 = sha1_file(&p).map_err(|e| with_path(e, &p))?;
                    let (ids, version) = if cat == Category::Mod {
                        read_mod_info(&p)
                            .map(|mi| (mi.mod_ids, mi.version))
                            .unwrap_or_default()
                    } else {
                        (Vec::new(), None)
                    };
                    (sha1, ids, version)
                }
            };
            if let Some(fingerprint) = fp {
                cache.entries.insert(
                    rel.clone(),
                    CachedFile {
                        fingerprint,
                        sha1: sha1.clone(),
                        mod_ids: mod_ids.clone(),
                        mod_version: mod_version.clone(),
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
                mod_ids,
                mod_version,
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

fn is_relevant(cat: Category, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    match cat {
        Category::Mod => lower.ends_with(".jar"),
        Category::Resourcepack | Category::Shaderpack => lower.ends_with(".zip"),
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
            "[[mods]]\nmodId=\"create\"\nversion=\"6.0.10\"\n",
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
        assert_eq!(c.sha1, sha1_bytes(&create));
        assert_eq!(c.size, create.len() as u64);
        assert!(found[1].mod_ids.is_empty(), "library jar without metadata");
        let _ = std::fs::remove_dir_all(&inst);
    }

    #[test]
    fn cache_is_used_while_files_are_unchanged() {
        let inst = tmp("cache");
        write(&inst.join("mods/a.jar"), b"AAAA");
        let mut cache = ScanCache::default();
        let first = scan_instance_cached(&inst, &ALL, &mut cache).unwrap();
        assert_eq!(first[0].sha1, sha1_bytes(b"AAAA"));

        // Poison the cached hash: an unchanged file must be served from cache.
        cache.entries.get_mut("mods/a.jar").unwrap().sha1 = "from-cache".into();
        let second = scan_instance_cached(&inst, &ALL, &mut cache).unwrap();
        assert_eq!(second[0].sha1, "from-cache");

        // A changed file (size differs) is re-hashed.
        write(&inst.join("mods/a.jar"), b"BBBBBB");
        let third = scan_instance_cached(&inst, &ALL, &mut cache).unwrap();
        assert_eq!(third[0].sha1, sha1_bytes(b"BBBBBB"));

        // Vanished files leave the cache; other categories' entries stay.
        cache.entries.insert(
            "shaderpacks/x.zip".into(),
            CachedFile {
                fingerprint: "f".into(),
                sha1: "s".into(),
                mod_ids: vec![],
                mod_version: None,
            },
        );
        std::fs::remove_file(inst.join("mods/a.jar")).unwrap();
        scan_instance_cached(&inst, &[Category::Mod], &mut cache).unwrap();
        assert!(!cache.entries.contains_key("mods/a.jar"));
        assert!(cache.entries.contains_key("shaderpacks/x.zip"));
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
