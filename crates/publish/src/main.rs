//! `bonegrader-publish` — CLI around the publisher library ([`bonegrader_publish`]).
//! Scans an instance, builds the channel manifest, shows the diff vs. what's live
//! and populates the content-addressed store for upload.

use std::path::{Path, PathBuf};

use anyhow::Result;
use bonegrader_core::manifest::Manifest;
use bonegrader_publish::{
    build_manifest, category_counts, diff_manifests, gc_store, populate_store, write_manifest,
    BuildOptions, ManifestDiff,
};
use clap::{Args, Parser, Subcommand};

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
    match Cli::parse().cmd {
        Cmd::Build(args) => build(args),
    }
}

fn build(args: BuildArgs) -> Result<()> {
    let opts = BuildOptions {
        pack: args.pack,
        channel: args.channel,
        base_url: args.base_url,
        mc_version: args.mc_version,
        loader_version: args.loader_version,
        loader_type: args.loader_type,
        ignore: args.ignore,
    };
    let built = build_manifest(&args.instance, &opts)?;

    for path in &built.no_modid {
        eprintln!("  note: no modId for {path} (file-name identity)");
    }

    print_diff(&diff_manifests(
        load_previous(&args.out).as_ref(),
        &built.manifest,
    ));

    let (mods, rp, sh) = category_counts(&built.manifest);
    let ign = if built.ignored > 0 {
        format!(", {} ignored", built.ignored)
    } else {
        String::new()
    };
    println!(
        "\nManifest: {} files ({mods} mods, {rp} resourcepacks, {sh} shaderpacks){ign}",
        built.manifest.files.len()
    );
    println!(
        "Loader:   {} {} (MC {})",
        built.manifest.loader.loader_type,
        built.manifest.loader.loader_version,
        built.manifest.loader.mc_version
    );

    if args.dry_run {
        println!("\n[dry-run] nothing written.");
        return Ok(());
    }

    let copied = populate_store(&args.instance, &args.out, &built.local)?;
    write_manifest(&args.out, &built.manifest)?;
    println!(
        "\nWrote {}/manifest.json ({copied} new blobs).",
        args.out.display()
    );

    if args.gc {
        println!(
            "GC: removed {} orphaned blobs.",
            gc_store(&args.out, &built.manifest)?
        );
    }
    Ok(())
}

fn load_previous(out: &Path) -> Option<Manifest> {
    Manifest::from_json(&std::fs::read_to_string(out.join("manifest.json")).ok()?).ok()
}

fn print_diff(d: &ManifestDiff) {
    if d.first_build {
        println!(
            "No previous manifest — first build ({} files).",
            d.added.len()
        );
        return;
    }
    if d.is_empty() {
        println!("No changes vs. previous manifest.");
        return;
    }
    println!("Changes vs. previous manifest:");
    for u in &d.updated {
        println!("  ~ update {}: {} -> {}", u.mod_id, u.old_file, u.new_file);
    }
    for p in &d.changed {
        println!("  ~ change {p}");
    }
    for p in &d.added {
        println!("  + add    {p}");
    }
    for p in &d.removed {
        println!("  - remove {p}");
    }
}
