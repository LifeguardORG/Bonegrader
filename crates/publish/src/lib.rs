//! Reusable publisher logic shared by the `bonegrader-publish` CLI and the admin
//! GUI: scan an instance, build and check a channel manifest, diff it against
//! what's live, sign it and populate the content-addressed store. Talking to
//! the server (upload, history, rollback) lives in [`remote`].

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use bonegrader_core::hash::digest_file;
use bonegrader_core::launcher::{
    loader_from_game_log, loader_from_mmc_pack, loader_type_from_name, parse_curseforge_instance,
    LoaderInfo,
};
use bonegrader_core::manifest::{
    Category, ClientInfo, FileEntry, Loader, Manifest, SeedEntry, ServerInfo, SCHEMA_VERSION,
};
use bonegrader_core::paths::{is_safe_component, is_safe_seed_path, SEED_DIRS, SEED_FILES};
use bonegrader_core::scan::{scan_instance, LocalFile};
use bonegrader_core::sign::{PublicKey, SecretKey, SIGNATURE_FILE};

pub mod remote;
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

const CATEGORIES: [Category; 3] = [Category::Mod, Category::Resourcepack, Category::Shaderpack];

/// Where the manifest's Minecraft and loader versions came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LoaderSource {
    /// Entered by hand (`--mc-version` … / the admin's "Erweitert" fields).
    Manual,
    /// CurseForge's `minecraftinstance.json`.
    CurseForge,
    /// Prism Launcher's / MultiMC's `mmc-pack.json`.
    Prism,
    /// The game's last start (`logs/latest.log`) — works for every launcher.
    GameLog,
    /// Taken over from the manifest published before.
    Previous,
}

impl LoaderSource {
    pub fn describe(self) -> &'static str {
        match self {
            LoaderSource::Manual => "von Hand eingetragen",
            LoaderSource::CurseForge => "aus der CurseForge-Instanz",
            LoaderSource::Prism => "aus der Prism-/MultiMC-Instanz",
            LoaderSource::GameLog => "aus dem letzten Spielstart (logs/latest.log)",
            LoaderSource::Previous => "vom bisher veröffentlichten Manifest übernommen",
        }
    }
}

/// Loader and Minecraft version found in an instance, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedLoader {
    pub loader_type: String,
    pub mc_version: String,
    pub loader_version: String,
    pub source: LoaderSource,
}

impl DetectedLoader {
    fn new(info: LoaderInfo, source: LoaderSource) -> Self {
        DetectedLoader {
            loader_type: info.loader_type,
            mc_version: info.mc_version,
            loader_version: info.loader_version,
            source,
        }
    }
}

/// The folder holding `mods/`, `config/` …: the instance folder itself, or —
/// when a Prism/MultiMC instance folder was picked — its `minecraft` /
/// `.minecraft` subfolder.
pub fn game_dir(instance: &Path) -> PathBuf {
    if instance.join("mods").is_dir() {
        return instance.to_path_buf();
    }
    [".minecraft", "minecraft"]
        .iter()
        .map(|sub| instance.join(sub))
        .find(|dir| dir.join("mods").is_dir())
        .unwrap_or_else(|| instance.to_path_buf())
}

/// Best-effort loader detection, launcher metadata first: CurseForge's
/// `minecraftinstance.json`, Prism/MultiMC's `mmc-pack.json` (next to the game
/// folder), then the log of the game's last start, which every launcher
/// leaves in `logs/latest.log`.
pub fn detect_loader(instance: &Path) -> Option<DetectedLoader> {
    let game = game_dir(instance);
    let read = |p: PathBuf| std::fs::read_to_string(p).ok();
    let curseforge = || {
        let cf = parse_curseforge_instance(&read(game.join("minecraftinstance.json"))?)?;
        Some(DetectedLoader {
            loader_type: loader_type_from_name(cf.loader_name.as_deref().unwrap_or_default())
                .to_string(),
            mc_version: cf.mc_version?,
            loader_version: cf.loader_version?,
            source: LoaderSource::CurseForge,
        })
    };
    let prism = || {
        [Some(instance), game.parent()]
            .into_iter()
            .flatten()
            .find_map(|dir| loader_from_mmc_pack(&read(dir.join("mmc-pack.json"))?))
            .map(|info| DetectedLoader::new(info, LoaderSource::Prism))
    };
    let game_log = || {
        loader_from_game_log(&read_log_head(&game.join("logs/latest.log"))?)
            .map(|info| DetectedLoader::new(info, LoaderSource::GameLog))
    };
    curseforge().or_else(prism).or_else(game_log)
}

