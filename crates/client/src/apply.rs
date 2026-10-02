//! Transactional application of an [`ExecutionPlan`].
//!
//! 1. **Validate** every path and checksum the plan uses.
//! 2. **Stage**: download (or copy a matching local file) into
//!    `.bonegrader-tmp/<sha1>` and verify size, SHA-1 and SHA-256 — several
//!    files in parallel, with retries. Verified blobs survive a failed run, so a
//!    retry only fetches what is still missing. The instance is untouched.
//! 3. **Pre-flight**: every file that will be moved must be movable (on
//!    Windows a running game or a virus scanner may hold it open).
//! 4. **Commit**: move replaced/removed files into a fresh backup folder and the
//!    staged blobs into place, journalling every step. If any step fails, the
//!    journal is rolled back and the instance is exactly as before.
//! 5. **Record** what happened in `update.json` inside the backup folder (so
//!    [`restore_last`] can undo it) and prune old backups.

use crate::exec::ExecutionPlan;
use crate::fetch::{is_permanent, resolve_url, Fetcher};
use anyhow::{anyhow, bail, Context, Result};
use bonegrader_core::hash::Hasher;
use bonegrader_core::manifest::{is_lower_hex, Manifest};
use bonegrader_core::paths::{is_safe_component, is_safe_managed_path, is_safe_seed_path};
use bonegrader_core::state::{ClientState, ManagedEntry};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Staging folder inside the instance (same filesystem → atomic renames).
pub const TMP_DIR: &str = ".bonegrader-tmp";
/// One timestamped folder per update below this.
pub const BACKUP_DIR: &str = ".bonegrader-backup";
/// What an update did, stored in its backup folder.
pub const RECORD_FILE: &str = "update.json";
/// Inside a backup folder: the files an undone update had installed.
const UNDONE_DIR: &str = "undone";

/// Tuning knobs for [`apply_with_progress`].
#[derive(Debug, Clone)]
pub struct ApplyOptions {
    /// Concurrent downloads.
    pub parallel: usize,
    /// Extra attempts per file after a transient failure.
    pub retries: u32,
    /// Wait before the first retry; doubles for each further one.
    pub retry_delay: Duration,
    /// Backup folders to keep (older ones are deleted after an update).
    pub keep_backups: usize,
    /// Set to `true` to stop while downloading. Honoured until the commit
    /// starts (the instance is untouched until then); verified downloads are
    /// kept for the next attempt.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            parallel: 4,
            retries: 2,
            retry_delay: Duration::from_secs(1),
            keep_backups: 5,
            cancel: None,
        }
    }
}

impl ApplyOptions {
    fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::SeqCst))
    }
}

/// The player cancelled the update before anything was changed.
#[derive(Debug, thiserror::Error)]
#[error("Abgebrochen – es wurde nichts verändert.")]
pub struct Cancelled;

/// A download did not match the manifest (size or checksum).
#[derive(Debug, thiserror::Error)]
#[error("{file}: {problem}")]
pub struct BadDownload {
    pub file: String,
    pub problem: String,
}

/// A file the update must move is held open by another program.
#[derive(Debug, thiserror::Error)]
#[error(
    "die Datei wird von einem anderen Programm benutzt – läuft Minecraft noch? Bitte \
     schließen und erneut versuchen."
)]
pub struct FileInUse;

/// A progress update emitted during [`apply_with_progress`]. `phase` is one of
/// `download`, `apply`, `done`. Byte counts move while a file is transferred.
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
    /// Distinct files fetched from the server.
    pub downloaded: usize,
    /// Distinct files taken from an earlier attempt or a local copy.
    pub reused: usize,
    /// Files written into the instance (managed files and seeds).
    pub installed: usize,
    /// Files removed (moved into the backup): removals, duplicates, resolved
    /// collisions and chosen extras.
    pub deleted: usize,
    /// Old versions moved into the backup because a download replaced them.
    pub replaced: usize,
    /// Seed files created.
    pub seeded: usize,
    /// Where replaced/removed files were moved, if anything changed.
    pub backup_dir: Option<PathBuf>,
    /// The new client state to persist.
    pub new_state: ClientState,
    /// Non-fatal problems (the update itself succeeded).
    pub warnings: Vec<String>,
}

