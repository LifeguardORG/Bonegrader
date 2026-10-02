//! Reusable publisher logic shared by the `bonegrader-publish` CLI and the admin
//! GUI: scan an instance, build and check a channel manifest, diff it against
//! what's live, sign it, populate the content-addressed store and upload a
//! channel to the server.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use bonegrader_core::hash::digest_file;
use bonegrader_core::launcher::{loader_type_from_name, parse_curseforge_instance};
use bonegrader_core::manifest::{
    Category, ClientInfo, FileEntry, Loader, Manifest, SeedEntry, ServerInfo, SCHEMA_VERSION,
};
use bonegrader_core::paths::{is_safe_component, is_safe_seed_path, SEED_DIRS, SEED_FILES};
use bonegrader_core::scan::{scan_instance, LocalFile};
use bonegrader_core::sign::{PublicKey, SecretKey, SIGNATURE_FILE};
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
    let cf = parse_curseforge_instance(&text)?;
    Some(DetectedLoader {
        loader_type: loader_type_from_name(cf.loader_name.as_deref().unwrap_or_default())
            .to_string(),
        mc_version: cf.mc_version?,
        loader_version: cf.loader_version?,
    })
}

#[derive(Default)]
pub struct BuildOptions {
    pub pack: String,
    pub channel: String,
    pub base_url: String,
    pub mc_version: Option<String>,
    pub loader_version: Option<String>,
    pub loader_type: String,
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
    let detected = detect_loader(instance);
    let mc_version = opts
        .mc_version
        .clone()
        .or_else(|| detected.as_ref().map(|d| d.mc_version.clone()))
        .context("Minecraft-Version nicht ermittelbar (bitte angeben)")?;
    let loader_version = opts
        .loader_version
        .clone()
        .or_else(|| detected.as_ref().map(|d| d.loader_version.clone()))
        .context("Loader-Version nicht ermittelbar (bitte angeben)")?;
    let loader_type = detected
        .as_ref()
        .map(|d| d.loader_type.clone())
        .unwrap_or_else(|| opts.loader_type.clone());

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
        loader: Loader {
            loader_type,
            mc_version,
            loader_version,
        },
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
        local,
        ignored,
        no_modid,
        warnings,
    })
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

/// Characters allowed in the SSH target and remote folder: enough for
/// `deploy@host`, `/srv/bonegrader`, `~/sites/x` — and nothing a shell or
/// rsync would interpret.
fn is_plain_arg(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._/~:-".contains(c))
}

/// rsync a built channel dir to `<ssh_host>:<remote_base>/<channel>`: blobs
/// first (additive, immutable), then signature and manifest, switched with one
/// remote command (signature first — a client caught in between sees a
/// mismatched pair once and simply retries). Requires `rsync`/`ssh`.
pub fn upload_channel(out: &Path, ssh_host: &str, remote_base: &str, channel: &str) -> Result<()> {
    if !is_plain_arg(ssh_host) || !is_plain_arg(remote_base) || !is_safe_component(channel) {
        bail!("SSH-Ziel, Remote-Ordner oder Channel enthalten unzulässige Zeichen");
    }
    let dest = format!("{}/{channel}", remote_base.trim_end_matches('/'));
    let signed = out.join(SIGNATURE_FILE).exists();

    run(Command::new("ssh")
        .arg(ssh_host)
        .arg(format!("mkdir -p {dest}/files/by-hash")))?;
    run(Command::new("rsync")
        .args(["-a", "--ignore-existing"])
        .arg(format!("{}/", out.join("files").display()))
        .arg(format!("{ssh_host}:{dest}/files/")))?;
    run(Command::new("rsync")
        .arg("-a")
        .arg(out.join("manifest.json"))
        .arg(format!("{ssh_host}:{dest}/manifest.json.tmp")))?;
    let mut switch = String::new();
    if signed {
        run(Command::new("rsync")
            .arg("-a")
            .arg(out.join(SIGNATURE_FILE))
            .arg(format!("{ssh_host}:{dest}/{SIGNATURE_FILE}.tmp")))?;
        switch.push_str(&format!(
            "mv -f {dest}/{SIGNATURE_FILE}.tmp {dest}/{SIGNATURE_FILE} && "
        ));
    }
    switch.push_str(&format!(
        "mv -f {dest}/manifest.json.tmp {dest}/manifest.json"
    ));
    run(Command::new("ssh").arg(ssh_host).arg(switch))?;
    Ok(())
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("{cmd:?} starten"))?;
    if !status.success() {
        bail!("Befehl fehlgeschlagen ({:?}): {cmd:?}", status.code());
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
            sha256: None,
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
            loader_type: "neoforge".into(),
            ..Default::default()
        }
    }

    /// A tiny jar declaring the given modIds.
    fn jar(ids: &[&str]) -> Vec<u8> {
        use std::io::Write;
        let mut toml = String::new();
        for id in ids {
            toml.push_str(&format!("[[mods]]\nmodId=\"{id}\"\nversion=\"1\"\n"));
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
        let old = man(vec![
            fe("mods/keep.jar", "k1", Some("keep")),
            fe("mods/gone.jar", "g1", Some("gone")),
        ]);
        let new = man(vec![
            fe("mods/keep.jar", "k2", Some("keep")),
            fe("mods/new.jar", "n1", Some("new")),
        ]);
        let d = diff_manifests(Some(&old), &new);
        assert_eq!(d.added, vec!["mods/new.jar".to_string()]);
        assert_eq!(d.removed, vec!["mods/gone.jar".to_string()]);
        assert_eq!(d.changed, vec!["mods/keep.jar".to_string()]);
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

    #[test]
    fn upload_arguments_are_checked() {
        assert!(is_plain_arg("deploy@bonegrader.example.com"));
        assert!(is_plain_arg("/srv/bonegrader"));
        assert!(!is_plain_arg("host; rm -rf /"));
        assert!(!is_plain_arg("/srv/x'y"));
        assert!(!is_plain_arg("-oProxyCommand=evil"));
        let out = tempfile::Builder::new().tempdir().unwrap();
        assert!(upload_channel(out.path(), "h", "/srv/$(evil)", "main").is_err());
        assert!(upload_channel(out.path(), "h", "/srv", "../main").is_err());
    }
}
