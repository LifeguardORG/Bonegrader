//! Reusable publisher logic shared by the `bonegrader-publish` CLI and the admin
//! GUI: scan an instance, build a channel manifest, diff it against what's live,
//! populate the content-addressed store and upload a channel to the server.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use bonegrader_core::manifest::{Category, FileEntry, Loader, Manifest};
use bonegrader_core::scan::{scan_instance, LocalFile};
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

const CATEGORIES: [Category; 3] = [Category::Mod, Category::Resourcepack, Category::Shaderpack];

pub struct DetectedLoader {
    pub loader_type: String,
    pub mc_version: String,
    pub loader_version: String,
}

/// Best-effort loader detection from a CurseForge `minecraftinstance.json`.
pub fn detect_loader(instance_dir: &Path) -> Option<DetectedLoader> {
    let text = std::fs::read_to_string(instance_dir.join("minecraftinstance.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let bml = v.get("baseModLoader")?;
    let mc_version = bml
        .get("minecraftVersion")
        .and_then(|x| x.as_str())
        .or_else(|| v.get("gameVersion").and_then(|x| x.as_str()))?
        .to_string();
    let loader_version = bml.get("forgeVersion").and_then(|x| x.as_str())?.to_string();
    let name = bml.get("name").and_then(|x| x.as_str()).unwrap_or_default();
    let loader_type = loader_type_from_name(name).to_string();
    Some(DetectedLoader { loader_type, mc_version, loader_version })
}

fn loader_type_from_name(name: &str) -> &'static str {
    let n = name.to_ascii_lowercase();
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

pub struct BuildOptions {
    pub pack: String,
    pub channel: String,
    pub base_url: String,
    pub mc_version: Option<String>,
    pub loader_version: Option<String>,
    pub loader_type: String,
    pub ignore: Vec<String>,
}

pub struct Built {
    pub manifest: Manifest,
    pub local: Vec<LocalFile>,
    pub ignored: usize,
    /// Mod paths without a stable modId (they fall back to file-name identity).
    pub no_modid: Vec<String>,
}

/// Scan an instance and build its channel manifest (nothing is written).
pub fn build_manifest(instance: &Path, opts: &BuildOptions) -> Result<Built> {
    if !instance.is_dir() {
        bail!("instance path is not a directory: {}", instance.display());
    }
    let detected = detect_loader(instance);
    let mc_version = opts
        .mc_version
        .clone()
        .or_else(|| detected.as_ref().map(|d| d.mc_version.clone()))
        .context("could not determine mc version (pass it explicitly)")?;
    let loader_version = opts
        .loader_version
        .clone()
        .or_else(|| detected.as_ref().map(|d| d.loader_version.clone()))
        .context("could not determine loader version (pass it explicitly)")?;
    let loader_type = detected
        .as_ref()
        .map(|d| d.loader_type.clone())
        .unwrap_or_else(|| opts.loader_type.clone());

    let mut local = scan_instance(instance, &CATEGORIES)
        .with_context(|| format!("scanning instance {}", instance.display()))?;

    let ignore: BTreeSet<&str> = opts.ignore.iter().map(String::as_str).collect();
    let before = local.len();
    local.retain(|lf| !ignore.contains(lf.file_name.as_str()) && !ignore.contains(lf.path.as_str()));
    let ignored = before - local.len();
    local.sort_by(|a, b| a.path.cmp(&b.path));

    let no_modid = local
        .iter()
        .filter(|lf| lf.category == Category::Mod && lf.mod_ids.is_empty())
        .map(|lf| lf.path.clone())
        .collect();

    let files = local.iter().map(|lf| build_entry(lf, &opts.base_url)).collect();
    let manifest = Manifest {
        schema_version: 1,
        pack_name: opts.pack.clone(),
        channel: opts.channel.clone(),
        generated_at: OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default(),
        loader: Loader { loader_type, mc_version, loader_version },
        files,
    };
    Ok(Built { manifest, local, ignored, no_modid })
}

fn build_entry(lf: &LocalFile, base_url: &str) -> FileEntry {
    let rel = format!("files/by-hash/{}", lf.sha1);
    let url = if base_url.is_empty() {
        rel
    } else {
        format!("{}/{}", base_url.trim_end_matches('/'), rel)
    };
    FileEntry {
        category: lf.category,
        path: lf.path.clone(),
        file_name: lf.file_name.clone(),
        size: lf.size,
        sha1: lf.sha1.clone(),
        mod_id: lf.mod_ids.first().cloned(),
        mod_version: lf.mod_version.clone(),
        url,
    }
}

pub fn category_counts(m: &Manifest) -> (usize, usize, usize) {
    let mut c = (0, 0, 0);
    for f in &m.files {
        match f.category {
            Category::Mod => c.0 += 1,
            Category::Resourcepack => c.1 += 1,
            Category::Shaderpack => c.2 += 1,
        }
    }
    c
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModUpdate {
    pub mod_id: String,
    pub old_file: String,
    pub new_file: String,
}

/// Structured diff of a new manifest against the previously published one.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestDiff {
    pub first_build: bool,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub updated: Vec<ModUpdate>,
}

impl ManifestDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty() && self.updated.is_empty()
    }
}

