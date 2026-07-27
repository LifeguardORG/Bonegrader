//! `bonegrader-publish` — the developer side of Bonegrader.
//!
//! It scans a Minecraft instance, builds a channel manifest and populates a
//! content-addressed store (`<out>/files/by-hash/<sha1>`) that can be rsync'd to
//! the server. It always shows the change vs. the previous manifest first, so a
//! stray dev/junk mod can be caught before anything is shipped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use bonegrader_core::manifest::{Category, FileEntry, Loader, Manifest};
use bonegrader_core::scan::{scan_instance, LocalFile};
use clap::{Args, Parser, Subcommand};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Parser)]
#[command(
    name = "bonegrader-publish",
    version,
    about = "Build/publish a Bonegrader channel from a Minecraft instance"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scan an instance, build the manifest and populate the content store.
    Build(BuildArgs),
}

#[derive(Args)]
struct BuildArgs {
    /// Path to the source Minecraft instance (dev copy).
    #[arg(long)]
    instance: PathBuf,
    /// Output channel directory (holds manifest.json + files/by-hash/).
    #[arg(long)]
    out: PathBuf,
    #[arg(long, default_value = "BonesAndBees")]
    pack: String,
    #[arg(long, default_value = "main")]
    channel: String,
    /// Minecraft version (auto-detected from minecraftinstance.json if omitted).
    #[arg(long)]
    mc_version: Option<String>,
    /// Loader version (auto-detected from minecraftinstance.json if omitted).
    #[arg(long)]
    loader_version: Option<String>,
    #[arg(long, default_value = "neoforge")]
    loader_type: String,
    /// Optional absolute base URL to prefix `files/by-hash/...` in the manifest.
    #[arg(long, default_value = "")]
    base_url: String,
    /// File name or client-relative path to exclude (repeatable).
    #[arg(long)]
    ignore: Vec<String>,
    /// Compute and print the diff only; write nothing.
    #[arg(long)]
    dry_run: bool,
    /// After building, delete store blobs no longer referenced by the manifest.
    #[arg(long)]
    gc: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Build(args) => build(args),
    }
}

fn build(args: BuildArgs) -> Result<()> {
    if !args.instance.is_dir() {
        bail!("instance path is not a directory: {}", args.instance.display());
    }

    // Resolve loader/mc version: CLI overrides, else read minecraftinstance.json.
    let detected = detect_loader(&args.instance);
    let mc_version = args
        .mc_version
        .clone()
        .or_else(|| detected.as_ref().map(|d| d.mc_version.clone()))
        .context("could not determine mc version; pass --mc-version")?;
    let loader_version = args
        .loader_version
        .clone()
        .or_else(|| detected.as_ref().map(|d| d.loader_version.clone()))
        .context("could not determine loader version; pass --loader-version")?;
    let loader_type = detected
        .as_ref()
        .map(|d| d.loader_type.clone())
        .unwrap_or(args.loader_type.clone());

    // Scan the three managed folders.
    let categories = [Category::Mod, Category::Resourcepack, Category::Shaderpack];
    let mut local = scan_instance(&args.instance, &categories)
        .with_context(|| format!("scanning instance {}", args.instance.display()))?;

    // Apply ignore list.
    let ignore: BTreeSet<&str> = args.ignore.iter().map(String::as_str).collect();
    let before = local.len();
    local.retain(|lf| !ignore.contains(lf.file_name.as_str()) && !ignore.contains(lf.path.as_str()));
    let ignored = before - local.len();
    local.sort_by(|a, b| a.path.cmp(&b.path));

    // Warn about mods without a stable modId (they fall back to file-name identity).
    for lf in &local {
        if lf.category == Category::Mod && lf.mod_ids.is_empty() {
            eprintln!("  note: no modId for {} (file-name identity)", lf.path);
        }
    }

    let files: Vec<FileEntry> = local.iter().map(|lf| build_entry(lf, &args.base_url)).collect();

    let manifest = Manifest {
        schema_version: 1,
        pack_name: args.pack.clone(),
        channel: args.channel.clone(),
        generated_at: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default(),
        loader: Loader {
            loader_type,
            mc_version,
            loader_version,
        },
        files,
    };

    // Diff against the previous manifest (if any) and print it.
    let manifest_path = args.out.join("manifest.json");
    let previous = load_previous(&manifest_path);
    print_diff(previous.as_ref(), &manifest);

    let counts = category_counts(&manifest);
    println!(
        "\nManifest: {} files ({} mods, {} resourcepacks, {} shaderpacks){}",
        manifest.files.len(),
        counts.0,
        counts.1,
        counts.2,
        if ignored > 0 {
            format!(", {ignored} ignored")
        } else {
            String::new()
        }
    );
    println!(
        "Loader:   {} {} (MC {})",
        manifest.loader.loader_type, manifest.loader.loader_version, manifest.loader.mc_version
    );

    if args.dry_run {
        println!("\n[dry-run] nothing written.");
        return Ok(());
    }

    // Populate the content-addressed store, then write the manifest last.
    let copied = populate_store(&args.instance, &args.out, &local)?;
    write_atomic(&manifest_path, manifest.to_json_pretty()?.as_bytes())?;
    println!("\nWrote {} ({copied} new blobs).", manifest_path.display());

    if args.gc {
        let removed = gc_store(&args.out, &manifest)?;
        println!("GC: removed {removed} orphaned blobs.");
    }

    Ok(())
}