/// A file moved into a backup folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackedUp {
    /// Instance-relative path it came from.
    pub original: String,
    /// Path relative to the backup folder.
    pub backup: String,
}

/// The `update.json` written into each backup folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateRecord {
    pub format: u32,
    pub timestamp: String,
    pub manifest_generated_at: String,
    /// Instance-relative paths this update wrote.
    pub installed: Vec<String>,
    pub backed_up: Vec<BackedUp>,
    /// State before the update (restored by [`restore_last`]).
    pub previous_state: ClientState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undone_at: Option<String>,
}

/// Execute `exec` against `instance` with default options and no progress.
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
        &ApplyOptions::default(),
        &|_| {},
    )
}

/// Execute `exec` against `instance`, calling `progress` as work proceeds.
/// See the module docs for the order of operations.
#[allow(clippy::too_many_arguments)]
pub fn apply_with_progress(
    instance: &Path,
    manifest: &Manifest,
    exec: &ExecutionPlan,
    fetcher: &dyn Fetcher,
    base_url: &str,
    old_state: &ClientState,
    timestamp: &str,
    opts: &ApplyOptions,
    progress: &(dyn Fn(&Progress) + Sync),
) -> Result<ApplyReport> {
    // 1. Validate.
    validate(exec)?;
    let blobs = collect_blobs(exec, base_url)?;
    let reporter = Reporter::new(
        progress,
        blobs.len(),
        blobs.values().map(|b| b.size).sum(),
        opts.cancel.as_deref(),
    );
    if opts.cancelled() {
        return Err(Cancelled.into());
    }

    // 2. Stage — nothing destructive yet; on failure the verified blobs stay
    //    for the next attempt.
    reporter.phase("download");
    let tmp_dir = instance.join(TMP_DIR);
    fs::create_dir_all(&tmp_dir).with_context(|| format!("{} anlegen", tmp_dir.display()))?;
    let staged = stage_all(instance, &tmp_dir, &blobs, fetcher, opts, &reporter)?;
    if opts.cancelled() {
        return Err(Cancelled.into()); // last chance: nothing was changed yet
    }

    // 3 + 4. Pre-flight and journalled commit.
    reporter.phase("apply");
    let backup_root = unique_dir(&instance.join(BACKUP_DIR), timestamp);
    let mut journal = Journal::default();
    let committed = preflight(instance, exec)
        .and_then(|()| commit(instance, exec, &tmp_dir, &backup_root, &mut journal));
    let committed = match committed {
        Ok(c) => c,
        Err(e) => {
            let problems = journal.rollback();
            let _ = remove_empty_dirs(&backup_root);
            if problems.is_empty() {
                return Err(e.context("Update abgebrochen – alle Änderungen wurden zurückgenommen"));
            }
            return Err(e.context(format!(
                "Update abgebrochen, Rücknahme unvollständig ({}). Gesicherte Dateien liegen in {}",
                problems.join("; "),
                backup_root.display()
            )));
        }
    };

    // 5. Record, clean up.
    let new_state = derive_state(instance, manifest, old_state);
    let mut warnings = Vec::new();
    let changed = !committed.installed.is_empty() || !committed.backed_up.is_empty();
    if changed {
        let record = UpdateRecord {
            format: 1,
            timestamp: timestamp.to_string(),
            manifest_generated_at: manifest.generated_at.clone(),
            installed: committed.installed.clone(),
            backed_up: committed.backed_up.clone(),
            previous_state: old_state.clone(),
            undone_at: None,
        };
        if let Err(e) = write_record(&backup_root, &record) {
            warnings.push(format!(
                "Update-Protokoll konnte nicht gespeichert werden: {e:#}"
            ));
        }
    }
    let _ = fs::remove_dir_all(&tmp_dir);
    prune_backups(&instance.join(BACKUP_DIR), opts.keep_backups, &backup_root);
    reporter.phase("done");

    Ok(ApplyReport {
        downloaded: staged.downloaded,
        reused: staged.reused,
        installed: committed.installed.len(),
        deleted: committed.deleted,
        replaced: committed.replaced,
        seeded: committed.seeded,
        backup_dir: changed.then_some(backup_root),
        new_state,
        warnings,
    })
}

// --- 1. validation ------------------------------------------------------------

