//! Turn a plan + user decisions into a concrete execution plan.

use bonegrader_core::diff::{Download, UpdatePlan};
use std::collections::BTreeSet;

/// The user's answers to the interactive prompts.
#[derive(Debug, Clone, Default)]
pub struct Decisions {
    /// Subset of `UpdatePlan::user_extras` the user chose to delete (default: keep all).
    pub remove_extras: BTreeSet<String>,
    /// Collision `local_path`s the user chose to KEEP (default: resolve = replace
    /// with the managed version).
    pub keep_collision_local: BTreeSet<String>,
}

/// Concrete, decision-resolved actions for the applier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionPlan {
    /// Files to download (each may carry a `replaces` path handled at apply time).
    pub downloads: Vec<Download>,
    /// Files to back up and remove (removals + resolved collisions + chosen extras).
    pub deletions: Vec<String>,
}

/// Combine an [`UpdatePlan`] with [`Decisions`].
///
/// * A *kept* collision cancels the paired managed download (keeping both would
///   crash the game, so if the user keeps theirs, we don't install ours).
/// * A *resolved* collision deletes the user's clashing file.
pub fn finalize(plan: &UpdatePlan, decisions: &Decisions) -> ExecutionPlan {
    let kept_manifest_paths: BTreeSet<&str> = plan
        .collisions
        .iter()
        .filter(|c| decisions.keep_collision_local.contains(&c.local_path))
        .map(|c| c.manifest_path.as_str())
        .collect();

    let downloads: Vec<Download> = plan
        .downloads
        .iter()
        .filter(|d| !kept_manifest_paths.contains(d.entry.path.as_str()))
        .cloned()
        .collect();

    let mut deletions: BTreeSet<String> = BTreeSet::new();
    deletions.extend(plan.removals.iter().cloned());
    for c in &plan.collisions {
        if !decisions.keep_collision_local.contains(&c.local_path) {
            deletions.insert(c.local_path.clone());
        }
    }
    deletions.extend(decisions.remove_extras.iter().cloned());

    ExecutionPlan {
        downloads,
        deletions: deletions.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bonegrader_core::diff::{Collision, Download, UpdatePlan};
    use bonegrader_core::manifest::{Category, FileEntry};

    fn dl(path: &str) -> Download {
        Download {
            entry: FileEntry {
                category: Category::Mod,
                path: path.into(),
                file_name: path.into(),
                size: 1,
                sha1: "s".into(),
                mod_id: None,
                mod_version: None,
                url: "u".into(),
            },
            replaces: None,
        }
    }

    #[test]
    fn default_decisions_keep_extras_and_resolve_collisions() {
        let plan = UpdatePlan {
            downloads: vec![dl("mods/jei-19.5.jar")],
            removals: vec!["mods/gone.jar".into()],
            user_extras: vec!["mods/mine.jar".into()],
            collisions: vec![Collision {
                local_path: "mods/jei-old.jar".into(),
                mod_id: "jei".into(),
                manifest_path: "mods/jei-19.5.jar".into(),
            }],
            adopted: vec![],
        };
        let exec = finalize(&plan, &Decisions::default());
        // Managed jei still downloaded; old jei deleted; user extra kept.
        assert_eq!(exec.downloads.len(), 1);
        assert!(exec.deletions.contains(&"mods/gone.jar".to_string()));
        assert!(exec.deletions.contains(&"mods/jei-old.jar".to_string()));
        assert!(!exec.deletions.contains(&"mods/mine.jar".to_string()));
    }

    #[test]
    fn keeping_a_collision_cancels_its_download() {
        let plan = UpdatePlan {
            downloads: vec![dl("mods/jei-19.5.jar")],
            removals: vec![],
            user_extras: vec![],
            collisions: vec![Collision {
                local_path: "mods/jei-old.jar".into(),
                mod_id: "jei".into(),
                manifest_path: "mods/jei-19.5.jar".into(),
            }],
            adopted: vec![],
        };
        let mut d = Decisions::default();
        d.keep_collision_local.insert("mods/jei-old.jar".into());
        let exec = finalize(&plan, &d);
        assert!(
            exec.downloads.is_empty(),
            "kept collision must cancel download"
        );
        assert!(!exec.deletions.contains(&"mods/jei-old.jar".to_string()));
    }

    #[test]
    fn removing_a_chosen_extra() {
        let plan = UpdatePlan {
            user_extras: vec!["mods/mine.jar".into()],
            ..Default::default()
        };
        let mut d = Decisions::default();
        d.remove_extras.insert("mods/mine.jar".into());
        let exec = finalize(&plan, &d);
        assert_eq!(exec.deletions, vec!["mods/mine.jar".to_string()]);
    }
}
