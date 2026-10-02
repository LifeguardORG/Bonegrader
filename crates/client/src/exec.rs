//! Turn a plan + user decisions into a concrete execution plan.

use bonegrader_core::diff::{Download, UpdatePlan};
use bonegrader_core::manifest::SeedEntry;
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
    /// Files to back up and remove (removals, duplicates, resolved collisions,
    /// chosen extras).
    pub deletions: Vec<String>,
    /// Seed files to create if (still) missing; never overwrite anything.
    pub seeds: Vec<SeedEntry>,
}

impl ExecutionPlan {
    pub fn is_empty(&self) -> bool {
        self.downloads.is_empty() && self.deletions.is_empty() && self.seeds.is_empty()
    }
}

/// Combine an [`UpdatePlan`] with [`Decisions`].
///
/// * Decisions only count for paths the plan actually offered: a stale or
///   forged answer can never delete anything else.
/// * A *kept* collision cancels the paired managed download (keeping both would
///   crash the game, so if the user keeps theirs, we don't install ours) and
///   still retires the old pack copy that download would have replaced.
/// * A *resolved* collision deletes the user's clashing file.
pub fn finalize(plan: &UpdatePlan, decisions: &Decisions) -> ExecutionPlan {
    let kept: BTreeSet<&str> = plan
        .collisions
        .iter()
        .filter(|c| decisions.keep_collision_local.contains(&c.local_path))
        .map(|c| c.manifest_path.as_str())
        .collect();

    let mut deletions: BTreeSet<String> = BTreeSet::new();
    let mut downloads = Vec::new();
    for d in &plan.downloads {
        if kept.contains(d.entry.path.as_str()) {
            deletions.extend(d.replaces.iter().cloned());
        } else {
            downloads.push(d.clone());
        }
    }

    deletions.extend(plan.removals.iter().cloned());
    deletions.extend(plan.duplicates.iter().cloned());
    for c in &plan.collisions {
        if !decisions.keep_collision_local.contains(&c.local_path) {
            deletions.insert(c.local_path.clone());
        }
    }
    deletions.extend(
        plan.user_extras
            .iter()
            .filter(|p| decisions.remove_extras.contains(*p))
            .cloned(),
    );

    ExecutionPlan {
        downloads,
        deletions: deletions.into_iter().collect(),
        seeds: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bonegrader_core::diff::{Collision, Download, UpdatePlan};
    use bonegrader_core::manifest::{Category, FileEntry};

    fn dl(path: &str, replaces: Option<&str>) -> Download {
        Download {
            entry: FileEntry {
                category: Category::Mod,
                path: path.into(),
                file_name: path.rsplit('/').next().unwrap().into(),
                size: 1,
                sha1: "s".into(),
                sha256: None,
                mod_id: None,
                mod_version: None,
                url: "u".into(),
            },
            replaces: replaces.map(str::to_string),
            local_copy: None,
        }
    }

    fn jei_collision() -> Collision {
        Collision {
            local_path: "mods/jei-old.jar".into(),
            mod_id: "jei".into(),
            manifest_path: "mods/jei-19.5.jar".into(),
        }
    }

    #[test]
    fn default_decisions_keep_extras_and_resolve_collisions() {
        let plan = UpdatePlan {
            downloads: vec![dl("mods/jei-19.5.jar", None)],
            removals: vec!["mods/gone.jar".into()],
            duplicates: vec!["mods/a (1).jar".into()],
            user_extras: vec!["mods/mine.jar".into()],
            collisions: vec![jei_collision()],
            adopted: vec![],
        };
        let exec = finalize(&plan, &Decisions::default());
        // Managed jei still downloaded; old jei deleted; user extra kept.
        assert_eq!(exec.downloads.len(), 1);
        assert_eq!(
            exec.deletions,
            vec![
                "mods/a (1).jar".to_string(),
                "mods/gone.jar".into(),
                "mods/jei-old.jar".into()
            ]
        );
    }

    #[test]
    fn keeping_a_collision_cancels_its_download() {
        let plan = UpdatePlan {
            downloads: vec![dl("mods/jei-19.5.jar", None)],
            collisions: vec![jei_collision()],
            ..Default::default()
        };
        let mut d = Decisions::default();
        d.keep_collision_local.insert("mods/jei-old.jar".into());
        let exec = finalize(&plan, &d);
        assert!(
            exec.downloads.is_empty(),
            "kept collision must cancel download"
        );
        assert!(exec.deletions.is_empty());
    }

    #[test]
    fn keeping_a_collision_still_retires_the_old_pack_copy() {
        // The pack's old JEI (managed) would have been replaced by the new one.
        // If the player keeps their own JEI, the old pack copy must go anyway —
        // otherwise two JEIs are loaded.
        let plan = UpdatePlan {
            downloads: vec![dl("mods/jei-19.5.jar", Some("mods/jei-19.0.jar"))],
            collisions: vec![jei_collision()],
            ..Default::default()
        };
        let mut d = Decisions::default();
        d.keep_collision_local.insert("mods/jei-old.jar".into());
        let exec = finalize(&plan, &d);
        assert!(exec.downloads.is_empty());
        assert_eq!(exec.deletions, vec!["mods/jei-19.0.jar".to_string()]);
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

    #[test]
    fn decisions_for_paths_not_in_the_plan_are_ignored() {
        // A stale UI (or a forged request) asks to delete a pack mod as an
        // "extra" and to keep a collision that does not exist.
        let plan = UpdatePlan {
            downloads: vec![dl("mods/x.jar", None)],
            ..Default::default()
        };
        let mut d = Decisions::default();
        d.remove_extras.insert("mods/x.jar".into());
        d.keep_collision_local.insert("mods/x.jar".into());
        let exec = finalize(&plan, &d);
        assert!(exec.deletions.is_empty(), "{exec:?}");
        assert_eq!(exec.downloads.len(), 1);
    }
}
