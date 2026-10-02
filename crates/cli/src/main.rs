//! `bonegrader` — headless updater.
//!
//! Wires the whole pipeline together over HTTP: scan the instance, fetch the
//! manifest, compute the plan, and (with `update`) apply it. This is both a
//! usable CLI and the reference flow the Tauri GUI will wrap.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bonegrader_client::apply::apply;
use bonegrader_client::exec::{finalize, Decisions};
use bonegrader_client::http::{fetch_manifest, HttpFetcher};
use bonegrader_core::diff::{compute_plan, UpdatePlan};
use bonegrader_core::manifest::Category;
use bonegrader_core::scan::scan_instance;
use bonegrader_core::state::ClientState;
use clap::{Args, Parser, Subcommand};
use time::OffsetDateTime;

const STATE_FILE: &str = ".bonegrader-state.json";

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
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Plan(a) => {
            let (plan, _, _) = prepare(&a)?;
            print_plan(&plan);
            if plan.is_noop() {
                println!("\nAlready up to date.");
            } else {
                println!("\n(plan only — run `update` to apply)");
            }
            Ok(())
        }
        Cmd::Update(a) => run_update(a),
    }
}

/// Scan + fetch manifest + compute plan.
fn prepare(
    a: &CommonArgs,
) -> Result<(UpdatePlan, ClientState, bonegrader_core::manifest::Manifest)> {
    if !a.instance.is_dir() {
        anyhow::bail!("instance is not a directory: {}", a.instance.display());
    }
    let categories = [Category::Mod, Category::Resourcepack, Category::Shaderpack];
    let local = scan_instance(&a.instance, &categories)
        .with_context(|| format!("scanning {}", a.instance.display()))?;
    let manifest = fetch_manifest(&a.base_url)?;
    let state = load_state(&a.instance)?;
    let plan = compute_plan(&manifest, &local, &state);
    Ok((plan, state, manifest))
}

fn run_update(a: UpdateArgs) -> Result<()> {
    let (plan, state, manifest) = prepare(&a.common)?;
    print_plan(&plan);

    let mut decisions = Decisions::default();
    if a.remove_extras {
        decisions.remove_extras = plan.user_extras.iter().cloned().collect();
    }
    if a.keep_collisions {
        decisions.keep_collision_local = plan
            .collisions
            .iter()
            .map(|c| c.local_path.clone())
            .collect();
    }

    let exec = finalize(&plan, &decisions);
    if exec.downloads.is_empty() && exec.deletions.is_empty() {
        println!("\nNothing to apply.");
        return Ok(());
    }

    let fetcher = HttpFetcher::new();
    let report = apply(
        &a.common.instance,
        &manifest,
        &exec,
        &fetcher,
        &a.common.base_url,
        &state,
        &timestamp(),
    )?;
    save_state(&a.common.instance, &report.new_state)?;

    println!(
        "\nApplied: {} downloaded, {} removed.",
        report.downloaded, report.deleted
    );
    if let Some(b) = &report.backup_dir {
        println!("Backup of replaced/removed files: {}", b.display());
    }
    Ok(())
}

fn print_plan(plan: &UpdatePlan) {
    if plan.is_noop() && plan.user_extras.is_empty() {
        println!("No changes.");
        return;
    }
    for d in &plan.downloads {
        match &d.replaces {
            Some(old) => println!("  ~ update  {}  (replaces {old})", d.entry.path),
            None => println!("  + install {}", d.entry.path),
        }
    }
    for r in &plan.removals {
        println!("  - remove  {r}");
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
}

fn load_state(instance: &Path) -> Result<ClientState> {
    let path = instance.join(STATE_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => ClientState::from_json(&text).context("parsing state file"),
        Err(_) => Ok(ClientState {
            instance_path: instance.to_string_lossy().into(),
            launcher_type: "manual".into(),
            channel: "main".into(),
            ..Default::default()
        }),
    }
}

fn save_state(instance: &Path, state: &ClientState) -> Result<()> {
    let path = instance.join(STATE_FILE);
    let tmp = instance.join(format!("{STATE_FILE}.tmp"));
    std::fs::write(&tmp, state.to_json_pretty()?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Filesystem-safe UTC timestamp for the backup dir (no ':' — Windows-safe).
fn timestamp() -> String {
    let n = OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        n.year(),
        u8::from(n.month()),
        n.day(),
        n.hour(),
        n.minute(),
        n.second()
    )
}