/// The first part of a game log: the launch details are at the top, and a
/// long session's log can be large.
fn read_log_head(path: &Path) -> Option<String> {
    let mut head = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(1 << 20)
        .read_to_end(&mut head)
        .ok()?;
    Some(String::from_utf8_lossy(&head).into_owned())
}

#[derive(Default)]
pub struct BuildOptions {
    pub pack: String,
    pub channel: String,
    pub base_url: String,
    /// Override the detected Minecraft version.
    pub mc_version: Option<String>,
    /// Override the detected loader version.
    pub loader_version: Option<String>,
    /// Override the detected loader (`neoforge`, `forge`, `fabric`, `quilt`).
    pub loader_type: Option<String>,
    /// Used for whatever is neither given nor detectable, typically the
    /// loader of the manifest published before.
    pub previous_loader: Option<Loader>,
    pub ignore: Vec<String>,
    /// Client version hints published with the manifest.
    pub client: Option<ClientInfo>,
    /// Server added to the players' multiplayer list.
    pub server: Option<ServerInfo>,
    /// Instance-relative files or folders installed on clients only if missing
    /// (e.g. `config/xaerominimap.txt`, `config/jei`).
    pub seeds: Vec<String>,
}

pub struct Built {
    pub manifest: Manifest,
    /// The folder that was scanned (see [`game_dir`]); copy blobs from here.
    pub game_dir: PathBuf,
    /// Where the loader and Minecraft version came from.
    pub loader_source: LoaderSource,
    pub local: Vec<LocalFile>,
    pub ignored: usize,
    /// Mod paths without a stable modId (they fall back to file-name identity).
    pub no_modid: Vec<String>,
    /// Findings worth a look before publishing (nothing blocking).
    pub warnings: Vec<String>,
}

/// Scan an instance and build its channel manifest (nothing is written).
///
/// Refuses packs that would break every client: two jars declaring the same
/// modId (the game would not start) or paths a client would reject.
pub fn build_manifest(instance: &Path, opts: &BuildOptions) -> Result<Built> {
    if !instance.is_dir() {
        bail!("Instanz-Ordner nicht gefunden: {}", instance.display());
    }
    if !is_safe_component(&opts.channel) {
        bail!("ungültiger Channel-Name: {:?}", opts.channel);
    }
    let (loader, loader_source) = resolve_loader(instance, opts)?;
    let instance = &game_dir(instance);

    let mut local = scan_instance(instance, &CATEGORIES)
        .with_context(|| format!("Instanz {} einlesen", instance.display()))?;
    let ignore: BTreeSet<&str> = opts.ignore.iter().map(String::as_str).collect();
    let before = local.len();
    local
        .retain(|lf| !ignore.contains(lf.file_name.as_str()) && !ignore.contains(lf.path.as_str()));
    let ignored = before - local.len();
    local.sort_by(|a, b| a.path.cmp(&b.path));

    check_duplicate_mod_ids(&local)?;
    let warnings = duplicate_content_warnings(&local);
    let no_modid = local
        .iter()
        .filter(|lf| lf.category == Category::Mod && lf.mod_ids.is_empty())
        .map(|lf| lf.path.clone())
        .collect();

    let files = local
        .iter()
        .map(|lf| build_entry(instance, lf, &opts.base_url))
        .collect::<Result<Vec<_>>>()?;
    let seeds = collect_seeds(instance, &opts.seeds, &opts.base_url)?;
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        pack_name: opts.pack.clone(),
        channel: opts.channel.clone(),
        generated_at: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default(),
        loader,
        files,
        client: opts.client.clone().filter(|c| *c != ClientInfo::default()),
        server: opts.server.clone(),
        seeds,
    };
    manifest
        .validate()
        .context("Das Manifest würde von den Clients abgelehnt")?;
    Ok(Built {
        manifest,
        game_dir: instance.clone(),
        loader_source,
        local,
        ignored,
        no_modid,
        warnings,
    })
}

