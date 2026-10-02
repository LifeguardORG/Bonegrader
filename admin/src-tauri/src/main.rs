// Prevent an extra console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Bonegrader Admin (Tauri v2) — preview and publish channel updates, check
//! the SSH connection, and roll a channel back. Thin GUI over the
//! `bonegrader_publish` library (same logic as the CLI and deploy scripts).

use std::path::PathBuf;

use bonegrader_client::errors::{Kind, Report};
use bonegrader_client::http::HttpFetcher;
use bonegrader_client::session::{fetch_manifest, trusted_keys};
use bonegrader_core::manifest::{ClientInfo, ServerInfo};
use bonegrader_core::sign::{PublicKey, SecretKey};
use bonegrader_publish::remote::{
    list_history, rollback, test_connection, upload_channel, Connection, HistoryEntry, Remote,
};
use bonegrader_publish::{
    build_manifest, category_counts, diff_manifests, gc_store, load_signing_key, populate_store,
    sign_manifest, write_manifest, BuildOptions, Built, ManifestDiff,
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

/// `~/x` -> `$HOME/x` (paths typed into the form never pass through a shell).
fn expand_home(p: &str) -> PathBuf {
    let p = p.trim();
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    match (p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")), home) {
        (Some(rest), Some(h)) => PathBuf::from(h).join(rest),
        _ => PathBuf::from(p),
    }
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
        build_manifest(&expand_home(&self.instance), &self.build_options())
    }

    fn key(&self) -> anyhow::Result<Option<SecretKey>> {
        opt(&self.sign_key)
            .map(|k| load_signing_key(&expand_home(&k)))
            .transpose()
    }

    fn remote(&self) -> anyhow::Result<Remote> {
        if self.ssh_host.trim().is_empty() || self.remote_base.trim().is_empty() {
            anyhow::bail!("SSH-Ziel und Remote-Basis angeben");
        }
        Remote::new(&self.ssh_host, &self.remote_base)
    }

    fn target(&self) -> Option<String> {
        let r = self.remote().ok()?;
        Some(format!(
            "{}:{}",
            r.host(),
            r.dest(self.channel.trim()).ok()?
        ))
    }
}

/// How a publish relates to the keys this build's player app trusts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum Signing {
    /// Signed with a key the player app trusts.
    Trusted,
    /// Signed, but the player app does not check signatures yet.
    SignedUnchecked,
    /// Not signed, and the player app does not check yet: works, but a
    /// taken-over web server could ship anything.
    Unsigned,
    /// Not signed, but the player app requires a signature: every update
    /// would be refused.
    UnsignedRejected,
    /// Signed with a key the player app does not trust: every update would
    /// be refused.
    UntrustedKey,
}

impl Signing {
    /// Against the keys compiled into this build (`keys/manifest-signing.pub`).
    fn check(key: Option<&SecretKey>) -> anyhow::Result<Self> {
        Ok(Self::against(key, &trusted_keys()?))
    }

    fn against(key: Option<&SecretKey>, trusted: &[PublicKey]) -> Self {
        match (key, trusted.is_empty()) {
            (None, true) => Self::Unsigned,
            (None, false) => Self::UnsignedRejected,
            (Some(_), true) => Self::SignedUnchecked,
            (Some(k), false) if trusted.contains(&k.public()) => Self::Trusted,
            (Some(_), false) => Self::UntrustedKey,
        }
    }

    fn blocks_publishing(self) -> bool {
        matches!(self, Self::UnsignedRejected | Self::UntrustedKey)
    }
}

fn loader_name(t: &str) -> &str {
    match t {
        "neoforge" => "NeoForge",
        "forge" => "Forge",
        "fabric" => "Fabric",
        "quilt" => "Quilt",
        other => other,
    }
}

fn report(e: anyhow::Error) -> Report {
    Report::from(&e)
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, Report> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| Report {
            kind: Kind::Other,
            message: e.to_string(),
            detail: e.to_string(),
        })?
        .map_err(report)
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
    pack: String,
    channel: String,
    live_reachable: bool,
    live_generated_at: Option<String>,
    signing: Signing,
    /// `deploy@host:/srv/bonegrader/main`, if the SSH fields are filled in.
    target: Option<String>,
}

