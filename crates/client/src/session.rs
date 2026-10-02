//! The update pipeline shared by the CLI and the desktop app: fetch and verify
//! the manifest, scan the instance, plan, apply, undo. Front-ends only render
//! [`Prepared`] and pass the player's decisions back.

use crate::apply::{
    apply_with_progress, last_restorable, restore_last, ApplyOptions, ApplyReport, Progress,
    RestoreReport,
};
use crate::exec::{finalize, Decisions};
use crate::fetch::{is_not_found, resolve_url, Fetcher};
use crate::servers;
use anyhow::{bail, Context, Result};
use bonegrader_core::diff::{assess_instance, compute_plan, InstanceAssessment, UpdatePlan};
use bonegrader_core::hash::sha1_bytes;
use bonegrader_core::manifest::{
    ClientInfo, Loader, Manifest, ManifestError, SeedEntry, ServerInfo, SCHEMA_VERSION,
};
use bonegrader_core::scan::{scan_instance_cached, ScanCache};
use bonegrader_core::sign::{parse_public_keys, verify_any, PublicKey, SIGNATURE_FILE};
use bonegrader_core::state::ClientState;
use bonegrader_core::version::is_older;
use serde::{Deserialize, Serialize};
use std::io::ErrorKind;
use std::path::Path;

pub const STATE_FILE: &str = ".bonegrader-state.json";
pub const CACHE_FILE: &str = ".bonegrader-cache.json";
/// This build's version (compared with the manifest's client hints).
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const MANIFEST_LIMIT: u64 = 32 * 1024 * 1024;

/// Public keys trusted to sign manifests, compiled in from
/// `keys/manifest-signing.pub` (empty until the admin adds one).
const TRUSTED_KEYS: &str = include_str!("../../../keys/manifest-signing.pub");

pub fn trusted_keys() -> Result<Vec<PublicKey>> {
    parse_public_keys(TRUSTED_KEYS).context("keys/manifest-signing.pub ist ungültig")
}

/// How the manifest's authenticity was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SignatureStatus {
    /// Signed by a trusted key.
    Verified,
    /// This build trusts no keys yet, so no signature was required.
    NotRequired,
}

/// A downloaded, verified and validated manifest.
#[derive(Debug, Clone)]
pub struct FetchedManifest {
    pub manifest: Manifest,
    /// SHA-1 of the exact bytes — identifies this published version.
    pub id: String,
    pub signature: SignatureStatus,
}

/// Fetch `<base>/manifest.json`, check its signature (when this build trusts
/// keys), schema version and structure.
pub fn fetch_manifest(
    fetcher: &dyn Fetcher,
    base_url: &str,
    keys: &[PublicKey],
) -> Result<FetchedManifest> {
    let url = resolve_url(base_url, "manifest.json");
    let sig_url = resolve_url(base_url, SIGNATURE_FILE);
    // Two tries: a deploy swaps manifest and signature one after the other, so
    // a request in between can see a mismatched pair once.
    let mut last = None;
    for _ in 0..2 {
        let bytes = fetcher
            .get_limited(&url, MANIFEST_LIMIT)
            .with_context(|| format!("Server-Stand von {url} laden"))?;
        if keys.is_empty() {
            return parse_manifest(&bytes, SignatureStatus::NotRequired);
        }
        let sig = match fetcher.get_limited(&sig_url, 4096) {
            Ok(sig) => sig,
            Err(e) if is_not_found(&e) => bail!(
                "Der Server-Stand ist nicht signiert ({sig_url} fehlt). Bonegrader installiert nur \
                 signierte Server-Stände – bitte dem Admin Bescheid geben."
            ),
            Err(e) => return Err(e.context("Signatur laden")),
        };
        match verify_any(keys, &bytes, &String::from_utf8_lossy(&sig)) {
            Ok(_) => return parse_manifest(&bytes, SignatureStatus::Verified),
            Err(e) => last = Some(e),
        }
    }
    bail!(
        "Die Signatur des Server-Stands ist ungültig ({}). Bonegrader installiert nichts davon – \
         bitte dem Admin Bescheid geben.",
        last.map(|e| e.to_string()).unwrap_or_default()
    )
}