fn given(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// Each of the three values: given > detected in the instance > previous manifest.
fn resolve_loader(instance: &Path, opts: &BuildOptions) -> Result<(Loader, LoaderSource)> {
    let (mc, version, kind) = (
        given(&opts.mc_version),
        given(&opts.loader_version),
        given(&opts.loader_type),
    );
    let fallback = match (mc, version, kind) {
        (Some(_), Some(_), Some(_)) => None,
        _ => detect_loader(instance).or_else(|| {
            opts.previous_loader.clone().map(|l| DetectedLoader {
                loader_type: l.loader_type,
                mc_version: l.mc_version,
                loader_version: l.loader_version,
                source: LoaderSource::Previous,
            })
        }),
    };
    let pick = |manual: Option<&str>, auto: Option<&String>| {
        manual.map(String::from).or_else(|| auto.cloned())
    };
    let missing = || {
        anyhow::anyhow!(
            "Minecraft- und Loader-Version nicht ermittelbar: Die Instanz hat keine \
             CurseForge- oder Prism-Daten und kein logs/latest.log. Das Spiel einmal mit \
             dieser Instanz starten oder die Versionen von Hand angeben."
        )
    };
    let loader = Loader {
        mc_version: pick(mc, fallback.as_ref().map(|d| &d.mc_version)).ok_or_else(missing)?,
        loader_version: pick(version, fallback.as_ref().map(|d| &d.loader_version))
            .ok_or_else(missing)?,
        loader_type: pick(kind, fallback.as_ref().map(|d| &d.loader_type))
            .ok_or_else(missing)?
            .to_lowercase(),
    };
    if !["neoforge", "forge", "fabric", "quilt"].contains(&loader.loader_type.as_str()) {
        bail!(
            "unbekannter Loader {:?} (erlaubt: neoforge, forge, fabric, quilt)",
            loader.loader_type
        );
    }
    let source = fallback.map_or(LoaderSource::Manual, |d| d.source);
    Ok((loader, source))
}

/// Two jars declaring the same modId crash the game on start — for everyone.
fn check_duplicate_mod_ids(local: &[LocalFile]) -> Result<()> {
    let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
    let mut clashes = Vec::new();
    for lf in local.iter().filter(|l| l.category == Category::Mod) {
        for id in &lf.mod_ids {
            if let Some(other) = seen.insert(id, &lf.path) {
                clashes.push(format!("modId '{id}': {other} und {}", lf.path));
            }
        }
    }
    if !clashes.is_empty() {
        bail!(
            "Mehrere Jars deklarieren dieselbe modId – das Spiel würde bei allen Spielern abstürzen:\n  {}\n\
             Eine der Dateien entfernen oder mit --ignore ausschließen.",
            clashes.join("\n  ")
        );
    }
    Ok(())
}

fn duplicate_content_warnings(local: &[LocalFile]) -> Vec<String> {
    let mut by_sha: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for lf in local {
        by_sha.entry(&lf.sha1).or_default().push(&lf.path);
    }
    by_sha
        .values()
        .filter(|paths| paths.len() > 1)
        .map(|paths| format!("identischer Inhalt mehrfach: {}", paths.join(", ")))
        .collect()
}

fn blob_url(base_url: &str, sha1: &str) -> String {
    let rel = format!("files/by-hash/{sha1}");
    if base_url.is_empty() {
        rel
    } else {
        format!("{}/{}", base_url.trim_end_matches('/'), rel)
    }
}

fn build_entry(instance: &Path, lf: &LocalFile, base_url: &str) -> Result<FileEntry> {
    let d = digest_file(&instance.join(&lf.path)).with_context(|| format!("{} hashen", lf.path))?;
    if d.sha1 != lf.sha1 {
        bail!(
            "{} hat sich während des Bauens geändert – bitte erneut starten",
            lf.path
        );
    }
    Ok(FileEntry {
        category: lf.category,
        path: lf.path.clone(),
        file_name: lf.file_name.clone(),
        size: d.len,
        sha1: d.sha1,
        sha256: Some(d.sha256),
        mod_id: lf.mod_ids.first().cloned(),
        mod_version: lf.mod_version.clone(),
        mod_name: lf.mod_name.clone(),
        url: blob_url(base_url, &lf.sha1),
    })
}

/// Expand seed specs (files or folders, instance-relative) into entries.
fn collect_seeds(instance: &Path, specs: &[String], base_url: &str) -> Result<Vec<SeedEntry>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeSet<String>) -> Result<()> {
        for e in std::fs::read_dir(dir).with_context(|| format!("{} lesen", dir.display()))? {
            let p = e?.path();
            if p.is_dir() {
                walk(base, &p, out)?;
            } else if p.is_file() {
                let rel = p
                    .strip_prefix(base)
                    .expect("below base")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel);
            }
        }
        Ok(())
    }
    let mut paths = BTreeSet::new();
    for spec in specs {
        let rel = spec.trim().replace('\\', "/").trim_matches('/').to_string();
        if rel.is_empty() {
            continue;
        }
        let full = instance.join(&rel);
        if full.is_dir() {
            walk(instance, &full, &mut paths)?;
        } else if full.is_file() {
            paths.insert(rel);
        } else {
            bail!("Seed nicht gefunden: {spec}");
        }
    }
    paths
        .into_iter()
        .map(|path| {
            if !is_safe_seed_path(&path) {
                bail!(
                    "Seed-Pfad nicht erlaubt: {path} (erlaubt: {}, sowie Ordner {})",
                    SEED_FILES.join(", "),
                    SEED_DIRS.map(|d| format!("{d}/")).join(", ")
                );
            }
            let d = digest_file(&instance.join(&path)).with_context(|| format!("{path} hashen"))?;
            Ok(SeedEntry {
                url: blob_url(base_url, &d.sha1),
                path,
                size: d.len,
                sha1: d.sha1,
                sha256: Some(d.sha256),
            })
        })
        .collect()
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_version: Option<String>,
}

