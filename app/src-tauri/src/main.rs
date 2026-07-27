// Prevent an extra console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Bonegrader desktop app (Tauri v2).
//!
//! The GUI is intentionally thin: three commands wrap the exact same core
//! pipeline the headless CLI uses. Blocking work (HTTP, hashing, filesystem)
//! runs off the UI thread via `spawn_blocking`, and download/apply progress is
//! streamed to the frontend through the `update-progress` event.

use std::path::{Path, PathBuf};

use bonegrader_client::apply::{apply_with_progress, Progress};
use bonegrader_client::detect::{discover_all, DetectedInstance};
use bonegrader_client::exec::{finalize, Decisions};
use bonegrader_client::http::{fetch_manifest, HttpFetcher};
use bonegrader_core::diff::{compute_plan, UpdatePlan};
use bonegrader_core::manifest::Category;
use bonegrader_core::scan::scan_instance;
use bonegrader_core::state::ClientState;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use time::OffsetDateTime;

const STATE_FILE: &str = ".bonegrader-state.json";
const PROGRESS_EVENT: &str = "update-progress";
const CATEGORIES: [Category; 3] = [
    Category::Mod,
    Category::Resourcepack,
    Category::Shaderpack,
];

/// List every instance we can auto-detect.
#[tauri::command]
fn detect_instances() -> Vec<DetectedInstance> {
    discover_all()
}

/// Compute (but don't apply) the update for an instance against a channel.
#[tauri::command]
async fn plan_update(instance: String, base_url: String) -> Result<UpdatePlan, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<UpdatePlan, String> {
        let inst = PathBuf::from(&instance);
        let local = scan_instance(&inst, &CATEGORIES).map_err(|e| e.to_string())?;
        let manifest = fetch_manifest(&base_url).map_err(|e| e.to_string())?;
        let state = load_state(&inst).map_err(|e| e.to_string())?;
        Ok(compute_plan(&manifest, &local, &state))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApplyResult {
    downloaded: usize,
    deleted: usize,
    backup_dir: Option<String>,
}

/// Apply the update with the user's extra/collision decisions.
#[tauri::command]
async fn apply_update(
    app: AppHandle,
    instance: String,
    base_url: String,
    remove_extras: Vec<String>,
    keep_collisions: Vec<String>,
) -> Result<ApplyResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        run_apply(
            &app,
            &PathBuf::from(&instance),
            &base_url,
            remove_extras,
            keep_collisions,
        )
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

fn run_apply(
    app: &AppHandle,
    inst: &Path,
    base_url: &str,
    remove_extras: Vec<String>,
    keep_collisions: Vec<String>,
) -> anyhow::Result<ApplyResult> {
    let local = scan_instance(inst, &CATEGORIES)?;
    let manifest = fetch_manifest(base_url)?;
    let state = load_state(inst)?;
    let plan = compute_plan(&manifest, &local, &state);

    let decisions = Decisions {
        remove_extras: remove_extras.into_iter().collect(),
        keep_collision_local: keep_collisions.into_iter().collect(),
    };
    let exec = finalize(&plan, &decisions);
    let fetcher = HttpFetcher::new();

    let progress = |p: &Progress| {
        let _ = app.emit(PROGRESS_EVENT, p);
    };
    let report = apply_with_progress(
        inst,
        &manifest,
        &exec,
        &fetcher,
        base_url,
        &state,
        &timestamp(),
        &progress,
    )?;
    save_state(inst, &report.new_state)?;

    Ok(ApplyResult {
        downloaded: report.downloaded,
        deleted: report.deleted,
        backup_dir: report.backup_dir.map(|p| p.display().to_string()),
    })
}

fn load_state(instance: &Path) -> anyhow::Result<ClientState> {
    match std::fs::read_to_string(instance.join(STATE_FILE)) {
        Ok(text) => Ok(ClientState::from_json(&text)?),
        Err(_) => Ok(ClientState {
            instance_path: instance.to_string_lossy().into(),
            launcher_type: "manual".into(),
            channel: "main".into(),
            ..Default::default()
        }),
    }
}

fn save_state(instance: &Path, state: &ClientState) -> anyhow::Result<()> {
    let tmp = instance.join(format!("{STATE_FILE}.tmp"));
    std::fs::write(&tmp, state.to_json_pretty()?)?;
    std::fs::rename(&tmp, instance.join(STATE_FILE))?;
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

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            detect_instances,
            plan_update,
            apply_update
        ])
        .run(tauri::generate_context!())
        .expect("error while running Bonegrader");
}
