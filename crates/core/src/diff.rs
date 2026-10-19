//! The update planner — pure and deterministic.
//!
//! [`compute_plan`] takes the server manifest, a scan of the local instance and
//! the previous client state, and returns an [`UpdatePlan`] describing what
//! *should* happen. It performs no I/O and mutates nothing on disk. Applying the
//! plan (with the user's answers to the extras/collision prompts) is the
//! caller's responsibility and must honour the invariants below.
//!
//! Mods the player switched off (`mods/x.jar.disabled`, the launchers'
//! convention) are matched by their switched-on name (`mods/x.jar`, see
//! [`LocalFile::enabled_path`]), which is also what the client state records.
//! An update never changes whether a mod is on or off: a new version of a
//! switched-off mod is installed switched off.

use crate::manifest::{Category, FileEntry, Manifest};
use crate::paths::disabled_path;
use crate::scan::LocalFile;
use crate::state::ClientState;
use std::collections::{BTreeMap, BTreeSet};

/// A file to fetch from the server, optionally replacing an existing local file
/// (a same-path content change, or an old version of the same `modId`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    pub entry: FileEntry,
    /// Existing client-relative path this download supersedes, if any. Applying
    /// the download must remove this old file (after the new one is verified).
    pub replaces: Option<String>,
    /// A local file with exactly this content (same SHA-1), e.g. a renamed
    /// copy. The applier may copy it instead of downloading — it must still
    /// verify the bytes like any download.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_copy: Option<String>,
    /// Install switched off (at [`Download::target`]): the player had switched
    /// off every copy of this mod.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

impl Download {
    /// Where the file goes: the manifest path, or its `.disabled` name.
    pub fn target(&self) -> String {
        if self.disabled {
            disabled_path(&self.entry.path)
        } else {
            self.entry.path.clone()
        }
    }
}

/// An unmanaged local mod that shares a `modId` with a mod the pack manages.
/// Loading both would crash the game, so the user is asked to let Bonegrader
/// replace their copy. If they decline, the paired manifest download keyed by
/// `manifest_path` must be skipped too.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Collision {
    pub local_path: String,
    pub mod_id: String,
    pub manifest_path: String,
}

/// The proposed set of actions.
///
/// # Safety invariants for the applier
/// * Only ever delete paths listed in `removals`, `duplicates`,
///   `stale_disabled`, resolved `collisions`, chosen `user_extras`, or a
///   download's `replaces` — never by scanning a folder.
/// * Every such path is already known to be inside a managed folder; still,
///   re-validate with [`crate::paths::is_safe_managed_path`] before deleting.
/// * Download everything to a temp location and verify it *before* deleting
///   or moving anything; apply as one batch; move replaced/removed files to a
///   backup dir rather than hard-deleting.
///
/// All paths are real file names (`mods/x.jar.disabled` for a switched-off
/// mod), except in `adopted`, which lists switched-on names like the state.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePlan {
    pub downloads: Vec<Download>,
    /// Managed files no longer present in the manifest (safe to remove).
    pub removals: Vec<String>,
    /// Unmanaged mods that are byte-identical copies of a pack file under
    /// another name (e.g. `jei (1).jar` from a double download). Keeping them
    /// would load the mod twice and crash the game; removing them loses
    /// nothing (they are backed up like every removal).
    pub duplicates: Vec<String>,
    /// Switched-off leftovers of pack mods: an old or identical copy of a
    /// pack mod, or a `.disabled` twin of a pack file that is switched on
    /// (older Bonegrader versions re-installed mods the player had switched
    /// off and left these behind). The game never loads them, so removing
    /// them changes nothing in game; they are backed up like every removal.
    pub stale_disabled: Vec<String>,
    /// Unmanaged local *mods* not in the manifest (ask keep/remove), and the
    /// player's own copy of a pack mod whose pack copy they switched off.
    /// Packs are never listed here — the player's own resource/shader packs
    /// are kept silently. Switched-off mods neither: they are never loaded.
    pub user_extras: Vec<String>,
    /// Duplicate-`modId` clashes needing a user decision.
    pub collisions: Vec<Collision>,
    /// Local files newly taken under management this run (first-run adoption
    /// or an exact match at a manifest path). Informational.
    pub adopted: Vec<String>,
}

impl UpdatePlan {
    /// True if applying the plan would change anything on disk.
    pub fn is_noop(&self) -> bool {
        self.downloads.is_empty()
            && self.removals.is_empty()
            && self.duplicates.is_empty()
            && self.stale_disabled.is_empty()
            && self.collisions.is_empty()
    }
}