fn validate(exec: &ExecutionPlan) -> Result<()> {
    for d in &exec.downloads {
        if !is_safe_managed_path(&d.entry.path) {
            bail!("unsicherer Zielpfad: {}", d.entry.path);
        }
        for p in d.replaces.iter().chain(&d.local_copy) {
            if !is_safe_managed_path(p) {
                bail!("unsicherer Pfad: {p}");
            }
        }
        check_hashes(&d.entry.path, &d.entry.sha1, d.entry.sha256.as_deref())?;
    }
    for p in &exec.deletions {
        if !is_safe_managed_path(p) {
            bail!("unsicherer Löschpfad: {p}");
        }
    }
    for s in &exec.seeds {
        if !is_safe_seed_path(&s.path) {
            bail!("unsicherer Seed-Pfad: {}", s.path);
        }
        check_hashes(&s.path, &s.sha1, s.sha256.as_deref())?;
    }
    Ok(())
}

/// The SHA-1 doubles as the staging file name, so it must be plain hex.
fn check_hashes(path: &str, sha1: &str, sha256: Option<&str>) -> Result<()> {
    if !is_lower_hex(sha1, 40) || sha256.is_some_and(|s| !is_lower_hex(s, 64)) {
        bail!("{path}: ungültige Prüfsumme im Manifest");
    }
    Ok(())
}

// --- 2. staging ---------------------------------------------------------------

/// One distinct content to stage (several targets may share it).
struct Blob {
    sha1: String,
    sha256: Option<String>,
    size: u64,
    url: String,
    local_copy: Option<String>,
    /// First target path, for progress and error messages.
    label: String,
}

fn collect_blobs(exec: &ExecutionPlan, base_url: &str) -> Result<BTreeMap<String, Blob>> {
    let mut blobs: BTreeMap<String, Blob> = BTreeMap::new();
    let items = exec
        .downloads
        .iter()
        .map(|d| {
            let e = &d.entry;
            (
                &e.path,
                &e.sha1,
                &e.sha256,
                e.size,
                &e.url,
                d.local_copy.as_ref(),
            )
        })
        .chain(
            exec.seeds
                .iter()
                .map(|s| (&s.path, &s.sha1, &s.sha256, s.size, &s.url, None)),
        );
    for (path, sha1, sha256, size, url, local_copy) in items {
        match blobs.get_mut(sha1) {
            Some(b) => {
                let conflicting = b.size != size
                    || (b.sha256.is_some() && sha256.is_some() && &b.sha256 != sha256);
                if conflicting {
                    bail!("Manifest widersprüchlich: {} und {path} haben dieselbe SHA-1, aber verschiedene Daten", b.label);
                }
                if b.local_copy.is_none() {
                    b.local_copy = local_copy.cloned();
                }
                if b.sha256.is_none() {
                    b.sha256 = sha256.clone();
                }
            }
            None => {
                blobs.insert(
                    sha1.clone(),
                    Blob {
                        sha1: sha1.clone(),
                        sha256: sha256.clone(),
                        size,
                        url: resolve_url(base_url, url),
                        local_copy: local_copy.cloned(),
                        label: path.clone(),
                    },
                );
            }
        }
    }
    Ok(blobs)
}

struct Staged {
    downloaded: usize,
    reused: usize,
}

enum Source {
    Network,
    Local,
}

/// A worker gave up on retrying because another download already failed.
#[derive(Debug, thiserror::Error)]
#[error("abgebrochen")]
struct Aborted;

