//! Which mods need which: before switching mods off, what else stops loading;
//! before switching one on, what it needs switched on too; and whether the
//! mods switched on right now can start at all.
//!
//! A jar provides its own modIds and the ones it bundles (jar-in-jar), and
//! needs its required client dependencies ([`LocalFile::dependencies`]). A
//! dependency no local jar provides — Minecraft, NeoForge, or something the
//! pack simply doesn't have — is not judged here. Only mods count.

use crate::manifest::Category;
use crate::scan::LocalFile;
use std::collections::BTreeSet;

/// A mod that would fail to load, and the modIds it lacks.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Need {
    pub path: String,
    pub missing: Vec<String>,
}

/// A switched-on mod that can't load because what it needs is switched off,
/// and the switched-off mods that would provide it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Broken {
    pub path: String,
    pub missing: Vec<String>,
    pub fix: Vec<String>,
}

fn provided(f: &LocalFile) -> impl Iterator<Item = &str> {
    f.mod_ids.iter().chain(&f.provides).map(String::as_str)
}

fn mods(files: &[LocalFile]) -> impl Iterator<Item = &LocalFile> {
    files.iter().filter(|f| f.category == Category::Mod)
}

/// The modIds `f` needs that `have` lacks, among those some local jar provides.
fn lacks(f: &LocalFile, have: &BTreeSet<&str>, known: &BTreeSet<&str>) -> Vec<String> {
    f.dependencies
        .iter()
        .filter(|d| known.contains(d.as_str()) && !have.contains(d.as_str()))
        .cloned()
        .collect()
}

/// Mods among `active` that can't load, transitively: one that fails takes
/// what it provides with it.
fn failing<'a>(
    active: &[&'a LocalFile],
    known: &BTreeSet<&str>,
) -> Vec<(&'a LocalFile, Vec<String>)> {
    let mut active: Vec<&LocalFile> = active.to_vec();
    let mut out = Vec::new();
    loop {
        let have: BTreeSet<&str> = active.iter().flat_map(|f| provided(f)).collect();
        let (fails, ok): (Vec<_>, Vec<_>) = active
            .into_iter()
            .map(|f| (f, lacks(f, &have, known)))
            .partition(|(_, missing)| !missing.is_empty());
        if fails.is_empty() {
            return out;
        }
        out.extend(fails);
        active = ok.into_iter().map(|(f, _)| f).collect();
    }
}

