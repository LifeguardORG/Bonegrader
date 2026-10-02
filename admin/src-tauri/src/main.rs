// Prevent an extra console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Bonegrader Admin (Tauri v2) — preview and publish channel updates. Thin GUI
//! over the `bonegrader_publish` library (same logic as the CLI).

use std::path::Path;

use bonegrader_client::http::fetch_manifest;
use bonegrader_publish::{
    build_manifest, category_counts, diff_manifests, gc_store, populate_store, upload_channel,
    write_manifest, BuildOptions, ManifestDiff,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

const PROGRESS_EVENT: &str = "publish-progress";

fn opts(pack: String, channel: String, base_url: String, ignore: Vec<String>) -> BuildOptions {
    BuildOptions {
        pack,
        channel,
        base_url,
        mc_version: None,
        loader_version: None,
        loader_type: "neoforge".into(),
        ignore,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreviewResult {
    diff: ManifestDiff,
    files: usize,
    mods: usize,
    resourcepacks: usize,
    shaderpacks: usize,
    ignored: usize,
    no_mod_id: Vec<String>,
    loader: String,
    live_reachable: bool,
}

/// Build the manifest from the instance and diff it against the live server one.
#[tauri::command]
async fn preview(
    instance: String,
    channel: String,
    base_url: String,
    pack: String,
    ignore: Vec<String>,
) -> Result<PreviewResult, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<PreviewResult, String> {
        let built = build_manifest(
            Path::new(&instance),
            &opts(pack, channel, base_url.clone(), ignore),
        )
        .map_err(|e| e.to_string())?;
        let live = fetch_manifest(&base_url).ok();
        let diff = diff_manifests(live.as_ref(), &built.manifest);
        let (mods, resourcepacks, shaderpacks) = category_counts(&built.manifest);
        let l = &built.manifest.loader;
        Ok(PreviewResult {
            diff,
            files: built.manifest.files.len(),
            mods,
            resourcepacks,
            shaderpacks,
            ignored: built.ignored,
            no_mod_id: built.no_modid,
            loader: format!(
                "{} {} (MC {})",
                l.loader_type, l.loader_version, l.mc_version
            ),
            live_reachable: live.is_some(),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublishResult {
    files: usize,
    new_blobs: usize,
    gc_removed: usize,
}

/// Build, populate the local store and rsync the channel to the server.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn publish(
    app: AppHandle,
    instance: String,
    channel: String,
    base_url: String,
    ssh_host: String,
    remote_base: String,
    pack: String,
    ignore: Vec<String>,
    gc: bool,
) -> Result<PublishResult, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<PublishResult, String> {
        let phase = |p: &str| {
            let _ = app.emit(PROGRESS_EVENT, p);
        };

        phase("build");
        let built = build_manifest(
            Path::new(&instance),
            &opts(pack, channel.clone(), base_url, ignore),
        )
        .map_err(|e| e.to_string())?;

        let out = std::env::temp_dir()
            .join("bonegrader-publish")
            .join(&channel);
        let new_blobs =
            populate_store(Path::new(&instance), &out, &built.local).map_err(|e| e.to_string())?;
        write_manifest(&out, &built.manifest).map_err(|e| e.to_string())?;

        phase("upload");
        upload_channel(&out, &ssh_host, &remote_base, &channel).map_err(|e| e.to_string())?;

        let gc_removed = if gc {
            gc_store(&out, &built.manifest).map_err(|e| e.to_string())?
        } else {
            0
        };
        phase("done");
        Ok(PublishResult {
            files: built.manifest.files.len(),
            new_blobs,
            gc_removed,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![preview, publish])
        .run(tauri::generate_context!())
        .expect("error while running Bonegrader Admin");
}