fn stage_all(
    instance: &Path,
    tmp_dir: &Path,
    blobs: &BTreeMap<String, Blob>,
    fetcher: &dyn Fetcher,
    opts: &ApplyOptions,
    reporter: &Reporter,
) -> Result<Staged> {
    let jobs: Vec<&Blob> = blobs.values().collect();
    let next = AtomicUsize::new(0);
    // Once a download failed for good, no new ones start — but transfers
    // already running finish, so a retry can reuse them.
    let stop = AtomicBool::new(false);
    let first_error: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let downloaded = AtomicUsize::new(0);
    let reused = AtomicUsize::new(0);
    let workers = opts.parallel.clamp(1, jobs.len().max(1));

    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                if stop.load(Ordering::SeqCst) || opts.cancelled() {
                    break;
                }
                let Some(blob) = jobs.get(next.fetch_add(1, Ordering::SeqCst)) else {
                    break;
                };
                match stage_blob(instance, tmp_dir, blob, fetcher, opts, reporter, &stop) {
                    Ok(Source::Network) => downloaded.fetch_add(1, Ordering::SeqCst),
                    Ok(Source::Local) => reused.fetch_add(1, Ordering::SeqCst),
                    Err(e) => {
                        // Record the error before signalling the others, so an
                        // "aborted" from them can never win the slot.
                        let mut slot = first_error.lock().unwrap_or_else(|p| p.into_inner());
                        if slot.is_none() && e.downcast_ref::<Aborted>().is_none() {
                            *slot = Some(e);
                        }
                        stop.store(true, Ordering::SeqCst);
                        break;
                    }
                };
                reporter.file_done(&blob.label);
            });
        }
    });

    if opts.cancelled() {
        return Err(Cancelled.into());
    }
    if let Some(e) = first_error.into_inner().unwrap_or_else(|p| p.into_inner()) {
        return Err(e);
    }
    if stop.load(Ordering::SeqCst) {
        return Err(anyhow!(Aborted));
    }
    Ok(Staged {
        downloaded: downloaded.into_inner(),
        reused: reused.into_inner(),
    })
}

fn stage_blob(
    instance: &Path,
    tmp_dir: &Path,
    blob: &Blob,
    fetcher: &dyn Fetcher,
    opts: &ApplyOptions,
    reporter: &Reporter,
    stop: &AtomicBool,
) -> Result<Source> {
    let dest = tmp_dir.join(&blob.sha1);
    let part = tmp_dir.join(format!("{}.part", blob.sha1));

    // Verified in an earlier, interrupted run?
    if dest.is_file() {
        if let Ok(f) = File::open(&dest) {
            match verify_stream(f, None, blob, reporter) {
                Ok(()) => return Ok(Source::Local),
                // Interrupted, not disproven: keep it for the next attempt.
                Err(e) if e.is::<Cancelled>() => return Err(e),
                Err(_) => {}
            }
        }
        let _ = fs::remove_file(&dest);
    }

    // An identical file already in the instance (e.g. a renamed copy)?
    if let Some(src) = &blob.local_copy {
        let copied = File::open(instance.join(src))
            .map_err(anyhow::Error::from)
            .and_then(|f| verify_stream(f, Some(&part), blob, reporter));
        match copied {
            Ok(()) => {
                fs::rename(&part, &dest).with_context(|| format!("{} ablegen", dest.display()))?;
                return Ok(Source::Local);
            }
            Err(e) => {
                let _ = fs::remove_file(&part);
                if e.is::<Cancelled>() {
                    return Err(e);
                }
            }
        }
    }

    let mut attempt = 0u32;
    loop {
        let res = fetcher
            .open(&blob.url)
            .and_then(|r| verify_stream(r, Some(&part), blob, reporter));
        match res {
            Ok(()) => {
                fs::rename(&part, &dest).with_context(|| format!("{} ablegen", dest.display()))?;
                return Ok(Source::Network);
            }
            Err(e) => {
                let _ = fs::remove_file(&part);
                if e.is::<Cancelled>() || opts.cancelled() {
                    return Err(Cancelled.into());
                }
                if attempt >= opts.retries || is_permanent(&e) {
                    return Err(e.context(format!("Download von {} fehlgeschlagen", blob.label)));
                }
                // Another file already failed for good: don't keep retrying.
                if stop.load(Ordering::SeqCst) {
                    return Err(anyhow!(Aborted));
                }
                std::thread::sleep(opts.retry_delay.saturating_mul(1 << attempt.min(16)));
                attempt += 1;
            }
        }
    }
}

