//! Scan the managed folders of a local instance into [`LocalFile`]s that the
//! diff engine can consume.

use crate::hash::sha1_file;
use crate::manifest::Category;
use crate::modinfo::read_mod_info;
use crate::paths::category_dir;
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

/// Scan the given categories under `instance_dir`. Missing folders are skipped.
///
/// Only files that can actually be managed are returned: `.jar` for mods,
/// `.zip` for resource/shader packs. Disabled files (`*.jar.disabled`,
/// `*.zip.txt`) are intentionally ignored so the player's disable choices are
/// respected.
pub fn scan_instance(
    instance_dir: &Path,
    categories: &[Category],
) -> std::io::Result<Vec<LocalFile>> {
    let mut out = Vec::new();
    for &cat in categories {
        let dir = instance_dir.join(category_dir(cat));
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
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
            let size = entry.metadata()?.len();
            let sha1 = sha1_file(&p)?;
            let (mod_ids, mod_version) = if cat == Category::Mod {
                match read_mod_info(&p) {
                    Some(mi) => (mi.mod_ids, mi.version),
                    None => (Vec::new(), None),
                }
            } else {
                (Vec::new(), None)
            };
            out.push(LocalFile {
                category: cat,
                path: format!("{}/{}", category_dir(cat), file_name),
                file_name,
                size,
                sha1,
                mod_ids,
                mod_version,
            });
        }
    }
    Ok(out)
}

fn is_relevant(cat: Category, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    match cat {
        Category::Mod => lower.ends_with(".jar"),
        Category::Resourcepack | Category::Shaderpack => lower.ends_with(".zip"),
    }
}
