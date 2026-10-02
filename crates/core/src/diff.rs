//! The update planner — pure and deterministic.
//!
//! [`compute_plan`] takes the server manifest, a scan of the local instance and
//! the previous client state, and returns an [`UpdatePlan`] describing what
//! *should* happen. It performs no I/O and mutates nothing on disk. Applying the
//! plan (with the user's answers to the extras/collision prompts) is the
//! caller's responsibility and must honour the invariants below.

use crate::manifest::{Category, FileEntry, Manifest};
use crate::scan::LocalFile;
use crate::state::ClientState;
use std::collections::{HashMap, HashSet};

/// A file to fetch from the server, optionally replacing an existing local file
/// (a same-path content change, or an old version of the same `modId`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    pub entry: FileEntry,
    /// Existing client-relative path this download supersedes, if any. Applying
    /// the download must remove this old file (after the new one is verified).
    pub replaces: Option<String>,
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
/// * Only ever delete paths listed in `removals`, resolved `collisions`, or a
///   download's `replaces` — never by scanning a folder.
/// * Every such path is already known to be inside a managed folder; still,
///   re-validate with [`crate::paths::is_safe_managed_path`] before deleting.
/// * Download everything to a temp location and verify SHA-1 *before* deleting
///   or moving anything; apply as one batch; move replaced/removed files to a
///   backup dir rather than hard-deleting.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePlan {
    pub downloads: Vec<Download>,
    /// Managed files no longer present in the manifest (safe to remove).
    pub removals: Vec<String>,
    /// Unmanaged local *mods* not in the manifest (ask keep/remove). Packs are
    /// never listed here — the player's own resource/shader packs are kept
    /// silently.
    pub user_extras: Vec<String>,
    /// Duplicate-`modId` clashes needing a user decision.
    pub collisions: Vec<Collision>,
    /// Local files newly taken under management this run (first-run adoption or
    /// silent adoption of an exact-content match). Informational.
    pub adopted: Vec<String>,
}

impl UpdatePlan {
    /// True if applying the plan would change anything on disk.
    pub fn is_noop(&self) -> bool {
        self.downloads.is_empty() && self.removals.is_empty() && self.collisions.is_empty()
    }
}

/// Compute the update plan. Deterministic: output vectors are sorted.
pub fn compute_plan(manifest: &Manifest, local: &[LocalFile], state: &ClientState) -> UpdatePlan {
    let local_by_path: HashMap<&str, &LocalFile> =
        local.iter().map(|l| (l.path.as_str(), l)).collect();

    let manifest_paths: HashSet<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    let manifest_mod_ids: HashSet<&str> = manifest
        .mods()
        .filter_map(|f| f.mod_id.as_deref())
        .collect();
    let manifest_shas: HashSet<&str> = manifest.files.iter().map(|f| f.sha1.as_str()).collect();

    let original_managed: HashSet<String> = state.managed.keys().cloned().collect();
    let mut managed: HashSet<String> = original_managed.clone();

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
                managed.insert(lf.path.clone());
            }
        }
    }

    // modId -> managed path (for version-bump matching against the pre-update set).
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
    let mut managed_modid_path: HashMap<String, String> = HashMap::new();
    for p in &managed {
        for id in mod_ids_for(p) {
            managed_modid_path.insert(id, p.clone());
        }
    }

    let mut plan = UpdatePlan::default();
    let mut replaced_paths: HashSet<String> = HashSet::new();

    // --- 2. Downloads (walk the manifest) -------------------------------------
    for e in &manifest.files {
        if let Some(lf) = local_by_path.get(e.path.as_str()) {
            if lf.sha1 == e.sha1 {
                managed.insert(e.path.clone()); // up to date; ensure managed
                continue;
            }
            // Same path, changed content.
            plan.downloads.push(Download {
                entry: e.clone(),
                replaces: Some(e.path.clone()),
            });
            replaced_paths.insert(e.path.clone());
            continue;
        }

        // No file at the target path. For mods, see if a managed file with the
        // same modId is an older version to be superseded.
        if e.category == Category::Mod {
            if let Some(mid) = e.mod_id.as_deref() {
                if let Some(old_path) = managed_modid_path.get(mid) {
                    plan.downloads.push(Download {
                        entry: e.clone(),
                        replaces: Some(old_path.clone()),
                    });
                    replaced_paths.insert(old_path.clone());
                    continue;
                }
            }
        }

        // Brand-new file.
        plan.downloads.push(Download {
            entry: e.clone(),
            replaces: None,
        });
    }

    // --- 3. Removals (managed files gone from the manifest) -------------------
    for p in &managed {
        if replaced_paths.contains(p) || manifest_paths.contains(p.as_str()) {
            continue;
        }
        plan.removals.push(p.clone());
    }

    // --- 4. Extras & collisions (unmanaged local mods) ------------------------
    for lf in local {
        if lf.category != Category::Mod || managed.contains(&lf.path) {
            continue;
        }
        // A file sitting at a manifest path is managed content (being installed
        // or updated in step 2), never a user "extra".
        if manifest_paths.contains(lf.path.as_str()) {
            continue;
        }
        // An exact-content match of a pack file: adopt silently rather than
        // nagging about it.
        if manifest_shas.contains(lf.sha1.as_str()) {
            managed.insert(lf.path.clone());
            continue;
        }
        match lf
            .mod_ids
            .iter()
            .find(|id| manifest_mod_ids.contains(id.as_str()))
        {
            Some(mid) => {
                let manifest_path = manifest
                    .mods()
                    .find(|e| e.mod_id.as_deref() == Some(mid.as_str()))
                    .map(|e| e.path.clone())
                    .unwrap_or_default();
                plan.collisions.push(Collision {
                    local_path: lf.path.clone(),
                    mod_id: mid.clone(),
                    manifest_path,
                });
            }
            None => plan.user_extras.push(lf.path.clone()),
        }
    }

    // --- 5. Finalise ----------------------------------------------------------
    plan.adopted = managed.difference(&original_managed).cloned().collect();

    plan.downloads
        .sort_by(|a, b| a.entry.path.cmp(&b.entry.path));
    plan.removals.sort();
    plan.user_extras.sort();
    plan.collisions
        .sort_by(|a, b| a.local_path.cmp(&b.local_path));
    plan.adopted.sort();

    plan
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
            mod_id: mod_id.map(str::to_string),
            mod_version: None,
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
        }
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
        let st = state(&[]); // first run, but jei modId matches -> adopted? see note
                             // Force non-first-run so the local jei stays unmanaged and collides.
        let st = ClientState {
            managed: {
                let mut m = BTreeMap::new();
                m.insert(
                    "mods/placeholder.jar".into(),
                    ManagedEntry {
                        sha1: "z".into(),
                        mod_ids: vec!["placeholder".into()],
                    },
                );
                m
            },
            ..st
        };
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
}