/// Display name and version of a listed path (mods with metadata only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryLabel {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
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
    /// Names/versions of the paths in `added`, `removed` and `changed`.
    pub labels: BTreeMap<String, EntryLabel>,
}

impl ManifestDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.changed.is_empty()
            && self.updated.is_empty()
    }
}

/// Diff `new` against `old`, treating same-`modId` renames as version updates.
pub fn diff_manifests(old: Option<&Manifest>, new: &Manifest) -> ManifestDiff {
    let old = match old {
        Some(o) => o,
        None => {
            let added: Vec<String> = new.files.iter().map(|f| f.path.clone()).collect();
            return ManifestDiff {
                first_build: true,
                labels: labels_for(&added, new, None),
                added,
                ..Default::default()
            };
        }
    };

    let old_by_path: BTreeMap<&str, &FileEntry> =
        old.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let new_by_path: BTreeMap<&str, &FileEntry> =
        new.files.iter().map(|f| (f.path.as_str(), f)).collect();

    let mut added: Vec<&FileEntry> = new
        .files
        .iter()
        .filter(|f| !old_by_path.contains_key(f.path.as_str()))
        .collect();
    let mut removed: Vec<&FileEntry> = old
        .files
        .iter()
        .filter(|f| !new_by_path.contains_key(f.path.as_str()))
        .collect();
    let changed: Vec<String> = new
        .files
        .iter()
        .filter(|f| {
            old_by_path
                .get(f.path.as_str())
                .is_some_and(|o| o.sha1 != f.sha1)
        })
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
                    name: nf.mod_name.clone().or_else(|| of.mod_name.clone()),
                    old_version: of.mod_version.clone(),
                    new_version: nf.mod_version.clone(),
                });
            }
        }
    }
    let updated_ids: BTreeSet<&str> = updated.iter().map(|u| u.mod_id.as_str()).collect();
    added.retain(|f| {
        f.mod_id
            .as_deref()
            .is_none_or(|id| !updated_ids.contains(id))
    });
    removed.retain(|f| {
        f.mod_id
            .as_deref()
            .is_none_or(|id| !updated_ids.contains(id))
    });

    let added: Vec<String> = added.iter().map(|f| f.path.clone()).collect();
    let removed: Vec<String> = removed.iter().map(|f| f.path.clone()).collect();
    let listed: Vec<String> = added
        .iter()
        .chain(&removed)
        .chain(&changed)
        .cloned()
        .collect();
    ManifestDiff {
        first_build: false,
        labels: labels_for(&listed, new, Some(old)),
        added,
        removed,
        changed,
        updated,
    }
}

