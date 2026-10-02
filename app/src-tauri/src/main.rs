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
use bonegrader_client::detect::{default_dotminecraft, discover_all, DetectedInstance};
use bonegrader_client::exec::{finalize, Decisions};
use bonegrader_client::http::{fetch_manifest, HttpFetcher};
use bonegrader_client::install;
use bonegrader_core::diff::{compute_plan, UpdatePlan};
use bonegrader_core::manifest::Category;
use bonegrader_core::scan::scan_instance;
use bonegrader_core::state::ClientState;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

const STATE_FILE: &str = ".bonegrader-state.json";
const PROGRESS_EVENT: &str = "update-progress";
const CATEGORIES: [Category; 3] = [Category::Mod, Category::Resourcepack, Category::Shaderpack];

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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoaderStatus {
    /// `"ok" | "missing" | "mismatch" | "unmanaged" | "unknown"`.
    state: String,
    /// Bonegrader can install/repair the loader itself (vanilla + NeoForge).
    can_install: bool,
    /// Convenience for the frontend: show an install/repair affordance.
    needed: bool,
    java_available: bool,
    /// The required NeoForge version exists under `.minecraft/versions`
    /// (it may still not be the profile the player actually launches).
    on_disk: bool,
    /// Loader the pack requires (from the manifest).
    required_type: String,
    required_version: String,
    mc_version: String,
    /// Loader the selected instance currently uses, if known.
    installed_type: Option<String>,
    installed_version: Option<String>,
}

/// Compare the selected instance's loader against the pack's required loader.
///
/// This runs for *every* launcher type, so a wrong or missing NeoForge version
/// is reported instead of silently ignored — including for CurseForge and
/// manually-picked instances. Bonegrader can only auto-install for the vanilla
/// (official launcher) + NeoForge path; elsewhere it just flags the mismatch so
/// the player can fix it in their launcher.
#[tauri::command]
async fn loader_status(
    base_url: String,
    launcher: String,
    loader_type: Option<String>,
    loader_version: Option<String>,
) -> Result<LoaderStatus, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<LoaderStatus, String> {
        let required = fetch_manifest(&base_url).map_err(|e| e.to_string())?.loader;

        let is_vanilla = launcher.eq_ignore_ascii_case("vanilla");
        let required_neoforge = required.loader_type.eq_ignore_ascii_case("neoforge");
        let can_install = is_vanilla && required_neoforge;

        let type_ok = loader_type
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case(&required.loader_type));
        let version_ok = loader_version
            .as_deref()
            .is_some_and(|v| v == required.loader_version);

        // Vanilla path: also check whether the required version exists on disk
        // and whether a Java runtime is reachable for a fresh install.
        let dotmc = is_vanilla.then(default_dotminecraft).flatten();
        let on_disk = dotmc
            .as_deref()
            .is_some_and(|d| install::is_installed(d, &required.loader_version));
        let java_available = dotmc
            .as_deref()
            .is_some_and(|d| install::find_java(d).is_some());

        let state = if type_ok && version_ok {
            "ok"
        } else if loader_type.is_none() && loader_version.is_none() {
            // No loader recorded for the instance at all.
            if is_vanilla {
                "missing"
            } else {
                "unknown"
            }
        } else if !can_install {
            // We can see the mismatch but can't fix it (CurseForge / non-NeoForge).
            "unmanaged"
        } else if type_ok {
            "mismatch" // right loader family, wrong version
        } else {
            "missing" // wrong family entirely, or a version we can't match
        };

        let needed = can_install && state != "ok";

        Ok(LoaderStatus {
            state: state.into(),
            can_install,
            needed,
            java_available,
            on_disk,
            required_type: required.loader_type,
            required_version: required.loader_version,
            mc_version: required.mc_version,
            installed_type: loader_type,
            installed_version: loader_version,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Install the pack's NeoForge version and bind a launcher profile to it. When
/// the player selected an existing official-launcher profile, `profile_key`
/// targets that profile so it is converted to NeoForge *in place* — instead of
/// leaving their selected (vanilla) profile untouched and adding a duplicate.
#[tauri::command]
async fn install_loader(
    base_url: String,
    game_dir: String,
    pack_name: String,
    profile_key: Option<String>,
) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<bool, String> {
        let dotmc =
            default_dotminecraft().ok_or_else(|| ".minecraft-Ordner nicht gefunden".to_string())?;
        let loader_version = fetch_manifest(&base_url)
            .map_err(|e| e.to_string())?
            .loader
            .loader_version;
        let fetcher = HttpFetcher::new();
        let key = profile_key
            .as_deref()
            .filter(|k| !k.is_empty())
            .unwrap_or("bonegrader");
        install::ensure_client(
            &dotmc,
            &loader_version,
            &fetcher,
            key,
            &pack_name,
            Path::new(&game_dir),
            &now_iso(),
        )
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            detect_instances,
            plan_update,
            apply_update,
            loader_status,
            install_loader
        ])
        .run(tauri::generate_context!())
        .expect("error while running Bonegrader");
}