/// Parse and validate manifest bytes. The schema version is checked before
/// the full parse, so a newer format yields "please update" rather than a
/// confusing parse error.
pub fn parse_manifest(bytes: &[u8], signature: SignatureStatus) -> Result<FetchedManifest> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Probe {
        schema_version: u32,
    }
    let probe: Probe =
        serde_json::from_slice(bytes).context("manifest.json ist kein gültiges Manifest")?;
    if probe.schema_version > SCHEMA_VERSION {
        return Err(ManifestError::UnsupportedSchema {
            found: probe.schema_version,
            supported: SCHEMA_VERSION,
        }
        .into());
    }
    let manifest: Manifest =
        serde_json::from_slice(bytes).context("manifest.json ist kein gültiges Manifest")?;
    manifest.validate()?;
    Ok(FetchedManifest {
        manifest,
        id: sha1_bytes(bytes),
        signature,
    })
}

/// Whether this Bonegrader is recent enough for the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum ClientCompat {
    Current,
    #[serde(rename_all = "camelCase")]
    UpdateAvailable {
        latest: String,
        download_url: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    UpdateRequired {
        min: String,
        download_url: Option<String>,
    },
}

pub fn client_compat(info: Option<&ClientInfo>, own: &str) -> ClientCompat {
    let Some(info) = info else {
        return ClientCompat::Current;
    };
    if let Some(min) = info.min_version.as_ref().filter(|m| is_older(own, m)) {
        return ClientCompat::UpdateRequired {
            min: min.clone(),
            download_url: info.download_url.clone(),
        };
    }
    if let Some(latest) = info.latest_version.as_ref().filter(|l| is_older(own, l)) {
        return ClientCompat::UpdateAvailable {
            latest: latest.clone(),
            download_url: info.download_url.clone(),
        };
    }
    ClientCompat::Current
}

/// Everything a front-end needs to show before the player confirms.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub fetched: FetchedManifest,
    pub state: ClientState,
    pub plan: UpdatePlan,
    /// Seed files that will be created.
    pub seeds: Vec<SeedEntry>,
    /// The pack's server, if it still has to be added to the multiplayer list.
    pub server: Option<ServerInfo>,
    pub assessment: InstanceAssessment,
    pub compat: ClientCompat,
}

impl Prepared {
    /// True if applying would change nothing.
    pub fn is_noop(&self) -> bool {
        self.plan.is_noop() && self.seeds.is_empty() && self.server.is_none()
    }

    /// Bytes the downloads and seeds add up to (local copies included).
    pub fn download_bytes(&self) -> u64 {
        let dl: u64 = self.plan.downloads.iter().map(|d| d.entry.size).sum();
        dl + self.seeds.iter().map(|s| s.size).sum::<u64>()
    }

    /// A serializable summary for the UI.
    pub fn view(&self) -> PlanView<'_> {
        let m = &self.fetched.manifest;
        PlanView {
            manifest_id: &self.fetched.id,
            pack_name: &m.pack_name,
            channel: &m.channel,
            generated_at: &m.generated_at,
            loader: &m.loader,
            signature: self.fetched.signature,
            plan: &self.plan,
            seeds: self.seeds.iter().map(|s| s.path.as_str()).collect(),
            server: self.server.as_ref(),
            assessment: &self.assessment,
            compat: &self.compat,
            noop: self.is_noop(),
            download_bytes: self.download_bytes(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanView<'a> {
    pub manifest_id: &'a str,
    pub pack_name: &'a str,
    pub channel: &'a str,
    pub generated_at: &'a str,
    pub loader: &'a Loader,
    pub signature: SignatureStatus,
    pub plan: &'a UpdatePlan,
    pub seeds: Vec<&'a str>,
    pub server: Option<&'a ServerInfo>,
    pub assessment: &'a InstanceAssessment,
    pub compat: &'a ClientCompat,
    pub noop: bool,
    pub download_bytes: u64,
}

/// The server state changed between the player's check and the update.
#[derive(Debug, thiserror::Error)]
#[error("Der Server-Stand hat sich seit der Prüfung geändert – bitte erneut prüfen.")]
pub struct StaleManifest;

/// The result of [`Updater::apply`].
#[derive(Debug)]
pub struct UpdateOutcome {
    pub report: ApplyReport,
    /// The pack's server was added to the multiplayer list.
    pub server_added: bool,
}

/// One instance, one channel.
pub struct Updater<'a> {
    pub instance: &'a Path,
    pub base_url: &'a str,
    pub fetcher: &'a dyn Fetcher,
    pub keys: &'a [PublicKey],
}

impl Updater<'_> {
    /// Fetch + verify the manifest (first, so a network problem shows before
    /// the slow scan), scan the instance and plan the update. Changes nothing.
    pub fn prepare(&self) -> Result<Prepared> {
        if !self.instance.is_dir() {
            bail!("Instanz-Ordner nicht gefunden: {}", self.instance.display());
        }
        let fetched = fetch_manifest(self.fetcher, self.base_url, self.keys)?;
        let state = load_state(self.instance)?;
        let mut cache = load_cache(self.instance);
        let local = scan_instance_cached(self.instance, &crate::CATEGORIES, &mut cache)
            .context("Instanz einlesen")?;
        save_cache(self.instance, &cache);

        let m = &fetched.manifest;
        let plan = compute_plan(m, &local, &state);
        let seeds = m
            .seeds
            .iter()
            .filter(|s| !state.seeded.contains(&s.path) && !self.instance.join(&s.path).exists())
            .cloned()
            .collect();
        let server = pending_server(self.instance, m, &state);
        let assessment = assess_instance(m, &local, &state);
        let compat = client_compat(m.client.as_ref(), CLIENT_VERSION);
        Ok(Prepared {
            fetched,
            state,
            plan,
            seeds,
            server,
            assessment,
            compat,
        })
    }