struct DetectedLoader {
    loader_type: String,
    mc_version: String,
    loader_version: String,
}

/// Best-effort loader detection from a CurseForge `minecraftinstance.json`.
fn detect_loader(instance_dir: &Path) -> Option<DetectedLoader> {
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
    let loader_type = if name.contains("fabric") {
        "fabric"
    } else {
        "neoforge"
    }
    .to_string();
    Some(DetectedLoader {
        loader_type,
        mc_version,
        loader_version,
    })
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

fn load_previous(manifest_path: &Path) -> Option<Manifest> {
    let text = std::fs::read_to_string(manifest_path).ok()?;
    Manifest::from_json(&text).ok()
}

fn category_counts(m: &Manifest) -> (usize, usize, usize) {
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

/// Print a human diff between the old and new manifest, treating same-`modId`
/// file renames as version updates rather than add+remove pairs.
fn print_diff(old: Option<&Manifest>, new: &Manifest) {
    let old = match old {
        Some(o) => o,
        None => {
            println!("No previous manifest — first build ({} files).", new.files.len());
            return;
        }
    };

    let old_by_path: BTreeMap<&str, &FileEntry> =
        old.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let new_by_path: BTreeMap<&str, &FileEntry> =
        new.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let old_mod_by_id = mods_by_id(old);
    let new_mod_by_id = mods_by_id(new);

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
    let changed: Vec<(&FileEntry, &FileEntry)> = new
        .files
        .iter()
        .filter_map(|f| {
            old_by_path
                .get(f.path.as_str())
                .filter(|o| o.sha1 != f.sha1)
                .map(|o| (*o, f))
        })
        .collect();

    // Reconcile version bumps: same modId, different path.
    let mut updated: Vec<(String, String, String)> = Vec::new(); // modId, old file, new file
    for (mid, nf) in &new_mod_by_id {
        if let Some(of) = old_mod_by_id.get(mid) {
            if of.path != nf.path {
                updated.push((mid.clone(), of.file_name.clone(), nf.file_name.clone()));
            }
        }
    }
    let updated_ids: BTreeSet<&str> = updated.iter().map(|(id, _, _)| id.as_str()).collect();
    added.retain(|f| f.mod_id.as_deref().is_none_or(|id| !updated_ids.contains(id)));
    removed.retain(|f| f.mod_id.as_deref().is_none_or(|id| !updated_ids.contains(id)));

    if added.is_empty() && removed.is_empty() && changed.is_empty() && updated.is_empty() {
        println!("No changes vs. previous manifest.");
        return;
    }

    println!("Changes vs. previous manifest:");
    for (id, o, n) in &updated {
        println!("  ~ update {id}: {o} -> {n}");
    }
    for (o, n) in &changed {
        println!("  ~ change {} (sha {}… -> {}…)", n.path, short(&o.sha1), short(&n.sha1));
    }
    for f in &added {
        println!("  + add    {}", f.path);
    }
    for f in &removed {
        println!("  - remove {}", f.path);
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

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

/// Copy each scanned file into the content store if absent. Returns new-blob count.
fn populate_store(instance: &Path, out: &Path, local: &[LocalFile]) -> Result<usize> {
    let store = out.join("files").join("by-hash");
    std::fs::create_dir_all(&store)
        .with_context(|| format!("creating store {}", store.display()))?;
    let mut copied = 0;
    for lf in local {
        let dst = store.join(&lf.sha1);
        if dst.exists() {
            continue;
        }
        let src = instance.join(&lf.path);
        // Copy to a temp name then rename, so a blob is never half-written.
        let tmp = store.join(format!(".{}.tmp", lf.sha1));
        std::fs::copy(&src, &tmp)
            .with_context(|| format!("copying {} -> store", src.display()))?;
        std::fs::rename(&tmp, &dst)?;
        copied += 1;
    }
    Ok(copied)
}

fn gc_store(out: &Path, manifest: &Manifest) -> Result<usize> {
    let keep: BTreeSet<&str> = manifest.files.iter().map(|f| f.sha1.as_str()).collect();
    let store = out.join("files").join("by-hash");
    let mut removed = 0;
    if !store.is_dir() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(&store)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue; // skip temp files
        }
        if !keep.contains(name.as_ref()) {
            std::fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