/// Read `src` to the end, writing it to `out` (if given) while hashing, and
/// check size, SHA-1 and SHA-256. Progress bytes are reported as they arrive
/// and taken back if the attempt fails.
fn verify_stream(
    mut src: impl Read,
    out: Option<&Path>,
    blob: &Blob,
    reporter: &Reporter,
) -> Result<()> {
    let mut counted = 0u64;
    let res = (|| -> Result<()> {
        let mut file = match out {
            Some(p) => Some(File::create(p).with_context(|| format!("{} anlegen", p.display()))?),
            None => None,
        };
        let mut hasher = Hasher::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if reporter.cancelled() {
                return Err(Cancelled.into());
            }
            let n = src.read(&mut buf).context("Lesefehler")?;
            if n == 0 {
                break;
            }
            if hasher.len() + n as u64 > blob.size {
                return Err(BadDownload {
                    file: blob.label.clone(),
                    problem: format!("mehr Daten als erwartet ({} Bytes)", blob.size),
                }
                .into());
            }
            hasher.update(&buf[..n]);
            if let Some(f) = file.as_mut() {
                f.write_all(&buf[..n]).context("Schreibfehler")?;
            }
            counted += n as u64;
            reporter.bytes(n as u64, &blob.label);
        }
        if let Some(f) = file {
            f.sync_all().context("Schreibfehler")?;
        }
        let d = hasher.finish();
        if d.len != blob.size {
            return Err(BadDownload {
                file: blob.label.clone(),
                problem: format!("unvollständig ({} von {} Bytes)", d.len, blob.size),
            }
            .into());
        }
        if d.sha1 != blob.sha1 || blob.sha256.as_ref().is_some_and(|want| want != &d.sha256) {
            return Err(BadDownload {
                file: blob.label.clone(),
                problem: "Prüfsumme stimmt nicht – Datei beschädigt".into(),
            }
            .into());
        }
        Ok(())
    })();
    if res.is_err() {
        reporter.unbytes(counted);
    }
    res
}

/// Throttled, thread-safe progress reporting (and the cancel flag, which every
/// transfer checks between chunks).
struct Reporter<'a> {
    sink: &'a (dyn Fn(&Progress) + Sync),
    total_files: usize,
    total_bytes: u64,
    done_files: AtomicUsize,
    done_bytes: AtomicU64,
    last_emit: Mutex<Option<Instant>>,
    cancel: Option<&'a AtomicBool>,
}

impl<'a> Reporter<'a> {
    fn new(
        sink: &'a (dyn Fn(&Progress) + Sync),
        total_files: usize,
        total_bytes: u64,
        cancel: Option<&'a AtomicBool>,
    ) -> Self {
        Self {
            sink,
            total_files,
            total_bytes,
            done_files: AtomicUsize::new(0),
            done_bytes: AtomicU64::new(0),
            last_emit: Mutex::new(None),
            cancel,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.load(Ordering::SeqCst))
    }

    fn emit(&self, phase: &str, current: &str, force: bool) {
        let mut last = self.last_emit.lock().unwrap_or_else(|p| p.into_inner());
        if !force && last.is_some_and(|t| t.elapsed() < Duration::from_millis(100)) {
            return;
        }
        *last = Some(Instant::now());
        (self.sink)(&Progress {
            phase: phase.to_string(),
            current: current.to_string(),
            done_files: self.done_files.load(Ordering::SeqCst),
            total_files: self.total_files,
            done_bytes: self.done_bytes.load(Ordering::SeqCst).min(self.total_bytes),
            total_bytes: self.total_bytes,
        });
    }

    fn bytes(&self, n: u64, current: &str) {
        self.done_bytes.fetch_add(n, Ordering::SeqCst);
        self.emit("download", current, false);
    }

    fn unbytes(&self, n: u64) {
        self.done_bytes.fetch_sub(n, Ordering::SeqCst);
    }

    fn file_done(&self, current: &str) {
        self.done_files.fetch_add(1, Ordering::SeqCst);
        self.emit("download", current, true);
    }

    fn phase(&self, phase: &str) {
        self.emit(phase, "", true);
    }
}

// --- 3. pre-flight ------------------------------------------------------------