/// Build the manifest from the instance and diff it against the live server one.
#[tauri::command]
async fn preview(settings: Settings) -> Result<PreviewResult, Report> {
    blocking(move || {
        let built = settings.build()?;
        let key = settings.key()?; // fail early on a wrong key path
        let signing = Signing::check(key.as_ref())?;
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
                loader_name(&l.loader_type),
                l.loader_version,
                l.mc_version
            ),
            pack: built.manifest.pack_name.clone(),
            channel: built.manifest.channel.clone(),
            live_reachable: live.is_some(),
            live_generated_at: live.map(|m| m.generated_at),
            signing,
            target: settings.target(),
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
    /// Where the replaced manifest was saved (for a rollback).
    previous: Option<String>,
}

/// Build, populate the local store, sign and rsync the channel to the server.
#[tauri::command]
async fn publish(app: AppHandle, settings: Settings) -> Result<PublishResult, Report> {
    // Per-user folder (a fixed name in a shared temp dir could be tampered with).
    let store_root: PathBuf = app
        .path()
        .app_local_data_dir()
        .map_err(|e| report(e.into()))?
        .join("publish");
    blocking(move || {
        let phase = |p: &str| {
            let _ = app.emit(PROGRESS_EVENT, p);
        };
        let remote = settings.remote()?;
        let key = settings.key()?;
        let signing = Signing::check(key.as_ref())?;
        if signing.blocks_publishing() {
            anyhow::bail!(
                "Nicht veröffentlicht: Die Spieler-App würde diesen Stand ablehnen ({}).",
                match signing {
                    Signing::UnsignedRejected => "sie verlangt eine Signatur",
                    _ => "der Schlüssel gehört nicht zu keys/manifest-signing.pub",
                }
            );
        }

        phase("build");
        let built = settings.build()?;
        let channel = built.manifest.channel.clone();
        let out = store_root.join(&channel);
        let new_blobs = populate_store(&expand_home(&settings.instance), &out, &built.manifest)?;
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
        let previous = upload_channel(&out, &remote, &channel)?;

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
            previous,
        })
    })
    .await
}

/// Log in without prompting and check what an upload needs.
#[tauri::command]
async fn ssh_test(settings: Settings) -> Result<Connection, Report> {
    blocking(move || test_connection(&settings.remote()?)).await
}

/// The channel's live manifest and its history, newest first.
#[tauri::command]
async fn history(settings: Settings) -> Result<Vec<HistoryEntry>, Report> {
    blocking(move || list_history(&settings.remote()?, settings.channel.trim())).await
}

/// Switch the channel back to a history entry; returns where the replaced
/// manifest was saved.
#[tauri::command]
async fn rollback_to(settings: Settings, name: String) -> Result<String, Report> {
    blocking(move || rollback(&settings.remote()?, settings.channel.trim(), &name)).await
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            preview,
            publish,
            ssh_test,
            history,
            rollback_to
        ])
        .run(tauri::generate_context!())
        .expect("error while running Bonegrader Admin");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_is_checked_against_the_trusted_keys() {
        let a = SecretKey::generate().unwrap();
        let b = SecretKey::generate().unwrap();
        let trusted = [a.public()];
        assert_eq!(Signing::against(None, &[]), Signing::Unsigned);
        assert_eq!(Signing::against(Some(&a), &[]), Signing::SignedUnchecked);
        assert_eq!(Signing::against(Some(&a), &trusted), Signing::Trusted);
        assert_eq!(Signing::against(None, &trusted), Signing::UnsignedRejected);
        assert_eq!(Signing::against(Some(&b), &trusted), Signing::UntrustedKey);
        assert!(Signing::UnsignedRejected.blocks_publishing());
        assert!(Signing::UntrustedKey.blocks_publishing());
        assert!(!Signing::Unsigned.blocks_publishing());
    }

    #[test]
    fn remote_targets_need_both_fields() {
        let mut s: Settings = serde_json::from_value(serde_json::json!({
            "instance": "/x", "channel": "main", "baseUrl": "https://h/main", "pack": "P",
            "sshHost": "deploy@h", "remoteBase": "/srv/bonegrader/"
        }))
        .unwrap();
        assert_eq!(s.target().as_deref(), Some("deploy@h:/srv/bonegrader/main"));
        s.remote_base = String::new();
        assert!(s.remote().is_err() && s.target().is_none());
    }
}