    /// Apply a prepared plan with the player's decisions and persist the new
    /// state. Refuses when this Bonegrader is older than the manifest allows.
    pub fn apply(
        &self,
        prepared: &Prepared,
        decisions: &Decisions,
        opts: &ApplyOptions,
        timestamp: &str,
        progress: &(dyn Fn(&Progress) + Sync),
    ) -> Result<UpdateOutcome> {
        if let ClientCompat::UpdateRequired { min, .. } = &prepared.compat {
            bail!(
                "Diese Bonegrader-Version ({CLIENT_VERSION}) ist zu alt für den aktuellen Server-Stand – \
                 mindestens {min} wird benötigt. Bitte zuerst Bonegrader aktualisieren."
            );
        }
        let manifest = &prepared.fetched.manifest;
        let mut exec = finalize(&prepared.plan, decisions);
        exec.seeds = prepared.seeds.clone();
        let mut report = apply_with_progress(
            self.instance,
            manifest,
            &exec,
            self.fetcher,
            self.base_url,
            &prepared.state,
            timestamp,
            opts,
            progress,
        )?;

        let mut server_added = false;
        if let Some(server) = &prepared.server {
            match servers::add_server(self.instance, &server.name, &server.address) {
                Ok(added) => {
                    server_added = added;
                    report.new_state.server_added = Some(server.address.clone());
                }
                Err(e) => report
                    .warnings
                    .push(format!("Serverliste nicht aktualisiert: {e:#}")),
            }
        } else if let Some(server) = &manifest.server {
            // Listed already (by us or the player): remember it, so a later
            // removal sticks. An unreadable list is retried next time instead.
            if servers::has_server(self.instance, &server.address).unwrap_or(false) {
                report.new_state.server_added = Some(server.address.clone());
            }
        }
        save_state(self.instance, &report.new_state)?;
        Ok(UpdateOutcome {
            report,
            server_added,
        })
    }

    /// Prepare again and apply — but only if the server still publishes the
    /// manifest the player reviewed (`expected_manifest_id` from that check).
    pub fn apply_checked(
        &self,
        expected_manifest_id: Option<&str>,
        decisions: &Decisions,
        opts: &ApplyOptions,
        timestamp: &str,
        progress: &(dyn Fn(&Progress) + Sync),
    ) -> Result<UpdateOutcome> {
        let prepared = self.prepare()?;
        if expected_manifest_id.is_some_and(|id| id != prepared.fetched.id) {
            return Err(StaleManifest.into());
        }
        self.apply(&prepared, decisions, opts, timestamp, progress)
    }
}