/// Fail early — before anything moved — if a file the update must move is
/// held open by another program.
fn preflight(instance: &Path, exec: &ExecutionPlan) -> Result<()> {
    let mut paths: Vec<&String> = exec.deletions.iter().collect();
    for d in &exec.downloads {
        paths.extend(d.replaces.iter());
        paths.push(&d.entry.path);
    }
    for rel in paths {
        let p = instance.join(rel);
        if p.exists() {
            ensure_movable(&p).with_context(|| format!("{rel} ist gesperrt"))?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn ensure_movable(p: &Path) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    for _ in 0..10 {
        match fs::OpenOptions::new().read(true).share_mode(0).open(p) {
            Ok(_) => return Ok(()),
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
                ) =>
            {
                // Virus scanners hold fresh files briefly; give them a moment.
                std::thread::sleep(Duration::from_millis(200));
            }
            // Other errors (e.g. no read permission) don't mean "in use";
            // the move itself will tell.
            Err(_) => return Ok(()),
        }
    }
    Err(FileInUse.into())
}

#[cfg(not(windows))]
fn ensure_movable(_p: &Path) -> Result<()> {
    // Unix renames work on open files; the journal covers real failures.
    Ok(())
}

// --- 4. commit ----------------------------------------------------------------

#[derive(Default)]
struct Committed {
    installed: Vec<String>,
    backed_up: Vec<BackedUp>,
    deleted: usize,
    replaced: usize,
    seeded: usize,
}

fn commit(
    instance: &Path,
    exec: &ExecutionPlan,
    tmp_dir: &Path,
    backup_root: &Path,
    journal: &mut Journal,
) -> Result<Committed> {
    let mut c = Committed::default();
    // How many targets still need each blob: the last one gets the rename,
    // earlier ones a copy (several manifest entries may share one content).
    let mut uses: BTreeMap<&str, usize> = BTreeMap::new();
    for sha1 in exec
        .downloads
        .iter()
        .map(|d| d.entry.sha1.as_str())
        .chain(exec.seeds.iter().map(|s| s.sha1.as_str()))
    {
        *uses.entry(sha1).or_default() += 1;
    }

    for rel in &exec.deletions {
        if backup(instance, backup_root, rel, journal, &mut c)? {
            c.deleted += 1;
        }
    }
    for d in &exec.downloads {
        if let Some(r) = &d.replaces {
            if backup(instance, backup_root, r, journal, &mut c)? {
                c.replaced += 1;
            }
        }
        // Anything still occupying the target (e.g. an unmanaged file).
        backup(instance, backup_root, &d.entry.path, journal, &mut c)?;
        install(
            tmp_dir,
            &d.entry.sha1,
            &mut uses,
            &instance.join(&d.entry.path),
            journal,
        )?;
        c.installed.push(d.entry.path.clone());
    }
    for s in &exec.seeds {
        let dst = instance.join(&s.path);
        if dst.exists() {
            continue; // never overwrite the player's own file
        }
        install(tmp_dir, &s.sha1, &mut uses, &dst, journal)?;
        c.installed.push(s.path.clone());
        c.seeded += 1;
    }
    Ok(c)
}

/// Move `instance/rel` into the backup (never overwriting an earlier backup).
/// Returns whether a file was there to move.
fn backup(
    instance: &Path,
    backup_root: &Path,
    rel: &str,
    journal: &mut Journal,
    c: &mut Committed,
) -> Result<bool> {
    let src = instance.join(rel);
    if fs::symlink_metadata(&src).is_err() {
        return Ok(false);
    }
    let dst = unique_path(&backup_root.join(rel));
    journal
        .move_file(&src, &dst)
        .with_context(|| format!("{rel} sichern"))?;
    let backup_rel = dst
        .strip_prefix(backup_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| rel.to_string());
    c.backed_up.push(BackedUp {
        original: rel.to_string(),
        backup: backup_rel,
    });
    Ok(true)
}

fn install(
    tmp_dir: &Path,
    sha1: &str,
    uses: &mut BTreeMap<&str, usize>,
    dst: &Path,
    journal: &mut Journal,
) -> Result<()> {
    let staged = tmp_dir.join(sha1);
    let left = uses.get_mut(sha1).map(|n| {
        *n -= 1;
        *n
    });
    let res = if left == Some(0) {
        journal.move_file(&staged, dst)
    } else {
        journal.copy_file(&staged, dst)
    };
    res.with_context(|| format!("{} installieren", dst.display()))
}

/// Every filesystem change of a commit, so it can be undone on failure.
#[derive(Default)]
struct Journal {
    steps: Vec<Step>,
}

enum Step {
    Moved { from: PathBuf, to: PathBuf },
    Created(PathBuf),
}

impl Journal {
    fn move_file(&mut self, from: &Path, to: &Path) -> Result<()> {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        move_with_retry(from, to)?;
        self.steps.push(Step::Moved {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
        Ok(())
    }

    fn copy_file(&mut self, from: &Path, to: &Path) -> Result<()> {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = sibling(to, ".bonegrader-new");
        fs::copy(from, &tmp)?;
        if let Err(e) = move_with_retry(&tmp, to) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        self.steps.push(Step::Created(to.to_path_buf()));
        Ok(())
    }

    /// Undo every step, newest first. Returns what could not be undone.
    fn rollback(&mut self) -> Vec<String> {
        let mut problems = Vec::new();
        while let Some(step) = self.steps.pop() {
            let res = match &step {
                Step::Moved { from, to } => move_with_retry(to, from),
                Step::Created(path) => fs::remove_file(path),
            };
            if let Err(e) = res {
                let what = match step {
                    Step::Moved { from, to } => format!("{} → {}", to.display(), from.display()),
                    Step::Created(path) => path.display().to_string(),
                };
                problems.push(format!("{what}: {e}"));
            }
        }
        problems
    }
}

/// Rename, retrying briefly on Windows where virus scanners and indexers hold
/// fresh files for a moment; falls back to copy + delete across filesystems.
fn move_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    let attempts = if cfg!(windows) { 8 } else { 1 };
    let mut delay = Duration::from_millis(50);
    let mut attempt = 1;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                fs::copy(from, to)?;
                if let Err(e) = fs::remove_file(from) {
                    let _ = fs::remove_file(to);
                    return Err(e);
                }
                return Ok(());
            }
            Err(e) if attempt < attempts && is_transient_lock(&e) => {
                std::thread::sleep(delay);
                delay *= 2;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Access denied (5), sharing violation (32), lock violation (33): on Windows
/// usually a scanner or indexer holding the file for a moment.
fn is_transient_lock(e: &io::Error) -> bool {
    cfg!(windows) && matches!(e.raw_os_error(), Some(5 | 32 | 33))
}

/// `p`, or `name (2).ext`, `name (3).ext`, … if it already exists.
fn unique_path(p: &Path) -> PathBuf {
    if fs::symlink_metadata(p).is_err() {
        return p.to_path_buf();
    }
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|n| p.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|c| fs::symlink_metadata(c).is_err())
        .expect("unbounded")
}

/// `base/name`, or `base/name-2`, … if taken.
fn unique_dir(base: &Path, name: &str) -> PathBuf {
    let name = if is_safe_component(name) {
        name
    } else {
        "update"
    };
    let first = base.join(name);
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| base.join(format!("{name}-{n}")))
        .find(|c| !c.exists())
        .expect("unbounded")
}

fn sibling(p: &Path, suffix: &str) -> PathBuf {
    let mut name = p.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    p.with_file_name(name)
}

/// Remove `dir` and its subfolders if they contain no files.
fn remove_empty_dirs(dir: &Path) -> io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let p = entry?.path();
        if p.is_dir() {
            remove_empty_dirs(&p)?;
        }
    }
    fs::remove_dir(dir)
}

