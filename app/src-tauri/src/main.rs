// Prevent an extra console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Bonegrader desktop app (Tauri v2).
//!
//! The GUI is intentionally thin: every command wraps the same pipeline the
//! headless CLI uses ([`bonegrader_client::session`]). Blocking work (HTTP,
//! hashing, filesystem) runs off the UI thread via `spawn_blocking`, and
//! download/apply progress is streamed to the frontend through the
//! `update-progress` event. Errors reach the UI with their full cause chain.

use std::path::{Path, PathBuf};

use bonegrader_client::apply::{ApplyOptions, Progress};
use bonegrader_client::detect::{
    default_dotminecraft, discover_all, DetectedInstance, LauncherKind,
};
use bonegrader_client::exec::Decisions;
use bonegrader_client::fetch::check_url;
use bonegrader_client::http::HttpFetcher;
use bonegrader_client::install;
use bonegrader_client::session::{
    client_compat, fetch_manifest, now_rfc3339, restorable, timestamp, trusted_keys, undo_last,
    ClientCompat, FetchedManifest, Restorable, SignatureStatus, StaleManifest, Updater,
    CLIENT_VERSION,
};
use bonegrader_core::manifest::{Loader, ServerInfo};
use bonegrader_core::paths::is_safe_component;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

const PROGRESS_EVENT: &str = "update-progress";
/// Prefix of the error the UI answers with a fresh check (see `main.js`).
const STALE_PREFIX: &str = "[stale] ";

/// UI-facing error: the whole `anyhow` chain ("context: cause: cause"). A
/// plan that went stale is marked so the UI can re-check automatically.
fn err(e: anyhow::Error) -> String {
    if e.downcast_ref::<StaleManifest>().is_some() {
        format!("{STALE_PREFIX}{e}")
    } else {
        format!("{e:#}")
    }
}

/// Run blocking work off the UI thread.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
        .map_err(err)
}

fn fetch(base_url: &str) -> anyhow::Result<FetchedManifest> {
    fetch_manifest(&HttpFetcher::new(), base_url, &trusted_keys()?)
}

#[tauri::command]
fn app_version() -> &'static str {
    CLIENT_VERSION
}

