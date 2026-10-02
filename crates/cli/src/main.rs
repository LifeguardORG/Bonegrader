//! `bonegrader` — headless updater.
//!
//! Runs the same pipeline as the desktop app ([`bonegrader_client::session`]):
//! fetch and verify the manifest, scan the instance, show the plan, apply it —
//! or undo the last update.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Result};
use bonegrader_client::apply::{ApplyOptions, Progress};
use bonegrader_client::exec::Decisions;
use bonegrader_client::http::HttpFetcher;
use bonegrader_client::session::{
    restorable, timestamp, trusted_keys, undo_last, ClientCompat, Prepared, SignatureStatus,
    Updater,
};
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bonegrader", version, about = "Headless Bonegrader updater")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show what an update would do (no changes).
    Plan(CommonArgs),
    /// Apply the update.
    Update(UpdateArgs),
    /// Undo the last update: restore the replaced/removed files and the state.
    Undo {
        /// Instance/game directory (the folder that contains `mods/`).
        #[arg(long)]
        instance: PathBuf,
    },
}

#[derive(Args)]
struct CommonArgs {
    /// Instance/game directory (the folder that contains `mods/`).
    #[arg(long)]
    instance: PathBuf,
    /// Channel base URL, e.g. https://bonegrader.example.com/main
    #[arg(long)]
    base_url: String,
}

#[derive(Args)]
struct UpdateArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Also delete unmanaged "extra" mods (default: keep them).
    #[arg(long)]
    remove_extras: bool,
    /// Keep the player's clashing copy on a modId collision instead of replacing
    /// it (default: replace with the managed version).
    #[arg(long)]
    keep_collisions: bool,
    /// Update even if the instance does not look like it belongs to this pack.
    #[arg(long)]
    force: bool,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Plan(a) => {
            let fetcher = HttpFetcher::new();
            let keys = trusted_keys()?;
            let prepared = updater(&a, &fetcher, &keys).prepare()?;
            print_plan(&prepared);
            if prepared.is_noop() {
                println!("\nAlready up to date.");
            } else {
                println!("\n(plan only — run `update` to apply)");
            }
            Ok(())
        }
        Cmd::Update(a) => run_update(a),
        Cmd::Undo { instance } => {
            let Some(r) = restorable(&instance) else {
                bail!("Nothing to undo in {}.", instance.display());
            };
            println!(
                "Undoing the update from {} ({} files)…",
                r.timestamp, r.files
            );
            let report = undo_last(&instance, &timestamp())?;
            println!(
                "Done: {} files restored, {} files of that update set aside in {}.",
                report.restored,
                report.removed,
                report.backup_dir.display()
            );
            Ok(())
        }
    }
}

fn updater<'a>(
    a: &'a CommonArgs,
    fetcher: &'a HttpFetcher,
    keys: &'a [bonegrader_core::sign::PublicKey],
) -> Updater<'a> {
    Updater {
        instance: &a.instance,
        base_url: &a.base_url,
        fetcher,
        keys,
    }
}

fn run_update(a: UpdateArgs) -> Result<()> {
    let fetcher = HttpFetcher::new();
    let keys = trusted_keys()?;
    let up = updater(&a.common, &fetcher, &keys);
    let prepared = up.prepare()?;
    print_plan(&prepared);

    if prepared.assessment.suspicious && !a.force {
        bail!("This instance does not look like it belongs to this pack (see above). Re-run with --force if it does.");
    }
    let mut decisions = Decisions::default();
    if a.remove_extras {
        decisions.remove_extras = prepared.plan.user_extras.iter().cloned().collect();
    }
    if a.keep_collisions {
        decisions.keep_collision_local = prepared
            .plan
            .collisions
            .iter()
            .map(|c| c.local_path.clone())
            .collect();
    }
    if prepared.is_noop() && decisions.remove_extras.is_empty() {
        println!("\nNothing to apply.");
        return Ok(());
    }

    let outcome = up.apply(
        &prepared,
        &decisions,
        &ApplyOptions::default(),
        &timestamp(),
        &print_progress,
    )?;
    eprintln!();
    let r = &outcome.report;
    println!(
        "\nApplied: {} installed ({} downloaded, {} reused), {} replaced, {} removed{}.",
        r.installed,
        r.downloaded,
        r.reused,
        r.replaced,
        r.deleted,
        if r.seeded > 0 {
            format!(", {} config files created", r.seeded)
        } else {
            String::new()
        }
    );
    if outcome.server_added {
        println!("Added the server to the multiplayer list.");
    }
    if let Some(b) = &r.backup_dir {
        println!("Backup: {}  (undo with `bonegrader undo`)", b.display());
    }
    for w in &r.warnings {
        println!("warning: {w}");
    }
    Ok(())
}

fn print_progress(p: &Progress) {
    if p.phase == "download" && p.total_bytes > 0 {
        let pct = p.done_bytes * 100 / p.total_bytes;
        eprint!(
            "\r  downloading {}/{} files, {pct}%   ",
            p.done_files, p.total_files
        );
        let _ = std::io::stderr().flush();
    }
}

fn print_plan(p: &Prepared) {
    let m = &p.fetched.manifest;
    println!(
        "{} ({}) — {} {} for Minecraft {}{}",
        m.pack_name,
        m.channel,
        m.loader.loader_type,
        m.loader.loader_version,
        m.loader.mc_version,
        match p.fetched.signature {
            SignatureStatus::Verified => ", signature verified",
            SignatureStatus::NotRequired => "",
        }
    );
    match &p.compat {
        ClientCompat::Current => {}
        ClientCompat::UpdateAvailable {
            latest,
            download_url,
        } => println!(
            "Note: Bonegrader {latest} is available{}.",
            download_url
                .as_deref()
                .map(|u| format!(" ({u})"))
                .unwrap_or_default()
        ),
        ClientCompat::UpdateRequired { min, download_url } => println!(
            "This Bonegrader is too old for this pack — version {min} or newer is required{}.",
            download_url
                .as_deref()
                .map(|u| format!(" ({u})"))
                .unwrap_or_default()
        ),
    }
    let a = &p.assessment;
    if let Some(prev) = &a.previous_pack {
        println!("WARNING: this instance was last updated to a different pack ({prev}).");
    } else if a.suspicious {
        println!(
            "WARNING: only {} of the {} mods in this instance belong to the pack — is it the right instance?",
            a.pack_mods, a.local_mods
        );
    }

    let plan = &p.plan;
    if p.is_noop() && plan.user_extras.is_empty() {
        println!("No changes.");
        return;
    }
    for d in &plan.downloads {
        match &d.replaces {
            Some(old) if old == &d.entry.path => println!("  ~ update  {}", d.entry.path),
            Some(old) => println!("  ~ update  {}  (replaces {old})", d.entry.path),
            None => println!("  + install {}", d.entry.path),
        }
    }
    for r in &plan.removals {
        println!("  - remove  {r}");
    }
    for d in &plan.duplicates {
        println!("  - remove  {d}  (identical copy of a pack file)");
    }
    for c in &plan.collisions {
        println!(
            "  ! clash   {} shares modId '{}' with {}",
            c.local_path, c.mod_id, c.manifest_path
        );
    }
    for e in &plan.user_extras {
        println!("  = keep    {e}  (your own mod)");
    }
    for s in &p.seeds {
        println!(
            "  + create  {}  (default config, only because it is missing)",
            s.path
        );
    }
    if let Some(s) = &p.server {
        println!(
            "  + server  {} ({}) to the multiplayer list",
            s.name, s.address
        );
    }
}