fn pending_server(instance: &Path, manifest: &Manifest, state: &ClientState) -> Option<ServerInfo> {
    let server = manifest.server.as_ref()?;
    if state
        .server_added
        .as_deref()
        .is_some_and(|a| servers::same_address(a, &server.address))
    {
        return None;
    }
    // Unreadable list: leave it alone rather than risk the player's entries.
    match servers::has_server(instance, &server.address) {
        Ok(false) => Some(server.clone()),
        _ => None,
    }
}

/// The update that [`undo_last`] would revert.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Restorable {
    pub timestamp: String,
    pub manifest_generated_at: String,
    pub files: usize,
}

pub fn restorable(instance: &Path) -> Option<Restorable> {
    last_restorable(instance).map(|(_, r)| Restorable {
        timestamp: r.timestamp,
        manifest_generated_at: r.manifest_generated_at,
        files: r.installed.len() + r.backed_up.len(),
    })
}

/// Undo the newest update and restore the state from before it.
pub fn undo_last(instance: &Path, timestamp: &str) -> Result<RestoreReport> {
    let report = restore_last(instance, timestamp)?;
    save_state(instance, &report.previous_state)?;
    Ok(report)
}

pub fn load_state(instance: &Path) -> Result<ClientState> {
    let path = instance.join(STATE_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => ClientState::from_json(&text).with_context(|| {
            format!(
                "{} ist beschädigt – löschen, dann liest Bonegrader die Instanz neu ein",
                path.display()
            )
        }),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(ClientState {
            instance_path: instance.to_string_lossy().into(),
            launcher_type: "manual".into(),
            channel: "main".into(),
            ..Default::default()
        }),
        Err(e) => Err(e).with_context(|| format!("{} lesen", path.display())),
    }
}

pub fn save_state(instance: &Path, state: &ClientState) -> Result<()> {
    let tmp = instance.join(format!("{STATE_FILE}.tmp"));
    std::fs::write(&tmp, state.to_json_pretty()?)
        .with_context(|| format!("{} schreiben", tmp.display()))?;
    std::fs::rename(&tmp, instance.join(STATE_FILE)).context("Status speichern")?;
    Ok(())
}

/// The scan cache is an optimisation only: unreadable means empty.
fn load_cache(instance: &Path) -> ScanCache {
    std::fs::read(instance.join(CACHE_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_cache(instance: &Path, cache: &ScanCache) {
    let tmp = instance.join(format!("{CACHE_FILE}.tmp"));
    if let Ok(bytes) = serde_json::to_vec(cache) {
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, instance.join(CACHE_FILE));
        }
    }
}

/// Filesystem-safe UTC timestamp for backup folders (no ':' — Windows-safe).
pub fn timestamp() -> String {
    let n = time::OffsetDateTime::now_utc();
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

/// RFC 3339 "now" (launcher profile timestamps).
pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_key_file_parses() {
        trusted_keys().expect("keys/manifest-signing.pub must parse");
    }

    #[test]
    fn compat_levels() {
        let info = ClientInfo {
            min_version: Some("1.2.0".into()),
            latest_version: Some("1.3.0".into()),
            download_url: Some("https://x/".into()),
        };
        assert!(matches!(
            client_compat(Some(&info), "1.1.9"),
            ClientCompat::UpdateRequired { .. }
        ));
        assert!(matches!(
            client_compat(Some(&info), "1.2.0"),
            ClientCompat::UpdateAvailable { .. }
        ));
        assert_eq!(client_compat(Some(&info), "1.3.0"), ClientCompat::Current);
        assert_eq!(client_compat(None, "0.1.0"), ClientCompat::Current);
        let json = serde_json::to_string(&client_compat(Some(&info), "1.2.0")).unwrap();
        assert!(
            json.contains("\"state\":\"updateAvailable\"") && json.contains("downloadUrl"),
            "{json}"
        );
    }

    #[test]
    fn newer_schema_asks_for_an_update() {
        let json = br#"{"schemaVersion":99,"totally":"different"}"#;
        let err = parse_manifest(json, SignatureStatus::NotRequired).unwrap_err();
        assert!(format!("{err:#}").contains("aktualisieren"), "{err:#}");
    }

    #[test]
    fn timestamps_are_filesystem_safe() {
        let ts = timestamp();
        assert_eq!(ts.len(), 15);
        assert!(bonegrader_core::paths::is_safe_component(&ts));
    }
}