/// Diff `new` against `old`, treating same-`modId` renames as version updates.
pub fn diff_manifests(old: Option<&Manifest>, new: &Manifest) -> ManifestDiff {
    let old = match old {
        Some(o) => o,
        None => {
            return ManifestDiff {
                first_build: true,
                added: new.files.iter().map(|f| f.path.clone()).collect(),
                ..Default::default()
            };
        }
    };

    let old_by_path: BTreeMap<&str, &FileEntry> =
        old.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let new_by_path: BTreeMap<&str, &FileEntry> =
        new.files.iter().map(|f| (f.path.as_str(), f)).collect();

    let mut added: Vec<&FileEntry> =
        new.files.iter().filter(|f| !old_by_path.contains_key(f.path.as_str())).collect();
    let mut removed: Vec<&FileEntry> =
        old.files.iter().filter(|f| !new_by_path.contains_key(f.path.as_str())).collect();
    let changed: Vec<String> = new
        .files
        .iter()
        .filter(|f| old_by_path.get(f.path.as_str()).is_some_and(|o| o.sha1 != f.sha1))
        .map(|f| f.path.clone())
        .collect();

    let old_mod_by_id = mods_by_id(old);
    let new_mod_by_id = mods_by_id(new);
    let mut updated = Vec::new();
    for (mid, nf) in &new_mod_by_id {
        if let Some(of) = old_mod_by_id.get(mid) {
            if of.path != nf.path {
                updated.push(ModUpdate {
                    mod_id: mid.clone(),
                    old_file: of.file_name.clone(),
                    new_file: nf.file_name.clone(),
                });
            }
        }
    }
    let updated_ids: BTreeSet<&str> = updated.iter().map(|u| u.mod_id.as_str()).collect();
    added.retain(|f| f.mod_id.as_deref().is_none_or(|id| !updated_ids.contains(id)));
    removed.retain(|f| f.mod_id.as_deref().is_none_or(|id| !updated_ids.contains(id)));

    ManifestDiff {
        first_build: false,
        added: added.iter().map(|f| f.path.clone()).collect(),
        removed: removed.iter().map(|f| f.path.clone()).collect(),
        changed,
        updated,
    }
}

fn mods_by_id(m: &Manifest) -> BTreeMap<String, &FileEntry> {
    let mut out = BTreeMap::new();
    for f in m.mods() {
        if let Some(id) = &f.mod_id {
            out.insert(id.clone(), f);
        }
    }
    out
}

/// Copy each scanned file into `out/files/by-hash/` if absent. Returns new-blob count.
pub fn populate_store(instance: &Path, out: &Path, local: &[LocalFile]) -> Result<usize> {
    let store = out.join("files").join("by-hash");
    std::fs::create_dir_all(&store).with_context(|| format!("creating store {}", store.display()))?;
    let mut copied = 0;
    for lf in local {
        let dst = store.join(&lf.sha1);
        if dst.exists() {
            continue;
        }
        let src = instance.join(&lf.path);
        let tmp = store.join(format!(".{}.tmp", lf.sha1));
        std::fs::copy(&src, &tmp).with_context(|| format!("copying {} into store", src.display()))?;
        std::fs::rename(&tmp, &dst)?;
        copied += 1;
    }
    Ok(copied)
}

