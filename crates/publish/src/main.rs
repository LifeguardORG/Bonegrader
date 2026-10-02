//! `bonegrader-publish` — CLI around the publisher library ([`bonegrader_publish`]).
//! Scans an instance, builds the channel manifest, shows the diff vs. what's live,
//! signs it and populates the content-addressed store for upload.

use std::path::{Path, PathBuf};

use anyhow::Result;
use bonegrader_core::manifest::{ClientInfo, Manifest, ServerInfo};
use bonegrader_publish::{
    build_manifest, category_counts, diff_manifests, gc_store, keygen, load_signing_key,
    populate_store, sign_manifest, write_manifest, BuildOptions, ManifestDiff,
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
    Build(Box<BuildArgs>),
    /// Create a manifest signing key (once, on the admin's machine).
    Keygen(KeygenArgs),
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
    /// File or folder installed on clients only if missing, e.g. `config/jei`
    /// or `options.txt` (repeatable; allowed: options.txt, config/,
    /// defaultconfigs/, kubejs/).
    #[arg(long)]
    seed: Vec<String>,
    /// Sign the manifest with this key (see `keygen`). Also read from
    /// BONEGRADER_SIGNING_KEY.
    #[arg(long, env = "BONEGRADER_SIGNING_KEY")]
    sign_key: Option<PathBuf>,
    /// Oldest Bonegrader version allowed to apply this manifest.
    #[arg(long)]
    min_client_version: Option<String>,
    /// Newest released Bonegrader version (players get an update hint).
    #[arg(long)]
    latest_client_version: Option<String>,
    /// Where players download Bonegrader.
    #[arg(long)]
    client_download_url: Option<String>,
    /// Server name for the players' multiplayer list (with --server-address).
    #[arg(long, requires = "server_address")]
    server_name: Option<String>,
    /// Server address added to the players' multiplayer list.
    #[arg(long)]
    server_address: Option<String>,
    /// Compute and print the diff only; write nothing.
    #[arg(long)]
    dry_run: bool,
    /// After building, delete store blobs no longer referenced by the manifest.
    #[arg(long)]
    gc: bool,
}

#[derive(Args)]
struct KeygenArgs {
    /// Where to store the secret key (never commit it!).
    #[arg(long)]
    secret_out: PathBuf,
    /// Trusted-keys file to append the public key to, e.g.
    /// keys/manifest-signing.pub.
    #[arg(long)]
    public_out: Option<PathBuf>,
}

/// Errors are printed as one line with their full cause chain (no
/// backtrace dump), and the process exits with status 1.
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Build(args) => build(*args),
        Cmd::Keygen(args) => {
            let public = keygen(&args.secret_out, args.public_out.as_deref())?;
            println!(
                "Secret key:  {}  (keep it secret, never commit it)",
                args.secret_out.display()
            );
            println!("Public key:  {}", public.to_hex());
            if let Some(p) = &args.public_out {
                println!(
                    "Appended to: {} — commit it and release a new client.",
                    p.display()
                );
            }
            Ok(())
        }
    }
}

fn build(args: BuildArgs) -> Result<()> {
    let client = ClientInfo {
        min_version: args.min_client_version,
        latest_version: args.latest_client_version,
        download_url: args.client_download_url,
    };
    let server = args.server_address.map(|address| ServerInfo {
        name: args.server_name.unwrap_or_else(|| args.pack.clone()),
        address,
    });
    let signing_key = args.sign_key.as_deref().map(load_signing_key).transpose()?;
    let opts = BuildOptions {
        pack: args.pack,
        channel: args.channel,
        base_url: args.base_url,
        mc_version: args.mc_version,
        loader_version: args.loader_version,
        loader_type: args.loader_type,
        ignore: args.ignore,
        client: Some(client),
        server,
        seeds: args.seed,
    };
    let built = build_manifest(&args.instance, &opts)?;

    for path in &built.no_modid {
        eprintln!("  note: no modId for {path} (file-name identity)");
    }
    for w in &built.warnings {
        eprintln!("  warning: {w}");
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
        "\nManifest: {} files ({mods} mods, {rp} resourcepacks, {sh} shaderpacks){ign}, {} seeds",
        built.manifest.files.len(),
        built.manifest.seeds.len()
    );
    println!(
        "Loader:   {} {} (MC {})",
        built.manifest.loader.loader_type,
        built.manifest.loader.loader_version,
        built.manifest.loader.mc_version
    );
    if signing_key.is_none() {
        println!("Signature: none (pass --sign-key to sign)");
    }

    if args.dry_run {
        println!("\n[dry-run] nothing written.");
        return Ok(());
    }

    let copied = populate_store(&args.instance, &args.out, &built.manifest)?;
    let bytes = write_manifest(&args.out, &built.manifest)?;
    if let Some(key) = &signing_key {
        sign_manifest(&args.out, &bytes, key)?;
    }
    println!(
        "\nWrote {}/manifest.json{} ({copied} new blobs).",
        args.out.display(),
        if signing_key.is_some() {
            " + signature"
        } else {
            ""
        }
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
        let name = u.name.as_deref().unwrap_or(&u.mod_id);
        match (&u.old_version, &u.new_version) {
            (Some(o), Some(n)) if o != n => println!(
                "  ~ update {name} {o} -> {n} ({} -> {})",
                u.old_file, u.new_file
            ),
            _ => println!("  ~ update {name}: {} -> {}", u.old_file, u.new_file),
        }
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
