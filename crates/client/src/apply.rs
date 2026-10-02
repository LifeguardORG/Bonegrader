//! Transactional application of an [`ExecutionPlan`].

use crate::exec::ExecutionPlan;
use anyhow::{bail, Context, Result};
use bonegrader_core::hash::sha1_bytes;
use bonegrader_core::manifest::Manifest;
use bonegrader_core::paths::is_safe_managed_path;
use bonegrader_core::state::{ClientState, ManagedEntry};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Source of file bytes. Real clients back this with HTTP; tests use the
/// filesystem. Implementors need not verify hashes — [`apply`] always does.
pub trait Fetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>>;
}

/// A progress update emitted during [`apply_with_progress`]. `phase` is one of
/// `download`, `apply`, `done`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub phase: String,
    pub current: String,
    pub done_files: usize,
    pub total_files: usize,
    pub done_bytes: u64,
    pub total_bytes: u64,
}

/// What happened.
#[derive(Debug)]
pub struct ApplyReport {
    pub downloaded: usize,
    pub deleted: usize,
    /// Where replaced/removed files were moved, if any.
    pub backup_dir: Option<PathBuf>,
    /// The new client state to persist.
    pub new_state: ClientState,
}

/// Join a possibly-relative manifest url onto the base url.
pub fn resolve_url(base_url: &str, url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") || base_url.is_empty() {
        url.to_string()
    } else {
        format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            url.trim_start_matches('/')
        )
    }
}

/// Execute `exec` against `instance` (no progress reporting).
pub fn apply(
    instance: &Path,
    manifest: &Manifest,
    exec: &ExecutionPlan,
    fetcher: &dyn Fetcher,
    base_url: &str,
    old_state: &ClientState,
    timestamp: &str,
) -> Result<ApplyReport> {
    apply_with_progress(
        instance,
        manifest,
        exec,
        fetcher,
        base_url,
        old_state,
        timestamp,
        &|_| {},
    )
}

/// Execute `exec` against `instance`, calling `progress` as work proceeds.
///
/// Order of operations enforces the safety invariants:
/// 1. validate every write/delete path;
/// 2. download **and verify** everything into a temp dir — if anything fails
///    here, nothing on the instance has changed yet;
/// 3. move removed/replaced files into a timestamped backup dir, then move the
///    verified downloads into place;
/// 4. derive the new state from the manifest (managed = manifest files present).
#[allow(clippy::too_many_arguments)]
pub fn apply_with_progress(
    instance: &Path,
    manifest: &Manifest,
    exec: &ExecutionPlan,
    fetcher: &dyn Fetcher,
    base_url: &str,
    old_state: &ClientState,
    timestamp: &str,
    progress: &dyn Fn(&Progress),
) -> Result<ApplyReport> {
    // 1. Path safety.
    for d in &exec.downloads {
        if !is_safe_managed_path(&d.entry.path) {
            bail!("refusing unsafe target path: {}", d.entry.path);
        }
        if let Some(r) = &d.replaces {
            if !is_safe_managed_path(r) {
                bail!("refusing unsafe replace path: {r}");
            }
        }
    }
    for p in &exec.deletions {
        if !is_safe_managed_path(p) {
            bail!("refusing unsafe deletion path: {p}");
        }
    }

    let total_files = exec.downloads.len();
    let total_bytes: u64 = exec.downloads.iter().map(|d| d.entry.size).sum();
    let report = |phase: &str, current: &str, done_files: usize, done_bytes: u64| {
        progress(&Progress {
            phase: phase.to_string(),
            current: current.to_string(),
            done_files,
            total_files,
            done_bytes,
            total_bytes,
        });
    };

    // 2. Download + verify into temp — nothing destructive yet.
    report("download", "", 0, 0);
    let tmp_dir = instance.join(".bonegrader-tmp");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).context("creating temp dir")?;

    let mut staged: Vec<(PathBuf, String)> = Vec::new(); // (tmp file, target rel path)
    let mut done_files = 0usize;
    let mut done_bytes = 0u64;
    let staged_result = (|| -> Result<()> {
        for d in &exec.downloads {
            let url = resolve_url(base_url, &d.entry.url);
            let bytes = fetcher
                .get(&url)
                .with_context(|| format!("fetching {url}"))?;
            let got = sha1_bytes(&bytes);
            if got != d.entry.sha1 {
                bail!(
                    "sha1 mismatch for {} (want {}, got {got})",
                    d.entry.path,
                    d.entry.sha1
                );
            }
            if bytes.len() as u64 != d.entry.size {
                bail!(
                    "size mismatch for {} (want {}, got {})",
                    d.entry.path,
                    d.entry.size,
                    bytes.len()
                );
            }
            let tmp_file = tmp_dir.join(&d.entry.sha1);
            std::fs::write(&tmp_file, &bytes)?;
            staged.push((tmp_file, d.entry.path.clone()));
            done_files += 1;
            done_bytes += d.entry.size;
            report("download", &d.entry.path, done_files, done_bytes);
        }
        Ok(())
    })();
    if let Err(e) = staged_result {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(e); // instance untouched
    }

    // 3. Destructive phase (backup-then-move).
    report("apply", "", done_files, done_bytes);
    let backup_root = instance.join(".bonegrader-backup").join(timestamp);
    let mut backup_used = false;
    let mut deleted = 0;

    // 3a. Independent deletions.
    for p in &exec.deletions {
        if backup_file(instance, &backup_root, p)? {
            backup_used = true;
            deleted += 1;
        }
    }
    // 3b. Downloads: back up the replaced/occupying file, then move into place.
    for (tmp_file, target) in &staged {
        let d = exec
            .downloads
            .iter()
            .find(|d| &d.entry.path == target)
            .expect("staged download exists");
        if let Some(r) = &d.replaces {
            if backup_file(instance, &backup_root, r)? {
                backup_used = true;
            }
        }
        let dst = instance.join(target);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if dst.exists() {
            // Something still occupies the target (e.g. same-path replace already
            // handled, or an unmanaged file) — preserve it before overwriting.
            if backup_file(instance, &backup_root, target)? {
                backup_used = true;
            }
        }
        std::fs::rename(tmp_file, &dst).with_context(|| format!("installing {target}"))?;
    }
    let _ = std::fs::remove_dir_all(&tmp_dir);

    // 4. New state: managed = every manifest entry now present on disk.
    let mut managed: BTreeMap<String, ManagedEntry> = BTreeMap::new();
    for e in &manifest.files {
        if instance.join(&e.path).exists() {
            managed.insert(
                e.path.clone(),
                ManagedEntry {
                    sha1: e.sha1.clone(),
                    mod_ids: e.mod_id.clone().into_iter().collect(),
                },
            );
        }
    }

    let new_state = ClientState {
        instance_path: old_state.instance_path.clone(),
        launcher_type: old_state.launcher_type.clone(),
        channel: manifest.channel.clone(),
        last_manifest_generated_at: Some(manifest.generated_at.clone()),
        managed,
    };

    report("done", "", done_files, done_bytes);

    Ok(ApplyReport {
        downloaded: staged.len(),
        deleted,
        backup_dir: backup_used.then_some(backup_root),
        new_state,
    })
}

/// Move `instance/rel` into `backup_root/rel`. Returns whether a file was moved.
fn backup_file(instance: &Path, backup_root: &Path, rel: &str) -> Result<bool> {
    let src = instance.join(rel);
    if !src.exists() {
        return Ok(false);
    }
    let dst = backup_root.join(rel);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&src, &dst).with_context(|| format!("backing up {rel}"))?;
    Ok(true)
}