// --- 5. state, records, retention ---------------------------------------------

fn derive_state(instance: &Path, manifest: &Manifest, old: &ClientState) -> ClientState {
    let mut managed = BTreeMap::new();
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
    let mut seeded = old.seeded.clone();
    seeded.extend(
        manifest
            .seeds
            .iter()
            .filter(|s| instance.join(&s.path).exists())
            .map(|s| s.path.clone()),
    );
    ClientState {
        instance_path: old.instance_path.clone(),
        launcher_type: old.launcher_type.clone(),
        channel: manifest.channel.clone(),
        pack_name: Some(manifest.pack_name.clone()),
        last_manifest_generated_at: Some(manifest.generated_at.clone()),
        managed,
        seeded,
        server_added: old.server_added.clone(),
    }
}

fn write_record(dir: &Path, record: &UpdateRecord) -> Result<()> {
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{RECORD_FILE}.tmp"));
    fs::write(&tmp, serde_json::to_vec_pretty(record)?)?;
    fs::rename(&tmp, dir.join(RECORD_FILE))?;
    Ok(())
}

fn read_record(dir: &Path) -> Option<UpdateRecord> {
    serde_json::from_slice(&fs::read(dir.join(RECORD_FILE)).ok()?).ok()
}

/// Backup folders, oldest first (timestamped names sort chronologically).
fn backup_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(root)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