/// Delete store blobs no longer referenced by `manifest`. Returns removed count.
pub fn gc_store(out: &Path, manifest: &Manifest) -> Result<usize> {
    let keep: BTreeSet<&str> = manifest.files.iter().map(|f| f.sha1.as_str()).collect();
    let store = out.join("files").join("by-hash");
    if !store.is_dir() {
        return Ok(0);
    }
    let mut removed = 0;
    for entry in std::fs::read_dir(&store)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if !keep.contains(name.as_ref()) {
            std::fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Write `out/manifest.json` atomically.
pub fn write_manifest(out: &Path, manifest: &Manifest) -> Result<()> {
    let path = out.join("manifest.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, manifest.to_json_pretty()?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// rsync a built channel dir to `<ssh_host>:<remote_base>/<channel>`: blobs first
/// (additive, immutable), manifest last (atomic switch). Requires `rsync`/`ssh`.
pub fn upload_channel(out: &Path, ssh_host: &str, remote_base: &str, channel: &str) -> Result<()> {
    let dest = format!("{remote_base}/{channel}");
    run(Command::new("ssh").arg(ssh_host).arg(format!("mkdir -p '{dest}/files/by-hash'")))?;
    run(Command::new("rsync")
        .args(["-a", "--ignore-existing"])
        .arg(format!("{}/", out.join("files").display()))
        .arg(format!("{ssh_host}:{dest}/files/")))?;
    run(Command::new("rsync")
        .arg("-a")
        .arg(out.join("manifest.json"))
        .arg(format!("{ssh_host}:{dest}/manifest.json.tmp")))?;
    run(Command::new("ssh")
        .arg(ssh_host)
        .arg(format!("mv -f '{dest}/manifest.json.tmp' '{dest}/manifest.json'")))?;
    Ok(())
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("running {cmd:?}"))?;
    if !status.success() {
        bail!("command failed ({:?}): {cmd:?}", status.code());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bonegrader_core::manifest::{Category, FileEntry, Loader};

    fn fe(path: &str, sha1: &str, mod_id: Option<&str>) -> FileEntry {
        FileEntry {
            category: Category::Mod,
            path: path.into(),
            file_name: path.rsplit('/').next().unwrap().into(),
            size: 1,
            sha1: sha1.into(),
            mod_id: mod_id.map(str::to_string),
            mod_version: None,
            url: format!("files/by-hash/{sha1}"),
        }
    }
    fn man(files: Vec<FileEntry>) -> Manifest {
        Manifest {
            schema_version: 1,
            pack_name: "P".into(),
            channel: "main".into(),
            generated_at: "t".into(),
            loader: Loader { loader_type: "neoforge".into(), mc_version: "1.21.1".into(), loader_version: "21.1.234".into() },
            files,
        }
    }

    #[test]
    fn first_build_lists_all_as_added() {
        let d = diff_manifests(None, &man(vec![fe("mods/a.jar", "s1", Some("a"))]));
        assert!(d.first_build);
        assert_eq!(d.added, vec!["mods/a.jar".to_string()]);
    }

    #[test]
    fn version_bump_is_an_update_not_add_remove() {
        let old = man(vec![fe("mods/create-6.0.10.jar", "s10", Some("create"))]);
        let new = man(vec![fe("mods/create-6.0.11.jar", "s11", Some("create"))]);
        let d = diff_manifests(Some(&old), &new);
        assert!(d.added.is_empty() && d.removed.is_empty(), "{d:?}");
        assert_eq!(d.updated.len(), 1);
        assert_eq!(d.updated[0].mod_id, "create");
        assert_eq!(d.updated[0].new_file, "create-6.0.11.jar");
    }

    #[test]
    fn add_remove_change_classified() {
        let old = man(vec![fe("mods/keep.jar", "k1", Some("keep")), fe("mods/gone.jar", "g1", Some("gone"))]);
        let new = man(vec![fe("mods/keep.jar", "k2", Some("keep")), fe("mods/new.jar", "n1", Some("new"))]);
        let d = diff_manifests(Some(&old), &new);
        assert_eq!(d.added, vec!["mods/new.jar".to_string()]);
        assert_eq!(d.removed, vec!["mods/gone.jar".to_string()]);
        assert_eq!(d.changed, vec!["mods/keep.jar".to_string()]);
    }
}