/// Compute the update plan. Deterministic: the result depends only on the
/// inputs (never on hash-map iteration order) and all vectors are sorted.
pub fn compute_plan(manifest: &Manifest, local: &[LocalFile], state: &ClientState) -> UpdatePlan {
    // The file standing for each switched-on path. A switched-off twin
    // (`x.jar.disabled` next to `x.jar`) only stands in while the switched-on
    // file is missing; otherwise it is a leftover.
    let mut local_by_path: BTreeMap<&str, &LocalFile> = BTreeMap::new();
    for l in local {
        local_by_path
            .entry(l.enabled_path())
            .and_modify(|cur| {
                if (l.disabled, &l.path) < (cur.disabled, &cur.path) {
                    *cur = l;
                }
            })
            .or_insert(l);
    }
    let stands_for_path = |l: &LocalFile| {
        local_by_path
            .get(l.enabled_path())
            .is_some_and(|c| c.path == l.path)
    };
    // Any local file with a given content; the smallest path wins so the
    // choice is stable.
    let mut local_by_sha: BTreeMap<&str, &str> = BTreeMap::new();
    for l in local {
        local_by_sha
            .entry(l.sha1.as_str())
            .and_modify(|p| *p = (*p).min(l.path.as_str()))
            .or_insert(l.path.as_str());
    }

    let mut entries: Vec<&FileEntry> = manifest.files.iter().collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    let manifest_paths: BTreeSet<&str> = entries.iter().map(|f| f.path.as_str()).collect();
    let manifest_mod_ids: BTreeSet<&str> = manifest
        .mods()
        .filter_map(|f| f.mod_id.as_deref())
        .collect();
    let manifest_shas: BTreeSet<&str> = entries.iter().map(|f| f.sha1.as_str()).collect();

    let original_managed: BTreeSet<String> = state.managed.keys().cloned().collect();
    let mut managed: BTreeSet<String> = original_managed.clone();

    // --- 1. First-run adoption -------------------------------------------------
    // Seed the managed set from files that clearly belong to the pack, so an
    // existing manual install isn't mistaken for a pile of "extra" mods.
    if state.is_first_run() {
        for lf in local {
            let by_sha = manifest_shas.contains(lf.sha1.as_str());
            let by_mod = lf.category == Category::Mod
                && lf
                    .mod_ids
                    .iter()
                    .any(|id| manifest_mod_ids.contains(id.as_str()));
            if by_sha || by_mod {
                managed.insert(lf.enabled_path().to_string());
            }
        }
    }

    // A local file the pack owns: managed, or sitting at a manifest path.
    let owned = |l: &LocalFile| {
        stands_for_path(l)
            && (managed.contains(l.enabled_path()) || manifest_paths.contains(l.enabled_path()))
    };

    // modId -> managed file an incoming version may supersede. Files sitting at
    // a manifest path are excluded: they are owned by their own manifest entry
    // and must never be "replaced" by another one (e.g. when a jar declaring
    // two mods is split into two jars). Sorted iteration keeps the choice
    // stable when several old copies share a modId.
    let mod_ids_for = |path: &str| -> Vec<String> {
        if let Some(lf) = local_by_path.get(path) {
            return lf.mod_ids.clone();
        }
        state
            .managed
            .get(path)
            .map(|m| m.mod_ids.clone())
            .unwrap_or_default()
    };
    let mut supersede: BTreeMap<String, String> = BTreeMap::new();
    for p in &managed {
        if manifest_paths.contains(p.as_str()) {
            continue;
        }
        for id in mod_ids_for(p) {
            supersede.entry(id).or_insert_with(|| p.clone());
        }
    }

    // --- 2. On or off ----------------------------------------------------------
    // A pack mod goes in switched off only if the player switched off every
    // copy of it they had: the file at its path, the pack's older versions
    // (same modId) and identical copies. Whether *which* old copy gets
    // replaced decides nothing.
    let all_copies_off = |e: &FileEntry| -> bool {
        if e.category != Category::Mod {
            return false;
        }
        let mut copies = local
            .iter()
            .filter(|l| {
                let same_mod = e
                    .mod_id
                    .as_deref()
                    .is_some_and(|m| l.mod_ids.iter().any(|id| id == m));
                // The file at the path counts if it is this mod at all (a
                // library has nothing but its path to go by).
                let at_path = stands_for_path(l)
                    && l.enabled_path() == e.path
                    && (e.mod_id.is_none() || same_mod || l.sha1 == e.sha1);
                at_path || (same_mod && owned(l)) || (!l.disabled && l.sha1 == e.sha1)
            })
            .peekable();
        copies.peek().is_some() && copies.all(|l| l.disabled)
    };
    let off_before: BTreeMap<&str, bool> = entries
        .iter()
        .map(|e| (e.path.as_str(), all_copies_off(e)))
        .collect();

    // --- 3. Unmanaged local mods: duplicates, collisions, extras, leftovers ---
    let mut plan = UpdatePlan::default();
    // Every pack modId a colliding copy provides (a jar may declare several).
    let mut clashing: BTreeSet<&str> = BTreeSet::new();
    // How many loaded jars declare each modId.
    let mut active: BTreeMap<&str, usize> = BTreeMap::new();
    for l in local.iter().filter(|l| !l.disabled) {
        for id in &l.mod_ids {
            *active.entry(id.as_str()).or_default() += 1;
        }
    }
    for lf in local {
        if lf.category != Category::Mod || owned(lf) {
            continue;
        }
        let pack_ids: Vec<&String> = lf
            .mod_ids
            .iter()
            .filter(|id| manifest_mod_ids.contains(id.as_str()))
            .collect();
        if lf.disabled {
            // Never loaded, so never a crash risk: only pack leftovers go.
            let twin = local_by_path
                .get(lf.enabled_path())
                .is_some_and(|c| owned(c));
            if twin || manifest_shas.contains(lf.sha1.as_str()) || !pack_ids.is_empty() {
                plan.stale_disabled.push(lf.path.clone());
            }
            continue;
        }
        // Byte-identical to a pack file but under another name: the pack copy
        // is (or will be) installed at its own path, so this one is redundant.
        if manifest_shas.contains(lf.sha1.as_str()) {
            plan.duplicates.push(lf.path.clone());
            continue;
        }
        // Same modId as a pack mod that stays on: loading both would crash.
        // If the player switched off every pack copy, their (single) copy
        // replaces it.
        let clash = pack_ids.iter().find_map(|mid| {
            entries
                .iter()
                .find(|e| e.category == Category::Mod && e.mod_id.as_deref() == Some(mid.as_str()))
                .filter(|e| !off_before[e.path.as_str()] || active[mid.as_str()] > 1)
                .map(|e| (*mid, e.path.clone()))
        });
        match clash {
            Some((mid, manifest_path)) => {
                clashing.extend(pack_ids.iter().map(|id| id.as_str()));
                plan.collisions.push(Collision {
                    local_path: lf.path.clone(),
                    mod_id: mid.clone(),
                    manifest_path,
                });
            }
            None => plan.user_extras.push(lf.path.clone()),
        }
    }
    // A colliding copy is replaced by the pack's (by default), so the pack
    // copies of its mods must be on — they were on before.
    let goes_in_off = |e: &FileEntry| {
        off_before[e.path.as_str()] && e.mod_id.as_deref().is_none_or(|m| !clashing.contains(m))
    };

    // --- 4. Downloads (walk the manifest) -------------------------------------
    let mut replaced: BTreeSet<String> = BTreeSet::new();
    let local_copy_for = |e: &FileEntry| local_by_sha.get(e.sha1.as_str()).map(|p| p.to_string());
    for e in &entries {
        let off = goes_in_off(e);
        if let Some(lf) = local_by_path.get(e.path.as_str()) {
            if lf.sha1 == e.sha1 && lf.disabled == off {
                managed.insert(e.path.clone()); // up to date; ensure managed
                continue;
            }
            // Same path, changed content — or the same content to switch back
            // on because another active copy of the mod goes away.
            plan.downloads.push(Download {
                entry: (*e).clone(),
                replaces: Some(lf.path.clone()),
                local_copy: local_copy_for(e),
                disabled: off,
            });
            replaced.insert(e.path.clone());
            continue;
        }

        // No file at the target path. For mods, see if a managed file with the
        // same modId is an older version to be superseded (each old file is
        // claimed by at most one download).
        let old = (e.category == Category::Mod)
            .then(|| e.mod_id.as_deref().and_then(|mid| supersede.get(mid)))
            .flatten()
            .filter(|old| !replaced.contains(*old))
            .cloned();
        if let Some(old) = &old {
            replaced.insert(old.clone());
        }
        plan.downloads.push(Download {
            entry: (*e).clone(),
            replaces: old.map(|o| real_path(&local_by_path, o)),
            local_copy: local_copy_for(e),
            disabled: off,
        });
    }

    // --- 5. Removals (managed files gone from the manifest) -------------------
    for p in &managed {
        if replaced.contains(p) || manifest_paths.contains(p.as_str()) {
            continue;
        }
        plan.removals.push(real_path(&local_by_path, p.clone()));
    }

    // --- 6. Finalise ----------------------------------------------------------
    plan.adopted = managed.difference(&original_managed).cloned().collect();
    plan.removals.sort();
    plan.duplicates.sort();
    plan.stale_disabled.sort();
    plan.user_extras.sort();
    plan.collisions
        .sort_by(|a, b| a.local_path.cmp(&b.local_path));
    plan
}