/// Mods that stop loading if `off` (real paths) are switched off — directly or
/// because a mod they need goes with them. Mods that can't load anyway are
/// not blamed on the switch.
pub fn dependents(files: &[LocalFile], off: &BTreeSet<&str>) -> Vec<Need> {
    let known: BTreeSet<&str> = mods(files).flat_map(provided).collect();
    let on: Vec<&LocalFile> = mods(files).filter(|f| !f.disabled).collect();
    let already: BTreeSet<&str> = failing(&on, &known)
        .into_iter()
        .map(|(f, _)| f.path.as_str())
        .collect();
    let remaining: Vec<&LocalFile> = on
        .into_iter()
        .filter(|f| !off.contains(f.path.as_str()))
        .collect();
    let mut out: Vec<Need> = failing(&remaining, &known)
        .into_iter()
        .filter(|(f, _)| !already.contains(f.path.as_str()))
        .map(|(f, missing)| Need {
            path: f.path.clone(),
            missing,
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Switched-off mods that must come on with `on` (real paths) so these can
/// load — and what those need in turn. Per missing modId the switched-off
/// provider with the smallest path is picked.
pub fn requirements(files: &[LocalFile], on: &BTreeSet<&str>) -> Vec<String> {
    let known: BTreeSet<&str> = mods(files).flat_map(provided).collect();
    let mut active: BTreeSet<&str> = mods(files)
        .filter(|f| !f.disabled || on.contains(f.path.as_str()))
        .map(|f| f.path.as_str())
        .collect();
    let mut todo: Vec<&str> = on.iter().copied().collect();
    let mut added: BTreeSet<&str> = BTreeSet::new();
    while let Some(path) = todo.pop() {
        let Some(f) = mods(files).find(|f| f.path == path) else {
            continue;
        };
        let have: BTreeSet<&str> = mods(files)
            .filter(|g| active.contains(g.path.as_str()))
            .flat_map(provided)
            .collect();
        for id in lacks(f, &have, &known) {
            let provider = mods(files)
                .filter(|g| !active.contains(g.path.as_str()) && provided(g).any(|p| p == id))
                .min_by(|a, b| a.path.cmp(&b.path));
            if let Some(g) = provider {
                active.insert(&g.path);
                added.insert(&g.path);
                todo.push(&g.path);
            }
        }
    }
    added
        .into_iter()
        .filter(|p| !on.contains(p))
        .map(str::to_string)
        .collect()
}

/// Switched-on mods that can't load because something they need is switched
/// off (in a launcher, or before an update brought a mod needing it).
pub fn broken(files: &[LocalFile]) -> Vec<Broken> {
    let known: BTreeSet<&str> = mods(files).flat_map(provided).collect();
    let on: Vec<&LocalFile> = mods(files).filter(|f| !f.disabled).collect();
    let mut out: Vec<Broken> = failing(&on, &known)
        .into_iter()
        .map(|(f, missing)| {
            let fix: BTreeSet<&str> = missing
                .iter()
                .filter_map(|id| {
                    mods(files)
                        .filter(|g| g.disabled && provided(g).any(|p| p == id))
                        .map(|g| g.path.as_str())
                        .min()
                })
                .collect();
            Broken {
                path: f.path.clone(),
                missing,
                fix: fix.into_iter().map(str::to_string).collect(),
            }
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(path: &str, ids: &[&str], deps: &[&str], provides: &[&str]) -> LocalFile {
        let disabled = path.ends_with(".disabled");
        LocalFile {
            category: Category::Mod,
            path: path.into(),
            file_name: path.rsplit('/').next().unwrap().into(),
            size: 1,
            sha1: path.into(),
            mod_ids: ids.iter().map(|s| s.to_string()).collect(),
            mod_version: None,
            mod_name: None,
            dependencies: deps.iter().map(|s| s.to_string()).collect(),
            provides: provides.iter().map(|s| s.to_string()).collect(),
            disabled,
        }
    }

    fn set<'a>(paths: &[&'a str]) -> BTreeSet<&'a str> {
        paths.iter().copied().collect()
    }

    fn paths(needs: &[Need]) -> Vec<&str> {
        needs.iter().map(|n| n.path.as_str()).collect()
    }

    #[test]
    fn switching_off_a_library_takes_its_dependents_along() {
        let files = vec![
            m("mods/lib.jar", &["lib"], &["neoforge"], &[]),
            m("mods/a.jar", &["a"], &["lib", "minecraft"], &[]),
            m("mods/b.jar", &["b"], &["a"], &[]),
            m("mods/c.jar", &["c"], &[], &[]),
        ];
        let d = dependents(&files, &set(&["mods/lib.jar"]));
        assert_eq!(
            paths(&d),
            vec!["mods/a.jar", "mods/b.jar"],
            "b needs a, which goes"
        );
        assert_eq!(d[0].missing, vec!["lib"]);
        assert_eq!(d[1].missing, vec!["a"]);
        assert!(dependents(&files, &set(&["mods/c.jar"])).is_empty());
    }

    #[test]
    fn bundled_libraries_count_and_a_second_provider_keeps_things_running() {
        let files = vec![
            m("mods/create.jar", &["create"], &[], &["ponder", "flywheel"]),
            m("mods/fairylights.jar", &["fairylights"], &[], &["flywheel"]),
            m("mods/addon.jar", &["addon"], &["ponder", "flywheel"], &[]),
            m("mods/lights-addon.jar", &["la"], &["flywheel"], &[]),
        ];
        let d = dependents(&files, &set(&["mods/create.jar"]));
        assert_eq!(
            paths(&d),
            vec!["mods/addon.jar"],
            "ponder only comes with create"
        );
        assert_eq!(d[0].missing, vec!["ponder"]);
        let d = dependents(&files, &set(&["mods/fairylights.jar"]));
        assert!(d.is_empty(), "create still brings flywheel: {d:?}");
    }

    #[test]
    fn mods_that_cannot_start_anyway_are_not_blamed_on_the_switch() {
        let files = vec![
            m("mods/lib.jar.disabled", &["lib"], &[], &[]),
            m("mods/broken.jar", &["broken"], &["lib"], &[]),
            m("mods/x.jar", &["x"], &[], &[]),
            m("mods/y.jar", &["y"], &["x"], &[]),
        ];
        let d = dependents(&files, &set(&["mods/x.jar"]));
        assert_eq!(paths(&d), vec!["mods/y.jar"]);
    }

    #[test]
    fn switching_on_brings_what_is_needed_transitively() {
        let files = vec![
            m("mods/a.jar.disabled", &["a"], &["lib"], &[]),
            m("mods/lib-2.jar.disabled", &["lib"], &["core"], &[]),
            m("mods/lib-1.jar.disabled", &["lib"], &["core"], &[]),
            m("mods/core.jar.disabled", &["core"], &[], &[]),
            m("mods/other.jar", &["other"], &[], &["shared"]),
            m("mods/b.jar.disabled", &["b"], &["shared"], &[]),
        ];
        assert_eq!(
            requirements(&files, &set(&["mods/a.jar.disabled"])),
            vec!["mods/core.jar.disabled", "mods/lib-1.jar.disabled"],
            "smallest path picked for lib, then what lib needs"
        );
        assert!(
            requirements(&files, &set(&["mods/b.jar.disabled"])).is_empty(),
            "already provided by a switched-on jar"
        );
    }

    #[test]
    fn broken_states_come_with_the_fix() {
        let files = vec![
            m("mods/kff.jar.disabled", &[], &[], &["kotlinforforge"]),
            m("mods/k-mod.jar", &["kmod"], &["kotlinforforge"], &[]),
            m("mods/k-addon.jar", &["kaddon"], &["kmod"], &[]),
            m("mods/fine.jar", &["fine"], &["neoforge"], &[]),
        ];
        let b = broken(&files);
        assert_eq!(b.len(), 2, "{b:?}");
        assert_eq!(b[0].path, "mods/k-addon.jar");
        assert!(b[0].fix.is_empty(), "fails only because k-mod does");
        assert_eq!(b[1].path, "mods/k-mod.jar");
        assert_eq!(b[1].missing, vec!["kotlinforforge"]);
        assert_eq!(b[1].fix, vec!["mods/kff.jar.disabled"]);
    }
}