/// Delete all but the newest `keep` backup folders (best effort). `current`
/// is never deleted, even if a wrong system clock makes it sort as old.
pub fn prune_backups(root: &Path, keep: usize, current: &Path) {
    let dirs = backup_dirs(root);
    let excess = dirs.len().saturating_sub(keep.max(1));
    for d in dirs.into_iter().take(excess) {
        if d != current {
            let _ = fs::remove_dir_all(d);
        }
    }
}

// --- undo -----------------------------------------------------------------------

/// The newest update that can still be undone. Walks backwards over updates
/// already undone; stops at a folder without a readable record (e.g. from an
/// older Bonegrader), since nothing before it can be undone consistently.
pub fn last_restorable(instance: &Path) -> Option<(PathBuf, UpdateRecord)> {
    for dir in backup_dirs(&instance.join(BACKUP_DIR)).into_iter().rev() {
        let record = read_record(&dir)?;
        if record.undone_at.is_none() {
            return Some((dir, record));
        }
    }
    None
}

/// What [`restore_last`] did.
#[derive(Debug)]
pub struct RestoreReport {
    /// Files moved back from the backup.
    pub restored: usize,
    /// Files of the undone update that were taken out of the instance.
    pub removed: usize,
    /// State to persist (the one from before the undone update).
    pub previous_state: ClientState,
    pub backup_dir: PathBuf,
}

/// Undo the newest update that is not undone yet: take out the files it
/// installed (kept in `<backup>/undone/`) and put the backed-up files back.
/// Journalled like an update — on failure nothing changes.
pub fn restore_last(instance: &Path, timestamp: &str) -> Result<RestoreReport> {
    let (dir, mut record) = last_restorable(instance)
        .context("Es gibt kein Update, das rückgängig gemacht werden kann.")?;

    let installed_ok = record
        .installed
        .iter()
        .all(|p| is_safe_managed_path(p) || is_safe_seed_path(p));
    let backups_ok = record.backed_up.iter().all(|b| {
        is_safe_managed_path(&b.original)
            && b.backup.split('/').all(is_safe_component)
            && !b.backup.starts_with(UNDONE_DIR)
    });
    if !installed_ok || !backups_ok {
        bail!("{} ist beschädigt", dir.join(RECORD_FILE).display());
    }

    let undone = dir.join(UNDONE_DIR);
    for rel in record
        .installed
        .iter()
        .chain(record.backed_up.iter().map(|b| &b.original))
    {
        let p = instance.join(rel);
        if p.exists() {
            ensure_movable(&p).with_context(|| format!("{rel} ist gesperrt"))?;
        }
    }

    let mut journal = Journal::default();
    let res = (|| -> Result<(usize, usize)> {
        let mut removed = 0;
        for rel in &record.installed {
            let p = instance.join(rel);
            if p.exists() {
                journal.move_file(&p, &unique_path(&undone.join(rel)))?;
                removed += 1;
            }
        }
        let mut restored = 0;
        for b in &record.backed_up {
            let src = dir.join(&b.backup);
            if !src.exists() {
                continue;
            }
            let dst = instance.join(&b.original);
            if dst.exists() {
                journal.move_file(&dst, &unique_path(&undone.join(&b.original)))?;
            }
            journal.move_file(&src, &dst)?;
            restored += 1;
        }
        Ok((restored, removed))
    })();
    let (restored, removed) = match res {
        Ok(counts) => counts,
        Err(e) => {
            let problems = journal.rollback();
            if problems.is_empty() {
                return Err(
                    e.context("Rückgängig machen fehlgeschlagen – es wurde nichts verändert")
                );
            }
            return Err(e.context(format!(
                "Rückgängig machen fehlgeschlagen, Rücknahme unvollständig: {}",
                problems.join("; ")
            )));
        }
    };

    record.undone_at = Some(timestamp.to_string());
    write_record(&dir, &record)?;
    Ok(RestoreReport {
        restored,
        removed,
        previous_state: record.previous_state,
        backup_dir: dir,
    })
}