/// The real file name behind a switched-on path (`mods/x.jar.disabled` while
/// switched off); the path itself if nothing is there.
fn real_path(local_by_path: &BTreeMap<&str, &LocalFile>, path: String) -> String {
    match local_by_path.get(path.as_str()) {
        Some(l) => l.path.clone(),
        None => path,
    }
}

/// How well an instance matches the pack — to catch updating the wrong
/// instance (another modpack) before anything is replaced.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceAssessment {
    pub first_run: bool,
    /// Jars in `mods/` (switched-off ones included).
    pub local_mods: usize,
    /// Of those, how many belong to the pack (same content or same modId).
    pub pack_mods: usize,
    /// The pack this instance was last updated to, if it is a different one.
    pub previous_pack: Option<String>,
    /// Probably the wrong instance: it belongs to another pack, or a first run
    /// meets a folder full of mods that mostly don't belong to this pack.
    pub suspicious: bool,
}

/// Mods already in a folder before the first run; below this, a mismatch is
/// not worth a warning.
const SUSPICIOUS_MIN_MODS: usize = 10;

pub fn assess_instance(
    manifest: &Manifest,
    local: &[LocalFile],
    state: &ClientState,
) -> InstanceAssessment {
    let shas: BTreeSet<&str> = manifest.files.iter().map(|f| f.sha1.as_str()).collect();
    let ids: BTreeSet<&str> = manifest
        .mods()
        .filter_map(|f| f.mod_id.as_deref())
        .collect();
    let mods: Vec<&LocalFile> = local
        .iter()
        .filter(|l| l.category == Category::Mod)
        .collect();
    let pack_mods = mods
        .iter()
        .filter(|l| {
            shas.contains(l.sha1.as_str()) || l.mod_ids.iter().any(|i| ids.contains(i.as_str()))
        })
        .count();
    let first_run = state.is_first_run();
    let previous_pack = state.pack_name.clone().filter(|p| p != &manifest.pack_name);
    let suspicious = previous_pack.is_some()
        || (first_run && mods.len() >= SUSPICIOUS_MIN_MODS && pack_mods * 2 < mods.len());
    InstanceAssessment {
        first_run,
        local_mods: mods.len(),
        pack_mods,
        previous_pack,
        suspicious,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Category, FileEntry, Loader, Manifest};
    use crate::state::{ClientState, ManagedEntry};
    use std::collections::BTreeMap;

    fn fe(cat: Category, path: &str, sha1: &str, mod_id: Option<&str>) -> FileEntry {
        FileEntry {
            category: cat,
            path: path.into(),
            file_name: path.rsplit('/').next().unwrap().into(),
            size: 1,
            sha1: sha1.into(),
            sha256: None,
            mod_id: mod_id.map(str::to_string),
            mod_version: None,
            mod_name: None,
            required: None,
            url: format!("files/by-hash/{sha1}"),
        }
    }

    fn lf(cat: Category, path: &str, sha1: &str, mod_ids: &[&str]) -> LocalFile {
        LocalFile {
            category: cat,
            path: path.into(),
            file_name: path.rsplit('/').next().unwrap().into(),
            size: 1,
            sha1: sha1.into(),
            mod_ids: mod_ids.iter().map(|s| s.to_string()).collect(),
            mod_version: None,
            mod_name: None,
            dependencies: vec![],
            provides: vec![],
            disabled: false,
        }
    }

    /// A switched-off mod: `path` is the switched-on name.
    fn lf_off(path: &str, sha1: &str, mod_ids: &[&str]) -> LocalFile {
        let mut l = lf(Category::Mod, &format!("{path}.disabled"), sha1, mod_ids);
        l.disabled = true;
        l
    }

    fn manifest(files: Vec<FileEntry>) -> Manifest {
        Manifest {
            schema_version: 1,
            pack_name: "BonesAndBees".into(),
            channel: "main".into(),
            generated_at: "t".into(),
            loader: Loader {
                loader_type: "neoforge".into(),
                mc_version: "1.21.1".into(),
                loader_version: "21.1.222".into(),
            },
            files,
            client: None,
            server: None,
            seeds: vec![],
        }
    }

    /// Non-first-run state: mark the given (path, sha1, modIds) as managed.
    fn state(managed: &[(&str, &str, &[&str])]) -> ClientState {
        let mut m = BTreeMap::new();
        for (path, sha1, ids) in managed {
            m.insert(
                (*path).to_string(),
                ManagedEntry {
                    sha1: (*sha1).to_string(),
                    mod_ids: ids.iter().map(|s| s.to_string()).collect(),
                },
            );
        }
        ClientState {
            instance_path: "/x".into(),
            launcher_type: "curseforge".into(),
            channel: "main".into(),
            last_manifest_generated_at: Some("t0".into()),
            managed: m,
            ..Default::default()
        }
    }

    #[test]
    fn nothing_to_do_when_up_to_date() {
        let man = manifest(vec![fe(Category::Mod, "mods/a.jar", "s1", Some("a"))]);
        let local = vec![lf(Category::Mod, "mods/a.jar", "s1", &["a"])];
        let st = state(&[("mods/a.jar", "s1", &["a"])]);
        let plan = compute_plan(&man, &local, &st);
        assert!(plan.is_noop(), "{plan:?}");
        assert!(plan.user_extras.is_empty());
    }

    #[test]
    fn downloads_a_brand_new_mod() {
        let man = manifest(vec![
            fe(Category::Mod, "mods/a.jar", "s1", Some("a")),
            fe(Category::Mod, "mods/b.jar", "s2", Some("b")),
        ]);
        let local = vec![lf(Category::Mod, "mods/a.jar", "s1", &["a"])];
        let st = state(&[("mods/a.jar", "s1", &["a"])]);
        let plan = compute_plan(&man, &local, &st);
        assert_eq!(plan.downloads.len(), 1);
        assert_eq!(plan.downloads[0].entry.path, "mods/b.jar");
        assert_eq!(plan.downloads[0].replaces, None);
        assert_eq!(plan.downloads[0].local_copy, None);
        assert!(plan.removals.is_empty());
    }

    #[test]
    fn same_path_content_change_is_a_replace() {
        let man = manifest(vec![fe(Category::Mod, "mods/a.jar", "NEW", Some("a"))]);
        let local = vec![lf(Category::Mod, "mods/a.jar", "OLD", &["a"])];
        let st = state(&[("mods/a.jar", "OLD", &["a"])]);
        let plan = compute_plan(&man, &local, &st);
        assert_eq!(plan.downloads.len(), 1);
        assert_eq!(plan.downloads[0].replaces.as_deref(), Some("mods/a.jar"));
        assert!(
            plan.removals.is_empty(),
            "replace must not double as removal"
        );
    }

    #[test]
    fn version_bump_replaces_old_jar_by_modid() {
        // create 6.0.10 -> 6.0.11: different file name, same modId.
        let man = manifest(vec![fe(
            Category::Mod,
            "mods/create-6.0.11.jar",
            "s11",
            Some("create"),
        )]);
        let local = vec![lf(
            Category::Mod,
            "mods/create-6.0.10.jar",
            "s10",
            &["create"],
        )];
        let st = state(&[("mods/create-6.0.10.jar", "s10", &["create"])]);
        let plan = compute_plan(&man, &local, &st);
        assert_eq!(plan.downloads.len(), 1);
        assert_eq!(plan.downloads[0].entry.path, "mods/create-6.0.11.jar");
        assert_eq!(
            plan.downloads[0].replaces.as_deref(),
            Some("mods/create-6.0.10.jar")
        );
        assert!(plan.removals.is_empty());
        assert!(plan.user_extras.is_empty());
    }

    #[test]
    fn managed_mod_removed_server_side_is_removed() {
        let man = manifest(vec![fe(Category::Mod, "mods/a.jar", "s1", Some("a"))]);
        let local = vec![
            lf(Category::Mod, "mods/a.jar", "s1", &["a"]),
            lf(Category::Mod, "mods/old.jar", "s9", &["old"]),
        ];
        let st = state(&[
            ("mods/a.jar", "s1", &["a"]),
            ("mods/old.jar", "s9", &["old"]),
        ]);
        let plan = compute_plan(&man, &local, &st);
        assert_eq!(plan.removals, vec!["mods/old.jar".to_string()]);
        assert!(plan.downloads.is_empty());
        assert!(plan.user_extras.is_empty());
    }

    #[test]
    fn user_mod_is_kept_and_offered_in_extras() {
        let man = manifest(vec![fe(Category::Mod, "mods/a.jar", "s1", Some("a"))]);
        let local = vec![
            lf(Category::Mod, "mods/a.jar", "s1", &["a"]),
            lf(
                Category::Mod,
                "mods/myclientmod.jar",
                "sX",
                &["myclientmod"],
            ),
        ];
        let st = state(&[("mods/a.jar", "s1", &["a"])]);
        let plan = compute_plan(&man, &local, &st);
        assert_eq!(plan.user_extras, vec!["mods/myclientmod.jar".to_string()]);
        assert!(
            plan.removals.is_empty(),
            "user mod must never be auto-removed"
        );
        assert!(plan.collisions.is_empty());
    }

    #[test]
    fn duplicate_modid_is_reported_as_collision() {
        // Player manually added their own (older) JEI; the pack manages JEI too.
        let man = manifest(vec![fe(
            Category::Mod,
            "mods/jei-19.5.jar",
            "s195",
            Some("jei"),
        )]);
        let local = vec![lf(Category::Mod, "mods/jei-19.0.jar", "s190", &["jei"])];
        // Not a first run, so the local jei stays unmanaged and collides.
        let st = state(&[("mods/placeholder.jar", "z", &["placeholder"])]);
        let plan = compute_plan(&man, &local, &st);
        assert_eq!(plan.collisions.len(), 1, "{plan:?}");
        assert_eq!(plan.collisions[0].local_path, "mods/jei-19.0.jar");
        assert_eq!(plan.collisions[0].mod_id, "jei");
        assert_eq!(plan.collisions[0].manifest_path, "mods/jei-19.5.jar");
        // The pack version is still scheduled to download.
        assert!(plan
            .downloads
            .iter()
            .any(|d| d.entry.path == "mods/jei-19.5.jar"));
        // And the colliding user mod is NOT silently in extras.
        assert!(plan.user_extras.is_empty());
    }

    #[test]
    fn first_run_adopts_matching_files_and_handles_version_bump() {
        // Fresh state; player already has the pack installed manually, plus one
        // outdated mod and one genuinely personal mod.
        let man = manifest(vec![
            fe(Category::Mod, "mods/a.jar", "s1", Some("a")),
            fe(
                Category::Mod,
                "mods/create-6.0.11.jar",
                "s11",
                Some("create"),
            ),
        ]);
        let local = vec![
            lf(Category::Mod, "mods/a.jar", "s1", &["a"]), // exact match -> adopt
            lf(Category::Mod, "mods/create-6.0.10.jar", "s10", &["create"]), // old ver -> adopt+bump
            lf(Category::Mod, "mods/personal.jar", "sP", &["personal"]),     // extra
        ];
        let st = ClientState::default(); // is_first_run == true
        let plan = compute_plan(&man, &local, &st);

        assert!(plan.adopted.contains(&"mods/a.jar".to_string()));
        assert!(plan.adopted.contains(&"mods/create-6.0.10.jar".to_string()));
        // Version bump detected via adopted create.
        let bump = plan
            .downloads
            .iter()
            .find(|d| d.entry.path == "mods/create-6.0.11.jar")
            .expect("create bump download");
        assert_eq!(bump.replaces.as_deref(), Some("mods/create-6.0.10.jar"));
        // Personal mod is preserved and offered as an extra.
        assert_eq!(plan.user_extras, vec!["mods/personal.jar".to_string()]);
        assert!(!plan.adopted.contains(&"mods/personal.jar".to_string()));
    }

    #[test]
    fn changed_file_at_manifest_path_is_updated_not_an_extra() {
        // First run, a metadata-less jar (no modId) sitting where a pack file
        // belongs but with different content: must be updated, not flagged extra.
        let man = manifest(vec![fe(Category::Mod, "mods/lib.jar", "NEW", None)]);
        let local = vec![lf(Category::Mod, "mods/lib.jar", "OLD", &[])];
        let plan = compute_plan(&man, &local, &ClientState::default());
        assert_eq!(plan.downloads.len(), 1);
        assert_eq!(plan.downloads[0].replaces.as_deref(), Some("mods/lib.jar"));
        assert!(
            plan.user_extras.is_empty(),
            "must not be listed as an extra"
        );
    }

    #[test]
    fn resource_and_shader_packs_are_never_in_extras() {
        // An unmanaged local pack the server doesn't ship is kept silently.
        let man = manifest(vec![fe(
            Category::Resourcepack,
            "resourcepacks/pack.zip",
            "s1",
            None,
        )]);
        let local = vec![
            lf(Category::Resourcepack, "resourcepacks/pack.zip", "s1", &[]),
            lf(
                Category::Resourcepack,
                "resourcepacks/mypersonal.zip",
                "sX",
                &[],
            ),
            lf(Category::Shaderpack, "shaderpacks/myshader.zip", "sY", &[]),
        ];
        let st = state(&[("resourcepacks/pack.zip", "s1", &[])]);
        let plan = compute_plan(&man, &local, &st);
        assert!(plan.is_noop(), "{plan:?}");
        assert!(
            plan.user_extras.is_empty(),
            "packs are never listed as extras"
        );
    }

    #[test]
    fn managed_pack_removed_server_side_is_removed() {
        let man = manifest(vec![]);
        let local = vec![lf(Category::Shaderpack, "shaderpacks/bsl.zip", "s1", &[])];
        let st = state(&[("shaderpacks/bsl.zip", "s1", &[])]);
        let plan = compute_plan(&man, &local, &st);
        assert_eq!(plan.removals, vec!["shaderpacks/bsl.zip".to_string()]);
    }

    // --- regressions -----------------------------------------------------------

    #[test]
    fn identical_copy_next_to_the_pack_file_is_a_duplicate() {
        // `jei (1).jar` from a double download sits next to the managed jei.jar.
        // Before: silently "adopted" (never persisted, never removed) -> two
        // copies of JEI -> crash, while the UI claimed "up to date".
        let man = manifest(vec![fe(Category::Mod, "mods/jei.jar", "J", Some("jei"))]);
        let local = vec![
            lf(Category::Mod, "mods/jei.jar", "J", &["jei"]),
            lf(Category::Mod, "mods/jei (1).jar", "J", &["jei"]),
        ];
        let st = state(&[("mods/jei.jar", "J", &["jei"])]);
        let plan = compute_plan(&man, &local, &st);
        assert_eq!(plan.duplicates, vec!["mods/jei (1).jar".to_string()]);
        assert!(!plan.is_noop());
        assert!(
            plan.collisions.is_empty() && plan.user_extras.is_empty() && plan.downloads.is_empty()
        );
    }

    #[test]
    fn identical_copy_is_reused_when_the_pack_file_is_missing() {
        let man = manifest(vec![fe(Category::Mod, "mods/jei.jar", "J", Some("jei"))]);
        let local = vec![lf(Category::Mod, "mods/jei (1).jar", "J", &["jei"])];
        let st = state(&[("mods/other.jar", "o", &["other"])]);
        let plan = compute_plan(&man, &local, &st);
        assert_eq!(plan.downloads.len(), 1);
        assert_eq!(
            plan.downloads[0].local_copy.as_deref(),
            Some("mods/jei (1).jar")
        );
        assert_eq!(plan.duplicates, vec!["mods/jei (1).jar".to_string()]);
    }

    #[test]
    fn splitting_a_multi_mod_jar_never_replaces_a_manifest_path() {
        // a.jar used to declare mods a+b; the pack now ships a.jar (new
        // content, same name) and b.jar. Before: both downloads "replaced"
        // a.jar, so installing b.jar moved the fresh a.jar into the backup.
        let man = manifest(vec![
            fe(Category::Mod, "mods/a.jar", "A2", Some("a")),
            fe(Category::Mod, "mods/b.jar", "B", Some("b")),
        ]);
        let local = vec![lf(Category::Mod, "mods/a.jar", "A1", &["a", "b"])];
        let st = state(&[("mods/a.jar", "A1", &["a", "b"])]);
        let plan = compute_plan(&man, &local, &st);
        let a = plan
            .downloads
            .iter()
            .find(|d| d.entry.path == "mods/a.jar")
            .unwrap();
        let b = plan
            .downloads
            .iter()
            .find(|d| d.entry.path == "mods/b.jar")
            .unwrap();
        assert_eq!(a.replaces.as_deref(), Some("mods/a.jar"));
        assert_eq!(
            b.replaces, None,
            "b.jar must not replace the pack's own a.jar"
        );
        assert!(plan.removals.is_empty());
    }

    #[test]
    fn an_old_jar_is_superseded_by_at_most_one_download() {
        // ab-1.0.jar declared a+b; the pack now ships them as two jars.
        let man = manifest(vec![
            fe(Category::Mod, "mods/a-2.0.jar", "A2", Some("a")),
            fe(Category::Mod, "mods/b-2.0.jar", "B2", Some("b")),
        ]);
        let local = vec![lf(Category::Mod, "mods/ab-1.0.jar", "AB", &["a", "b"])];
        let st = state(&[("mods/ab-1.0.jar", "AB", &["a", "b"])]);
        let plan = compute_plan(&man, &local, &st);
        let replacing: Vec<_> = plan
            .downloads
            .iter()
            .filter_map(|d| d.replaces.as_deref())
            .collect();
        assert_eq!(replacing, vec!["mods/ab-1.0.jar"]);
        assert!(plan.removals.is_empty());
    }

    #[test]
    fn plan_is_deterministic() {
        // Two managed copies share a modId. Before, which one was "replaced"
        // and which "removed" depended on HashMap iteration order.
        let man = manifest(vec![fe(
            Category::Mod,
            "mods/create-3.jar",
            "C3",
            Some("create"),
        )]);
        let local = vec![
            lf(Category::Mod, "mods/create-2.jar", "C2", &["create"]),
            lf(Category::Mod, "mods/create-1.jar", "C1", &["create"]),
        ];
        let st = state(&[
            ("mods/create-1.jar", "C1", &["create"]),
            ("mods/create-2.jar", "C2", &["create"]),
        ]);
        let first = compute_plan(&man, &local, &st);
        for _ in 0..50 {
            assert_eq!(compute_plan(&man, &local, &st), first);
        }
        assert_eq!(
            first.downloads[0].replaces.as_deref(),
            Some("mods/create-1.jar")
        );
        assert_eq!(first.removals, vec!["mods/create-2.jar".to_string()]);
    }

    #[test]
    fn deleted_managed_file_is_simply_downloaded_again() {
        let man = manifest(vec![fe(Category::Mod, "mods/a.jar", "s1", Some("a"))]);
        let st = state(&[("mods/a.jar", "s1", &["a"])]);
        let plan = compute_plan(&man, &[], &st);
        assert_eq!(plan.downloads.len(), 1);
        assert_eq!(plan.downloads[0].replaces, None);
        assert!(plan.removals.is_empty());
    }

    #[test]
    fn assessment_flags_a_foreign_instance() {
        let man = manifest(vec![
            fe(Category::Mod, "mods/a.jar", "s1", Some("a")),
            fe(Category::Mod, "mods/b.jar", "s2", Some("b")),
        ]);
        // Fresh folder: fine.
        let fresh = assess_instance(&man, &[], &ClientState::default());
        assert!(fresh.first_run && !fresh.suspicious);

        // First run into another modpack: 12 mods, only one of ours.
        let mut other: Vec<LocalFile> = (0..11)
            .map(|i| {
                lf(
                    Category::Mod,
                    &format!("mods/other{i}.jar"),
                    &format!("o{i}"),
                    &["other"],
                )
            })
            .collect();
        other.push(lf(Category::Mod, "mods/a.jar", "s1", &["a"]));
        let a = assess_instance(&man, &other, &ClientState::default());
        assert_eq!((a.local_mods, a.pack_mods), (12, 1));
        assert!(a.suspicious);

        // A manual install of the pack itself (plus personal mods): fine.
        let mut own: Vec<LocalFile> = (0..10)
            .map(|i| {
                lf(
                    Category::Mod,
                    &format!("mods/p{i}.jar"),
                    &format!("x{i}"),
                    &["a"],
                )
            })
            .collect();
        own.push(lf(Category::Mod, "mods/mine.jar", "m", &["mine"]));
        assert!(!assess_instance(&man, &own, &ClientState::default()).suspicious);

        // Bound to another pack before.
        let mut st = state(&[("mods/a.jar", "s1", &["a"])]);
        st.pack_name = Some("AllTheMods".into());
        let b = assess_instance(&man, &[lf(Category::Mod, "mods/a.jar", "s1", &["a"])], &st);
        assert_eq!(b.previous_pack.as_deref(), Some("AllTheMods"));
        assert!(b.suspicious);
        st.pack_name = Some("BonesAndBees".into());
        assert!(!assess_instance(&man, &[], &st).suspicious);
    }

    // --- switched-off mods (`.jar.disabled`) -------------------------------------

    #[test]
    fn switched_off_mod_that_is_up_to_date_is_left_alone() {
        let man = manifest(vec![fe(Category::Mod, "mods/a.jar", "s1", Some("a"))]);
        let local = vec![lf_off("mods/a.jar", "s1", &["a"])];
        let st = state(&[("mods/a.jar", "s1", &["a"])]);
        let plan = compute_plan(&man, &local, &st);
        assert!(plan.is_noop(), "{plan:?}");
        assert!(plan.user_extras.is_empty() && plan.stale_disabled.is_empty());
    }

    #[test]
    fn new_version_of_a_switched_off_mod_stays_off() {
        let man = manifest(vec![fe(
            Category::Mod,
            "mods/create-2.jar",
            "NEW",
            Some("create"),
        )]);
        let local = vec![lf_off("mods/create-1.jar", "OLD", &["create"])];
        let st = state(&[("mods/create-1.jar", "OLD", &["create"])]);
        let plan = compute_plan(&man, &local, &st);
        assert_eq!(plan.downloads.len(), 1, "{plan:?}");
        let d = &plan.downloads[0];
        assert!(d.disabled);
        assert_eq!(d.target(), "mods/create-2.jar.disabled");
        assert_eq!(d.replaces.as_deref(), Some("mods/create-1.jar.disabled"));
        assert!(plan.removals.is_empty());
    }

    #[test]
    fn changed_content_of_a_switched_off_mod_stays_off() {
        let man = manifest(vec![fe(Category::Mod, "mods/a.jar", "NEW", Some("a"))]);
        let local = vec![lf_off("mods/a.jar", "OLD", &["a"])];
        let st = state(&[("mods/a.jar", "OLD", &["a"])]);
        let plan = compute_plan(&man, &local, &st);
        assert_eq!(plan.downloads.len(), 1);
        assert!(plan.downloads[0].disabled);
        assert_eq!(
            plan.downloads[0].replaces.as_deref(),
            Some("mods/a.jar.disabled")
        );
    }

    #[test]
    fn switched_off_mod_dropped_by_the_pack_is_removed_by_its_real_name() {
        let man = manifest(vec![]);
        let local = vec![lf_off("mods/gone.jar", "s1", &["gone"])];
        let st = state(&[("mods/gone.jar", "s1", &["gone"])]);
        let plan = compute_plan(&man, &local, &st);
        assert_eq!(plan.removals, vec!["mods/gone.jar.disabled".to_string()]);
    }

    #[test]
    fn new_version_is_on_if_any_old_copy_was_on() {
        // Two old copies of one mod; which of them gets "replaced" must not
        // decide whether the new version is on.
        let man = manifest(vec![fe(Category::Mod, "mods/x-3.jar", "NEW", Some("x"))]);
        let local = vec![
            lf_off("mods/x-1.jar", "OLD1", &["x"]),
            lf(Category::Mod, "mods/x-2.jar", "OLD2", &["x"]),
        ];
        let st = state(&[
            ("mods/x-1.jar", "OLD1", &["x"]),
            ("mods/x-2.jar", "OLD2", &["x"]),
        ]);
        let plan = compute_plan(&man, &local, &st);
        assert_eq!(plan.downloads.len(), 1);
        assert!(!plan.downloads[0].disabled, "{plan:?}");
        let mut gone: Vec<&str> = plan
            .removals
            .iter()
            .map(String::as_str)
            .chain(plan.downloads[0].replaces.as_deref())
            .collect();
        gone.sort();
        assert_eq!(gone, vec!["mods/x-1.jar.disabled", "mods/x-2.jar"]);
    }

    #[test]
    fn leftovers_from_older_versions_are_cleaned_up() {
        // An older Bonegrader did not see `x-1.jar.disabled` and installed the
        // pack's x again: the switched-off old copy is a leftover now.
        let man = manifest(vec![
            fe(Category::Mod, "mods/x-2.jar", "X2", Some("x")),
            fe(Category::Mod, "mods/y.jar", "Y", Some("y")),
        ]);
        let local = vec![
            lf_off("mods/x-1.jar", "X1", &["x"]),
            lf(Category::Mod, "mods/x-2.jar", "X2", &["x"]),
            // Same path, both on and off: the switched-off twin is a leftover.
            lf(Category::Mod, "mods/y.jar", "Y", &["y"]),
            lf_off("mods/y.jar", "Y", &["y"]),
        ];
        let st = state(&[("mods/x-2.jar", "X2", &["x"]), ("mods/y.jar", "Y", &["y"])]);
        let plan = compute_plan(&man, &local, &st);
        assert!(plan.downloads.is_empty(), "{plan:?}");
        assert_eq!(
            plan.stale_disabled,
            vec![
                "mods/x-1.jar.disabled".to_string(),
                "mods/y.jar.disabled".into()
            ]
        );
        assert!(!plan.is_noop());
    }

    #[test]
    fn own_switched_off_mods_are_left_alone() {
        let man = manifest(vec![fe(Category::Mod, "mods/a.jar", "s1", Some("a"))]);
        let local = vec![
            lf(Category::Mod, "mods/a.jar", "s1", &["a"]),
            lf_off("mods/mine.jar", "M", &["mine"]),
        ];
        let st = state(&[("mods/a.jar", "s1", &["a"])]);
        let plan = compute_plan(&man, &local, &st);
        assert!(plan.is_noop(), "{plan:?}");
        assert!(plan.user_extras.is_empty() && plan.stale_disabled.is_empty());
    }

    #[test]
    fn own_copy_replacing_a_switched_off_pack_mod_is_not_a_collision() {
        // The player switched off the pack's JEI and runs their own version.
        let man = manifest(vec![fe(
            Category::Mod,
            "mods/jei-19.5.jar",
            "PACK",
            Some("jei"),
        )]);
        let local = vec![
            lf_off("mods/jei-19.5.jar", "PACK", &["jei"]),
            lf(Category::Mod, "mods/jei-20.0.jar", "MINE", &["jei"]),
        ];
        let st = state(&[("mods/jei-19.5.jar", "PACK", &["jei"])]);
        let plan = compute_plan(&man, &local, &st);
        assert!(plan.is_noop(), "{plan:?}");
        assert_eq!(plan.user_extras, vec!["mods/jei-20.0.jar".to_string()]);
    }

    #[test]
    fn colliding_own_copy_switches_the_pack_copy_back_on() {
        // Two own copies of a pack mod are loaded at once (a crash already),
        // so they are collisions and the pack copy goes back on.
        let man = manifest(vec![fe(Category::Mod, "mods/jei.jar", "PACK", Some("jei"))]);
        let local = vec![
            lf_off("mods/jei.jar", "PACK", &["jei"]),
            lf(Category::Mod, "mods/jei-a.jar", "A", &["jei"]),
            lf(Category::Mod, "mods/jei-b.jar", "B", &["jei"]),
        ];
        let st = state(&[("mods/jei.jar", "PACK", &["jei"])]);
        let plan = compute_plan(&man, &local, &st);
        assert_eq!(plan.collisions.len(), 2, "{plan:?}");
        assert_eq!(plan.downloads.len(), 1);
        let d = &plan.downloads[0];
        assert!(!d.disabled);
        assert_eq!(d.replaces.as_deref(), Some("mods/jei.jar.disabled"));
        assert_eq!(
            d.local_copy.as_deref(),
            Some("mods/jei.jar.disabled"),
            "no download needed"
        );
    }

    #[test]
    fn active_identical_copy_keeps_a_switched_off_pack_mod_on() {
        // `jei (1).jar` is on, the pack's copy off: removing the duplicate
        // must not switch JEI off.
        let man = manifest(vec![fe(Category::Mod, "mods/jei.jar", "J", Some("jei"))]);
        let local = vec![
            lf_off("mods/jei.jar", "J", &["jei"]),
            lf(Category::Mod, "mods/jei (1).jar", "J", &["jei"]),
        ];
        let st = state(&[("mods/jei.jar", "J", &["jei"])]);
        let plan = compute_plan(&man, &local, &st);
        assert_eq!(plan.duplicates, vec!["mods/jei (1).jar".to_string()]);
        assert_eq!(plan.downloads.len(), 1);
        assert_eq!(plan.downloads[0].target(), "mods/jei.jar");
    }

    #[test]
    fn first_run_keeps_switched_off_pack_mods_off() {
        // A CurseForge instance where the player switched off a pack mod
        // before ever using Bonegrader.
        let man = manifest(vec![
            fe(Category::Mod, "mods/a.jar", "A", Some("a")),
            fe(Category::Mod, "mods/xaero-2.jar", "X2", Some("xaero")),
        ]);
        let local = vec![
            lf(Category::Mod, "mods/a.jar", "A", &["a"]),
            lf_off("mods/xaero-1.jar", "X1", &["xaero"]),
        ];
        let plan = compute_plan(&man, &local, &ClientState::default());
        assert_eq!(plan.downloads.len(), 1, "{plan:?}");
        let d = &plan.downloads[0];
        assert_eq!(d.target(), "mods/xaero-2.jar.disabled");
        assert_eq!(d.replaces.as_deref(), Some("mods/xaero-1.jar.disabled"));
        assert!(plan.stale_disabled.is_empty() && plan.user_extras.is_empty());
    }

    // --- property test: simulate apply on a model filesystem ---------------------

    mod model {
        use super::*;
        use proptest::prelude::*;

        const IDS: [&str; 4] = ["a", "b", "c", "d"];

        #[derive(Debug, Clone)]
        struct Case {
            manifest: Manifest,
            local: Vec<LocalFile>,
            state: ClientState,
        }

        /// Content `cN` of a manifest entry carries exactly that entry's modId,
        /// so equal bytes always mean equal metadata (as on a real disk).
        fn case() -> impl Strategy<Value = Case> {
            // Manifest: up to 4 mods on distinct paths with distinct contents.
            let manifest_files =
                proptest::collection::btree_map(0usize..6, (0usize..4, any::<bool>()), 0..5);
            // Keyed by (path, switched off): both twins of a path may exist.
            let local_files = proptest::collection::btree_map(
                (0usize..8, any::<bool>()),
                (0usize..8, proptest::collection::btree_set(0usize..4, 0..3)),
                0..6,
            );
            let managed = proptest::collection::btree_set(0usize..8, 0..5);
            (manifest_files, local_files, managed, any::<bool>()).prop_map(
                |(mf, lc, mg, first_run)| {
                    // Distinct modIds and contents across manifest entries.
                    let mut used = BTreeSet::new();
                    let mut files = Vec::new();
                    for (i, (pidx, (id, has_id))) in mf.into_iter().enumerate() {
                        let mod_id = (has_id && used.insert(id)).then(|| IDS[id].to_string());
                        files.push(FileEntry {
                            category: Category::Mod,
                            path: format!("mods/p{pidx}.jar"),
                            file_name: format!("p{pidx}.jar"),
                            size: 1,
                            sha1: format!("c{i}"),
                            sha256: None,
                            mod_id,
                            mod_version: None,
                            mod_name: None,
                            required: None,
                            url: String::new(),
                        });
                    }
                    let content_ids = |sha: &str, fallback: Vec<String>| -> Vec<String> {
                        match files.iter().find(|f| f.sha1 == sha) {
                            Some(f) => f.mod_id.clone().into_iter().collect(),
                            None => fallback,
                        }
                    };
                    let local: Vec<LocalFile> = lc
                        .into_iter()
                        .map(|((pidx, disabled), (content, ids))| {
                            let sha1 = format!("c{content}");
                            let ids = content_ids(
                                &sha1,
                                ids.into_iter().map(|i| IDS[i].to_string()).collect(),
                            );
                            let suffix = if disabled { ".disabled" } else { "" };
                            LocalFile {
                                category: Category::Mod,
                                path: format!("mods/p{pidx}.jar{suffix}"),
                                file_name: format!("p{pidx}.jar{suffix}"),
                                size: 1,
                                sha1,
                                mod_ids: ids,
                                mod_version: None,
                                mod_name: None,
                                dependencies: vec![],
                                provides: vec![],
                                disabled,
                            }
                        })
                        .collect();
                    let mut st = ClientState::default();
                    if !first_run {
                        for pidx in mg {
                            let path = format!("mods/p{pidx}.jar");
                            // The switched-on twin stands for the path if both exist.
                            let at_path = local
                                .iter()
                                .filter(|l| l.enabled_path() == path)
                                .min_by_key(|l| l.disabled);
                            let (sha1, mod_ids) = match at_path {
                                Some(l) => (l.sha1.clone(), l.mod_ids.clone()),
                                None => ("gone".into(), vec![IDS[pidx % 4].to_string()]),
                            };
                            st.managed.insert(path, ManagedEntry { sha1, mod_ids });
                        }
                    }
                    Case {
                        manifest: manifest(files),
                        local,
                        state: st,
                    }
                },
            )
        }

        /// Apply `plan` with default decisions (keep extras, resolve
        /// collisions) to a model disk; returns the new disk and state.
        fn simulate(c: &Case, plan: &UpdatePlan) -> (BTreeMap<String, LocalFile>, ClientState) {
            let mut disk: BTreeMap<String, LocalFile> = c
                .local
                .iter()
                .map(|l| (l.path.clone(), l.clone()))
                .collect();
            let deletions = plan
                .removals
                .iter()
                .chain(&plan.duplicates)
                .chain(&plan.stale_disabled)
                .chain(plan.collisions.iter().map(|c| &c.local_path));
            for p in deletions {
                disk.remove(p);
            }
            for d in &plan.downloads {
                if let Some(r) = &d.replaces {
                    disk.remove(r);
                }
                let target = d.target();
                disk.insert(
                    target.clone(),
                    LocalFile {
                        category: d.entry.category,
                        file_name: target.rsplit('/').next().unwrap().to_string(),
                        path: target,
                        size: d.entry.size,
                        sha1: d.entry.sha1.clone(),
                        mod_ids: d.entry.mod_id.clone().into_iter().collect(),
                        mod_version: None,
                        mod_name: None,
                        dependencies: vec![],
                        provides: vec![],
                        disabled: d.disabled,
                    },
                );
            }
            let mut st = ClientState::default();
            for e in &c.manifest.files {
                if disk.contains_key(&e.path) || disk.contains_key(&disabled_path(&e.path)) {
                    st.managed.insert(
                        e.path.clone(),
                        ManagedEntry {
                            sha1: e.sha1.clone(),
                            mod_ids: e.mod_id.clone().into_iter().collect(),
                        },
                    );
                }
            }
            (disk, st)
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(2000))]

            #[test]
            fn apply_converges_and_never_loses_player_files(c in case()) {
                let plan = compute_plan(&c.manifest, &c.local, &c.state);

                // Structural sanity: each old file is replaced at most once, and
                // never one that another download writes to.
                let targets: BTreeSet<String> = plan.downloads.iter().map(Download::target).collect();
                let mut seen = BTreeSet::new();
                for d in &plan.downloads {
                    if let Some(r) = &d.replaces {
                        prop_assert!(seen.insert(r.clone()), "replaced twice: {r} in {plan:?}");
                        prop_assert!(r == &d.target() || !targets.contains(r), "{plan:?}");
                    }
                }

                let (disk, st) = simulate(&c, &plan);
                let active = |disk: &BTreeMap<String, LocalFile>, id: &str| {
                    disk.values()
                        .filter(|l| !l.disabled && l.mod_ids.iter().any(|x| x == id))
                        .count()
                };

                // Every pack file is present once — on or off — with the right
                // content.
                for e in &c.manifest.files {
                    let on = disk.get(&e.path).map(|l| l.sha1.as_str());
                    let off = disk.get(&disabled_path(&e.path)).map(|l| l.sha1.as_str());
                    prop_assert!(on.is_none() || off.is_none(), "both twins of {}: {:?}", e.path, plan);
                    prop_assert_eq!(on.or(off), Some(e.sha1.as_str()), "{:?}", plan);
                }
                for id in c.manifest.mods().filter_map(|f| f.mod_id.as_deref()) {
                    // No pack modId is loaded twice (the game would crash).
                    let n = active(&disk, id);
                    prop_assert!(n <= 1, "modId {} active {} times; plan {:?}", id, n, plan);
                    // An update never switches a mod off …
                    let before = c.local.iter().any(|l| !l.disabled && l.mod_ids.iter().any(|x| x == id));
                    prop_assert!(!before || n == 1, "modId {} switched off; plan {:?}", id, plan);
                    // … nor back on, if the player switched off every pack copy.
                    let pack_owned = |l: &LocalFile| {
                        let p = l.enabled_path();
                        let stands = !l.disabled || !c.local.iter().any(|o| !o.disabled && o.enabled_path() == p);
                        let adoptable = c.state.is_first_run()
                            && (c.manifest.files.iter().any(|f| f.sha1 == l.sha1)
                                || l.mod_ids.iter().any(|x| c.manifest.mods().any(|f| f.mod_id.as_deref() == Some(x.as_str()))));
                        stands && (c.state.managed.contains_key(p) || c.manifest.files.iter().any(|f| f.path == p) || adoptable)
                    };
                    let switched_off = c.local.iter().any(|l| l.disabled && pack_owned(l) && l.mod_ids.iter().any(|x| x == id));
                    prop_assert!(before || !switched_off || n == 0, "modId {} switched on; plan {:?}", id, plan);
                }
                // A switched-off copy of a pack file at its own path (by modId,
                // content, or — for libraries — by name) is only replaced by a
                // switched-on one if another active copy of the mod goes.
                for d in &plan.downloads {
                    let e = &d.entry;
                    let same = |l: &LocalFile| l.sha1 == e.sha1 || e.mod_id.as_ref().is_some_and(|m| l.mod_ids.contains(m));
                    let Some(old) = d.replaces.as_ref().and_then(|r| c.local.iter().find(|l| &l.path == r)) else { continue };
                    if old.disabled && old.enabled_path() == e.path && (e.mod_id.is_none() || same(old)) {
                        let other_active = c.local.iter().any(|l| !l.disabled && same(l));
                        prop_assert!(d.disabled || other_active, "{} switched on without reason: {plan:?}", old.path);
                    }
                }
                // The player's own extras survive untouched.
                for p in &plan.user_extras {
                    let before = c.local.iter().find(|l| &l.path == p).unwrap();
                    prop_assert_eq!(disk.get(p), Some(before));
                }
                // A second run has nothing left to do.
                let local2: Vec<LocalFile> = disk.values().cloned().collect();
                let plan2 = compute_plan(&c.manifest, &local2, &st);
                prop_assert!(plan2.is_noop(), "second run not a no-op: {:?}\nfirst: {:?}", plan2, plan);
            }
        }
    }
}