/// Labels for `paths`, taken from `new` (or `old` for removed files).
fn labels_for(
    paths: &[String],
    new: &Manifest,
    old: Option<&Manifest>,
) -> BTreeMap<String, EntryLabel> {
    let mut by_path: BTreeMap<&str, &FileEntry> = BTreeMap::new();
    for f in old.into_iter().flat_map(|o| &o.files).chain(&new.files) {
        by_path.insert(f.path.as_str(), f); // the new entry wins
    }
    paths
        .iter()
        .filter_map(|p| {
            let f = by_path.get(p.as_str())?;
            (f.mod_name.is_some() || f.mod_version.is_some()).then(|| {
                (
                    p.clone(),
                    EntryLabel {
                        name: f.mod_name.clone(),
                        version: f.mod_version.clone(),
                    },
                )
            })
        })
        .collect()
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

/// Copy every file and seed of `manifest` into `out/files/by-hash/` if absent.
/// Returns the number of new blobs.
pub fn populate_store(instance: &Path, out: &Path, manifest: &Manifest) -> Result<usize> {
    let store = out.join("files").join("by-hash");
    std::fs::create_dir_all(&store).with_context(|| format!("{} anlegen", store.display()))?;
    let items = manifest
        .files
        .iter()
        .map(|f| (&f.path, &f.sha1))
        .chain(manifest.seeds.iter().map(|s| (&s.path, &s.sha1)));
    let mut copied = 0;
    for (path, sha1) in items {
        let dst = store.join(sha1);
        if dst.exists() {
            continue;
        }
        let src = instance.join(path);
        let tmp = store.join(format!(".{sha1}.tmp"));
        std::fs::copy(&src, &tmp)
            .with_context(|| format!("{} in den Store kopieren", src.display()))?;
        std::fs::rename(&tmp, &dst)?;
        copied += 1;
    }
    Ok(copied)
}

/// Delete store blobs no longer referenced by `manifest`. Returns removed count.
pub fn gc_store(out: &Path, manifest: &Manifest) -> Result<usize> {
    let keep: BTreeSet<&str> = manifest
        .files
        .iter()
        .map(|f| f.sha1.as_str())
        .chain(manifest.seeds.iter().map(|s| s.sha1.as_str()))
        .collect();
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

/// Write `out/manifest.json` atomically; returns the exact bytes written
/// (the ones to sign).
pub fn write_manifest(out: &Path, manifest: &Manifest) -> Result<Vec<u8>> {
    std::fs::create_dir_all(out)?;
    let bytes = manifest.to_json_pretty()?.into_bytes();
    write_atomic(&out.join("manifest.json"), &bytes)?;
    Ok(bytes)
}

/// Sign the manifest bytes and write `out/manifest.json.sig`.
pub fn sign_manifest(out: &Path, manifest_bytes: &[u8], key: &SecretKey) -> Result<()> {
    write_atomic(
        &out.join(SIGNATURE_FILE),
        key.sign(manifest_bytes).as_bytes(),
    )
}

/// Read a secret signing key written by [`keygen`].
pub fn load_signing_key(path: &Path) -> Result<SecretKey> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Signatur-Schlüssel {} lesen", path.display()))?;
    SecretKey::from_text(&text)
        .with_context(|| format!("{} ist kein gültiger Schlüssel", path.display()))
}

/// Create a signing key at `secret_out` (never overwriting one; owner-only
/// permissions on Unix) and append its public key to `public_out` if given.
pub fn keygen(secret_out: &Path, public_out: Option<&Path>) -> Result<PublicKey> {
    let key = SecretKey::generate()?;
    if let Some(parent) = secret_out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(secret_out)
        .with_context(|| format!("{} anlegen (existiert er schon?)", secret_out.display()))?;
    std::io::Write::write_all(&mut f, key.to_text().as_bytes())?;
    let public = key.public();
    if let Some(p) = public_out {
        let existing = std::fs::read_to_string(p).unwrap_or_default();
        let mut text = existing.clone();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&public.to_hex());
        text.push('\n');
        write_atomic(p, text.as_bytes())?;
    }
    Ok(public)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("{} schreiben", path.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("{} ersetzen", path.display()))?;
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
            sha256: None,
            mod_id: mod_id.map(str::to_string),
            mod_version: None,
            mod_name: None,
            url: format!("files/by-hash/{sha1}"),
        }
    }

    fn man(files: Vec<FileEntry>) -> Manifest {
        Manifest {
            schema_version: 1,
            pack_name: "P".into(),
            channel: "main".into(),
            generated_at: "t".into(),
            loader: Loader {
                loader_type: "neoforge".into(),
                mc_version: "1.21.1".into(),
                loader_version: "21.1.234".into(),
            },
            files,
            client: None,
            server: None,
            seeds: vec![],
        }
    }

    fn opts() -> BuildOptions {
        BuildOptions {
            pack: "P".into(),
            channel: "main".into(),
            mc_version: Some("1.21.1".into()),
            loader_version: Some("21.1.234".into()),
            loader_type: Some("neoforge".into()),
            ..Default::default()
        }
    }

    /// A tiny jar declaring the given modIds.
    fn jar(ids: &[&str]) -> Vec<u8> {
        use std::io::Write;
        let mut toml = String::new();
        for id in ids {
            toml.push_str(&format!(
                "[[mods]]\nmodId=\"{id}\"\nversion=\"1\"\ndisplayName=\"Mod {id}\"\n"
            ));
        }
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        z.start_file(
            "META-INF/neoforge.mods.toml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        z.write_all(toml.as_bytes()).unwrap();
        z.finish().unwrap().into_inner()
    }

    fn instance(files: &[(&str, Vec<u8>)]) -> tempfile::TempDir {
        let d = tempfile::Builder::new()
            .prefix("bg-publish-")
            .tempdir()
            .unwrap();
        for (p, bytes) in files {
            let full = d.path().join(p);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, bytes).unwrap();
        }
        d
    }

    #[test]
    fn first_build_lists_all_as_added() {
        let d = diff_manifests(None, &man(vec![fe("mods/a.jar", "s1", Some("a"))]));
        assert!(d.first_build);
        assert_eq!(d.added, vec!["mods/a.jar".to_string()]);
    }

    #[test]
    fn version_bump_is_an_update_not_add_remove() {
        let named = |path: &str, sha1: &str, version: &str| FileEntry {
            mod_version: Some(version.into()),
            mod_name: Some("Create".into()),
            ..fe(path, sha1, Some("create"))
        };
        let old = man(vec![named("mods/create-6.0.10.jar", "s10", "6.0.10")]);
        let new = man(vec![named("mods/create-6.0.11.jar", "s11", "6.0.11")]);
        let d = diff_manifests(Some(&old), &new);
        assert!(d.added.is_empty() && d.removed.is_empty(), "{d:?}");
        assert_eq!(d.updated.len(), 1);
        assert_eq!(d.updated[0].mod_id, "create");
        assert_eq!(d.updated[0].new_file, "create-6.0.11.jar");
        assert_eq!(d.updated[0].name.as_deref(), Some("Create"));
        assert_eq!(d.updated[0].old_version.as_deref(), Some("6.0.10"));
        assert_eq!(d.updated[0].new_version.as_deref(), Some("6.0.11"));
    }

    #[test]
    fn add_remove_change_classified() {
        let gone = FileEntry {
            mod_name: Some("Gone Mod".into()),
            mod_version: Some("1.0".into()),
            ..fe("mods/gone.jar", "g1", Some("gone"))
        };
        let old = man(vec![fe("mods/keep.jar", "k1", Some("keep")), gone]);
        let new = man(vec![
            fe("mods/keep.jar", "k2", Some("keep")),
            fe("mods/new.jar", "n1", Some("new")),
        ]);
        let d = diff_manifests(Some(&old), &new);
        assert_eq!(d.added, vec!["mods/new.jar".to_string()]);
        assert_eq!(d.removed, vec!["mods/gone.jar".to_string()]);
        assert_eq!(d.changed, vec!["mods/keep.jar".to_string()]);
        assert_eq!(
            d.labels.get("mods/gone.jar"),
            Some(&EntryLabel {
                name: Some("Gone Mod".into()),
                version: Some("1.0".into())
            }),
            "removed files are labelled from the old manifest"
        );
        assert!(
            !d.labels.contains_key("mods/new.jar"),
            "no metadata, no label"
        );
    }

    #[test]
    fn builds_a_valid_manifest_with_sha256_and_seeds() {
        let inst = instance(&[
            ("mods/a.jar", jar(&["a"])),
            ("resourcepacks/p.zip", b"PACK".to_vec()),
            ("config/jei/client.ini", b"x=1".to_vec()),
            ("options.txt", b"fov:90".to_vec()),
        ]);
        let mut o = opts();
        o.seeds = vec!["config/jei".into(), "options.txt".into()];
        o.base_url = "https://cdn.example/main".into();
        let built = build_manifest(inst.path(), &o).unwrap();
        let m = &built.manifest;
        assert_eq!(m.files.len(), 2);
        assert!(m
            .files
            .iter()
            .all(|f| f.sha256.as_ref().is_some_and(|s| s.len() == 64)));
        assert_eq!(m.files[0].mod_id.as_deref(), Some("a"));
        assert_eq!(m.files[0].mod_name.as_deref(), Some("Mod a"));
        assert_eq!(m.files[0].mod_version.as_deref(), Some("1"));
        assert!(m.files[0]
            .url
            .starts_with("https://cdn.example/main/files/by-hash/"));
        let seeds: Vec<&str> = m.seeds.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(seeds, vec!["config/jei/client.ini", "options.txt"]);
        assert!(m.validate().is_ok());

        let out = tempfile::Builder::new()
            .prefix("bg-out-")
            .tempdir()
            .unwrap();
        assert_eq!(populate_store(inst.path(), out.path(), m).unwrap(), 4);
        assert_eq!(
            populate_store(inst.path(), out.path(), m).unwrap(),
            0,
            "idempotent"
        );
    }

    #[test]
    fn refuses_duplicate_mod_ids_and_warns_on_duplicate_content() {
        let clash = instance(&[
            ("mods/a.jar", jar(&["a", "shared"])),
            ("mods/b.jar", jar(&["shared"])),
        ]);
        let err = build_manifest(clash.path(), &opts()).err().unwrap();
        assert!(format!("{err:#}").contains("shared"), "{err:#}");

        let mut o = opts();
        o.ignore = vec!["b.jar".into()];
        assert!(
            build_manifest(clash.path(), &o).is_ok(),
            "--ignore resolves it"
        );

        let dup = instance(&[
            ("resourcepacks/a.zip", b"SAME".to_vec()),
            ("resourcepacks/b.zip", b"SAME".to_vec()),
        ]);
        let built = build_manifest(dup.path(), &opts()).unwrap();
        assert_eq!(built.warnings.len(), 1, "{:?}", built.warnings);
    }

    /// Options without any version: everything must come from the instance.
    fn auto() -> BuildOptions {
        BuildOptions {
            pack: "P".into(),
            channel: "main".into(),
            ..Default::default()
        }
    }

    fn loader_of(b: &Built) -> (&str, &str, &str, LoaderSource) {
        let l = &b.manifest.loader;
        (
            l.loader_type.as_str(),
            l.mc_version.as_str(),
            l.loader_version.as_str(),
            b.loader_source,
        )
    }

    #[test]
    fn detects_the_loader_from_any_launcher() {
        let cf = instance(&[
            ("mods/a.jar", jar(&["a"])),
            (
                "minecraftinstance.json",
                br#"{"name":"P","gameVersion":"1.21.1",
                    "baseModLoader":{"name":"neoforge-21.1.234","forgeVersion":"21.1.234"}}"#
                    .to_vec(),
            ),
        ]);
        let b = build_manifest(cf.path(), &auto()).unwrap();
        assert_eq!(
            loader_of(&b),
            ("neoforge", "1.21.1", "21.1.234", LoaderSource::CurseForge)
        );

        // Prism/MultiMC: the instance folder (or its game folder) is picked.
        let prism = instance(&[
            ("instance.cfg", b"name=P\n".to_vec()),
            (
                "mmc-pack.json",
                br#"{"components":[{"uid":"net.minecraft","version":"1.21.1"},
                    {"uid":"net.neoforged","version":"21.1.230"}]}"#
                    .to_vec(),
            ),
            ("minecraft/mods/a.jar", jar(&["a"])),
        ]);
        for picked in [prism.path().to_path_buf(), prism.path().join("minecraft")] {
            let b = build_manifest(&picked, &auto()).unwrap();
            assert_eq!(
                loader_of(&b),
                ("neoforge", "1.21.1", "21.1.230", LoaderSource::Prism)
            );
            assert_eq!(b.game_dir, prism.path().join("minecraft"));
            assert_eq!(b.manifest.files[0].path, "mods/a.jar");
        }

        // Any other launcher: the game's last start.
        let other = instance(&[
            ("mods/a.jar", jar(&["a"])),
            (
                "logs/latest.log",
                b"[main/INFO]: ModLauncher running: args [--fml.neoForgeVersion, 21.1.228, \
                  --fml.fmlVersion, 4.0.31, --fml.mcVersion, 1.21.1]\n"
                    .to_vec(),
            ),
        ]);
        let b = build_manifest(other.path(), &auto()).unwrap();
        assert_eq!(
            loader_of(&b),
            ("neoforge", "1.21.1", "21.1.228", LoaderSource::GameLog)
        );

        // Given values win, field by field.
        let mut o = auto();
        o.loader_version = Some("21.1.240".into());
        let b = build_manifest(other.path(), &o).unwrap();
        assert_eq!(
            loader_of(&b),
            ("neoforge", "1.21.1", "21.1.240", LoaderSource::GameLog)
        );
    }

    #[test]
    fn without_launcher_data_it_falls_back_or_asks() {
        let bare = instance(&[("mods/a.jar", jar(&["a"]))]);
        let err = build_manifest(bare.path(), &auto()).err().unwrap();
        assert!(format!("{err:#}").contains("nicht ermittelbar"), "{err:#}");

        let mut o = auto();
        o.previous_loader = Some(man(vec![]).loader);
        let b = build_manifest(bare.path(), &o).unwrap();
        assert_eq!(
            loader_of(&b),
            ("neoforge", "1.21.1", "21.1.234", LoaderSource::Previous)
        );

        let b = build_manifest(bare.path(), &opts()).unwrap();
        assert_eq!(b.loader_source, LoaderSource::Manual);

        let mut o = opts();
        o.loader_type = Some("rift".into());
        assert!(build_manifest(bare.path(), &o).is_err(), "unknown loader");
    }

    #[test]
    fn refuses_seeds_outside_the_allowed_folders() {
        let inst = instance(&[("saves/w/level.dat", b"x".to_vec())]);
        let mut o = opts();
        o.seeds = vec!["saves".into()];
        assert!(build_manifest(inst.path(), &o).is_err());
        o.seeds = vec!["config/missing.toml".into()];
        assert!(build_manifest(inst.path(), &o).is_err());
    }

    #[test]
    fn signed_manifest_verifies_with_the_generated_key() {
        let dir = tempfile::Builder::new()
            .prefix("bg-keys-")
            .tempdir()
            .unwrap();
        let secret = dir.path().join("k/manifest.key");
        let public_file = dir.path().join("trusted.pub");
        std::fs::write(&public_file, "# trusted keys\n").unwrap();
        let public = keygen(&secret, Some(&public_file)).unwrap();
        assert!(keygen(&secret, None).is_err(), "never overwrites a key");
        let keys = bonegrader_core::sign::parse_public_keys(
            &std::fs::read_to_string(&public_file).unwrap(),
        )
        .unwrap();
        assert_eq!(keys, vec![public]);

        let bytes = write_manifest(dir.path(), &man(vec![])).unwrap();
        sign_manifest(dir.path(), &bytes, &load_signing_key(&secret).unwrap()).unwrap();
        let sig = std::fs::read_to_string(dir.path().join(SIGNATURE_FILE)).unwrap();
        assert_eq!(
            bonegrader_core::sign::verify_any(&keys, &bytes, &sig),
            Ok(0)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "secret key is owner-only");
        }
    }
}