/// Open a download page in the system browser (HTTPS only).
#[tauri::command]
fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    check_url(&url).map_err(|e| e.to_string())?;
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// List every instance we can auto-detect.
#[tauri::command]
async fn detect_instances() -> Result<Vec<DetectedInstance>, String> {
    blocking(|| Ok(discover_all())).await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestInfo {
    pack_name: String,
    channel: String,
    generated_at: String,
    loader: Loader,
    signature: SignatureStatus,
    compat: ClientCompat,
    server: Option<ServerInfo>,
    files: usize,
}

/// Check the channel URL and describe what it publishes.
#[tauri::command]
async fn manifest_info(base_url: String) -> Result<ManifestInfo, String> {
    blocking(move || {
        let f = fetch(&base_url)?;
        let m = f.manifest;
        Ok(ManifestInfo {
            compat: client_compat(m.client.as_ref(), CLIENT_VERSION),
            pack_name: m.pack_name,
            channel: m.channel,
            generated_at: m.generated_at,
            loader: m.loader,
            signature: f.signature,
            server: m.server,
            files: m.files.len(),
        })
    })
    .await
}

/// Compute (but don't apply) the update for an instance against a channel.
/// Returns the plan view plus the update that could be undone, if any.
#[tauri::command]
async fn plan_update(instance: String, base_url: String) -> Result<serde_json::Value, String> {
    blocking(move || {
        let inst = PathBuf::from(&instance);
        let fetcher = HttpFetcher::new();
        let keys = trusted_keys()?;
        let up = Updater {
            instance: &inst,
            base_url: &base_url,
            fetcher: &fetcher,
            keys: &keys,
        };
        let prepared = up.prepare()?;
        let mut view = serde_json::to_value(prepared.view())?;
        view["restorable"] = serde_json::to_value(restorable(&inst))?;
        Ok(view)
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApplyResult {
    installed: usize,
    downloaded: usize,
    reused: usize,
    replaced: usize,
    deleted: usize,
    seeded: usize,
    server_added: bool,
    backup_dir: Option<String>,
    warnings: Vec<String>,
}

/// Apply the update with the player's decisions — only if the server still
/// publishes the manifest the player reviewed (`manifest_id`).
#[tauri::command]
async fn apply_update(
    app: AppHandle,
    instance: String,
    base_url: String,
    manifest_id: String,
    remove_extras: Vec<String>,
    keep_collisions: Vec<String>,
) -> Result<ApplyResult, String> {
    blocking(move || {
        let inst = PathBuf::from(&instance);
        let fetcher = HttpFetcher::new();
        let keys = trusted_keys()?;
        let up = Updater {
            instance: &inst,
            base_url: &base_url,
            fetcher: &fetcher,
            keys: &keys,
        };
        let decisions = Decisions {
            remove_extras: remove_extras.into_iter().collect(),
            keep_collision_local: keep_collisions.into_iter().collect(),
        };
        let progress = |p: &Progress| {
            let _ = app.emit(PROGRESS_EVENT, p);
        };
        let outcome = up.apply_checked(
            Some(&manifest_id),
            &decisions,
            &ApplyOptions::default(),
            &timestamp(),
            &progress,
        )?;
        let r = outcome.report;
        Ok(ApplyResult {
            installed: r.installed,
            downloaded: r.downloaded,
            reused: r.reused,
            replaced: r.replaced,
            deleted: r.deleted,
            seeded: r.seeded,
            server_added: outcome.server_added,
            backup_dir: r.backup_dir.map(|p| p.display().to_string()),
            warnings: r.warnings,
        })
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UndoResult {
    restored: usize,
    removed: usize,
    backup_dir: String,
    next: Option<Restorable>,
}

/// Undo the last update of an instance.
#[tauri::command]
async fn undo_update(instance: String) -> Result<UndoResult, String> {
    blocking(move || {
        let inst = PathBuf::from(&instance);
        let r = undo_last(&inst, &timestamp())?;
        Ok(UndoResult {
            restored: r.restored,
            removed: r.removed,
            backup_dir: r.backup_dir.display().to_string(),
            next: restorable(&inst),
        })
    })
    .await
}

/// Native folder picker (instead of typing a path).
#[tauri::command]
async fn pick_folder(app: AppHandle) -> Result<Option<String>, String> {
    blocking(move || {
        Ok(app
            .dialog()
            .file()
            .blocking_pick_folder()
            .and_then(|f| f.into_path().ok())
            .map(|p| p.display().to_string()))
    })
    .await
}

/// Create a dedicated game folder for the pack under the official launcher's
/// `.minecraft` (instead of mixing the pack's mods into `.minecraft/mods`).
/// Installing NeoForge then binds a launcher profile to it.
#[tauri::command]
async fn create_instance(pack_name: String) -> Result<DetectedInstance, String> {
    blocking(move || {
        let dotmc = default_dotminecraft()
            .ok_or_else(|| anyhow::anyhow!(".minecraft-Ordner nicht gefunden"))?;
        let folder: String = pack_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "-_ ".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let folder = folder.trim();
        let folder = if is_safe_component(folder) {
            folder
        } else {
            "Bonegrader"
        };
        let path = dotmc.join("bonegrader").join(folder);
        std::fs::create_dir_all(path.join("mods"))?;
        Ok(DetectedInstance {
            name: pack_name.clone(),
            path,
            launcher: LauncherKind::Vanilla,
            mc_version: None,
            loader_type: None,
            loader_version: None,
            profile_key: None,
        })
    })
    .await
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
/// is reported instead of silently ignored — including for CurseForge, Prism
/// and manually-picked instances. Bonegrader can only auto-install for the
/// vanilla (official launcher) + NeoForge path; elsewhere it just flags the
/// mismatch so the player can fix it in their launcher.
#[tauri::command]
async fn loader_status(
    base_url: String,
    launcher: String,
    loader_type: Option<String>,
    loader_version: Option<String>,
) -> Result<LoaderStatus, String> {
    blocking(move || {
        let required = fetch(&base_url)?.manifest.loader;

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
    blocking(move || {
        let dotmc = default_dotminecraft()
            .ok_or_else(|| anyhow::anyhow!(".minecraft-Ordner nicht gefunden"))?;
        let loader_version = fetch(&base_url)?.manifest.loader.loader_version;
        let key = profile_key
            .as_deref()
            .filter(|k| !k.is_empty())
            .unwrap_or("bonegrader");
        install::ensure_client(
            &dotmc,
            &loader_version,
            &HttpFetcher::new(),
            key,
            &pack_name,
            Path::new(&game_dir),
            &now_rfc3339(),
        )
    })
    .await
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            app_version,
            open_url,
            detect_instances,
            manifest_info,
            plan_update,
            apply_update,
            undo_update,
            pick_folder,
            create_instance,
            loader_status,
            install_loader
        ])
        .run(tauri::generate_context!())
        .expect("error while running Bonegrader");
}
