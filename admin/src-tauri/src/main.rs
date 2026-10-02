// Prevent an extra console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Bonegrader Admin (Tauri v2) — preview and publish channel updates. Thin GUI
//! over the `bonegrader_publish` library (same logic as the CLI).

use std::path::{Path, PathBuf};

use bonegrader_client::http::HttpFetcher;
use bonegrader_client::session::fetch_manifest;
use bonegrader_core::manifest::{ClientInfo, ServerInfo};
use bonegrader_publish::{
    build_manifest, category_counts, diff_manifests, gc_store, load_signing_key, populate_store,
    sign_manifest, upload_channel, write_manifest, BuildOptions, Built, ManifestDiff,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

const PROGRESS_EVENT: &str = "publish-progress";

/// Everything the admin form holds. Empty strings mean "not set".
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    instance: String,
    channel: String,
    base_url: String,
    pack: String,
    #[serde(default)]
    ignore: Vec<String>,
    #[serde(default)]
    seeds: Vec<String>,
    #[serde(default)]
    sign_key: String,
    #[serde(default)]
    min_client_version: String,
    #[serde(default)]
    latest_client_version: String,
    #[serde(default)]
    client_download_url: String,
    #[serde(default)]
    server_name: String,
    #[serde(default)]
    server_address: String,
    #[serde(default)]
    ssh_host: String,
    #[serde(default)]
    remote_base: String,
    #[serde(default)]
    gc: bool,
}

fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

impl Settings {
    fn build_options(&self) -> BuildOptions {
        BuildOptions {
            pack: self.pack.trim().to_string(),
            channel: self.channel.trim().to_string(),
            base_url: self.base_url.trim().to_string(),
            mc_version: None,
            loader_version: None,
            loader_type: "neoforge".into(),
            ignore: self.ignore.clone(),
            client: Some(ClientInfo {
                min_version: opt(&self.min_client_version),
                latest_version: opt(&self.latest_client_version),
                download_url: opt(&self.client_download_url),
            }),
            server: opt(&self.server_address).map(|address| ServerInfo {
                name: opt(&self.server_name).unwrap_or_else(|| self.pack.trim().to_string()),
                address,
            }),
            seeds: self.seeds.clone(),
        }
    }

    fn build(&self) -> anyhow::Result<Built> {
        build_manifest(Path::new(self.instance.trim()), &self.build_options())
    }
}

/// UI-facing error: the whole `anyhow` chain.
fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
        .map_err(err)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreviewResult {
    diff: ManifestDiff,
    files: usize,
    mods: usize,
    resourcepacks: usize,
    shaderpacks: usize,
    seeds: usize,
    ignored: usize,
    no_mod_id: Vec<String>,
    warnings: Vec<String>,
    loader: String,
    live_reachable: bool,
    signed: bool,
}

/// Build the manifest from the instance and diff it against the live server one.
#[tauri::command]
async fn preview(settings: Settings) -> Result<PreviewResult, String> {
    blocking(move || {
        let built = settings.build()?;
        if let Some(k) = opt(&settings.sign_key) {
            load_signing_key(Path::new(&k))?; // fail early on a wrong key path
        }
        // Informational only: no signature check needed for the live diff.
        let live = fetch_manifest(&HttpFetcher::new(), settings.base_url.trim(), &[])
            .ok()
            .map(|f| f.manifest);
        let diff = diff_manifests(live.as_ref(), &built.manifest);
        let (mods, resourcepacks, shaderpacks) = category_counts(&built.manifest);
        let l = &built.manifest.loader;
        Ok(PreviewResult {
            diff,
            files: built.manifest.files.len(),
            mods,
            resourcepacks,
            shaderpacks,
            seeds: built.manifest.seeds.len(),
            ignored: built.ignored,
            no_mod_id: built.no_modid,
            warnings: built.warnings,
            loader: format!(
                "{} {} (MC {})",
                l.loader_type, l.loader_version, l.mc_version
            ),
            live_reachable: live.is_some(),
            signed: opt(&settings.sign_key).is_some(),
        })
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublishResult {
    files: usize,
    new_blobs: usize,
    gc_removed: usize,
    signed: bool,
}

/// Build, populate the local store, sign and rsync the channel to the server.
#[tauri::command]
async fn publish(app: AppHandle, settings: Settings) -> Result<PublishResult, String> {
    // Per-user folder (a fixed name in a shared temp dir could be tampered with).
    let store_root: PathBuf = app
        .path()
        .app_local_data_dir()
        .map_err(|e| e.to_string())?
        .join("publish");
    blocking(move || {
        let phase = |p: &str| {
            let _ = app.emit(PROGRESS_EVENT, p);
        };
        let key = opt(&settings.sign_key)
            .map(|k| load_signing_key(Path::new(&k)))
            .transpose()?;

        phase("build");
        let built = settings.build()?;
        let channel = built.manifest.channel.clone();
        let out = store_root.join(&channel);
        let new_blobs = populate_store(Path::new(settings.instance.trim()), &out, &built.manifest)?;
        let bytes = write_manifest(&out, &built.manifest)?;
        let signature = out.join(bonegrader_core::sign::SIGNATURE_FILE);
        match &key {
            Some(k) => sign_manifest(&out, &bytes, k)?,
            // A stale signature from an earlier signed build must not be uploaded.
            None => {
                let _ = std::fs::remove_file(&signature);
            }
        }

        phase("upload");
        upload_channel(
            &out,
            settings.ssh_host.trim(),
            settings.remote_base.trim(),
            &channel,
        )?;

        let gc_removed = if settings.gc {
            gc_store(&out, &built.manifest)?
        } else {
            0
        };
        phase("done");
        Ok(PublishResult {
            files: built.manifest.files.len(),
            new_blobs,
            gc_removed,
            signed: key.is_some(),
        })
    })
    .await
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![preview, publish])
        .run(tauri::generate_context!())
        .expect("error while running Bonegrader Admin");
}
