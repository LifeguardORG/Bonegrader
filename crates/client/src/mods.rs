//! The player's mod list: every jar in `mods/`, switched on or off (the
//! launchers' `x.jar.disabled` convention, which updates keep), what the pack
//! says about it, and switching mods safely.
//!
//! Switching only renames within `mods/` — `x.jar` ⇄ `x.jar.disabled` — never
//! overwrites, and is all-or-nothing for a batch (switching off a library
//! together with the mods that need it).

use crate::apply::{ensure_movable, move_with_retry};
use crate::session::{load_cache, load_state, save_cache};
use anyhow::{bail, Context, Result};
use bonegrader_core::deps::{broken, dependents, requirements, Broken, Need};
use bonegrader_core::manifest::{Category, FileEntry, Manifest};
use bonegrader_core::paths::{disabled_path, enabled_path, is_mod_jar, is_safe_managed_path};
use bonegrader_core::scan::{scan_instance_cached, DisabledMods, LocalFile};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

/// One jar in `mods/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModRow {
    /// Real name: `mods/x.jar`, or `mods/x.jar.disabled` while switched off.
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub enabled: bool,
    /// Part of the pack (else one of the player's own mods).
    pub pack: bool,
    /// Players need it to join the server. `None`: not known (own mods,
    /// manifests from before the marking, no connection).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

/// The list plus what keeps the game from starting right now.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModList {
    pub mods: Vec<ModRow>,
    pub health: Vec<Broken>,
}

fn scan(instance: &Path) -> Result<Vec<LocalFile>> {
    if !instance.is_dir() {
        bail!("Instanz-Ordner nicht gefunden: {}", instance.display());
    }
    let mut cache = load_cache(instance);
    let local = scan_instance_cached(
        instance,
        &[Category::Mod],
        DisabledMods::Include,
        &mut cache,
    )
    .context("Mods einlesen")?;
    save_cache(instance, &cache);
    Ok(local)
}

/// The pack's entry for a local jar: same path, same content or same modId.
fn pack_entry<'a>(manifest: &'a Manifest, l: &LocalFile) -> Option<&'a FileEntry> {
    manifest.mods().find(|e| {
        e.path == l.enabled_path()
            || e.sha1 == l.sha1
            || e.mod_id.as_ref().is_some_and(|id| l.mod_ids.contains(id))
    })
}

/// Every mod of the instance, sorted by name. With the `manifest` the pack's
/// marking is known; without (offline), pack membership comes from the state.
pub fn list(instance: &Path, manifest: Option<&Manifest>) -> Result<ModList> {
    let local = scan(instance)?;
    let state = load_state(instance).unwrap_or_default();
    let mut mods: Vec<ModRow> = local
        .iter()
        .map(|l| {
            let entry = manifest.and_then(|m| pack_entry(m, l));
            ModRow {
                path: l.path.clone(),
                name: l.mod_name.clone(),
                version: l.mod_version.clone(),
                enabled: !l.disabled,
                pack: entry.is_some() || state.managed.contains_key(l.enabled_path()),
                required: entry.and_then(|e| e.required),
            }
        })
        .collect();
    let sort_key = |r: &ModRow| {
        r.name
            .clone()
            .unwrap_or_else(|| r.path.rsplit('/').next().unwrap_or_default().to_string())
            .to_lowercase()
    };
    mods.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)).then(a.path.cmp(&b.path)));
    Ok(ModList {
        mods,
        health: broken(&local),
    })
}

/// What a switch would do to the rest (asked before switching).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchCheck {
    /// Mods that stop loading if `off` go off.
    pub dependents: Vec<Need>,
    /// Switched-off mods that `on` need switched on too.
    pub requirements: Vec<String>,
}

pub fn check_switch(instance: &Path, off: &[String], on: &[String]) -> Result<SwitchCheck> {
    let local = scan(instance)?;
    let off: BTreeSet<&str> = off.iter().map(String::as_str).collect();
    let on: BTreeSet<&str> = on.iter().map(String::as_str).collect();
    Ok(SwitchCheck {
        dependents: dependents(&local, &off),
        requirements: requirements(&local, &on),
    })
}

/// One mod to switch on or off.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Switch {
    pub path: String,
    pub enabled: bool,
}

/// Two jars with the same modId would be loaded.
#[derive(Debug, thiserror::Error)]
#[error(
    "{file} lässt sich nicht einschalten: „{mod_id}“ ist schon aktiv ({other}) – zwei \
     gleiche Mods würden das Spiel abstürzen lassen."
)]
pub struct Clash {
    pub file: String,
    pub mod_id: String,
    pub other: String,
}

/// Switch mods on or off (rename `x.jar` ⇄ `x.jar.disabled`), all or
/// nothing. Returns the new paths.
pub fn switch(instance: &Path, switches: &[Switch]) -> Result<Vec<String>> {
    let local = scan(instance)?;
    let mut targets: BTreeSet<String> = BTreeSet::new();
    let mut moves: Vec<(String, String)> = Vec::new();
    for s in switches {
        let Some(lf) = local.iter().find(|l| l.path == s.path) else {
            bail!("{} gibt es nicht (mehr) – Liste neu laden.", s.path);
        };
        if !s.path.starts_with("mods/") || !is_safe_managed_path(&s.path) {
            bail!("unzulässiger Pfad: {}", s.path);
        }
        let target = match (s.enabled, lf.disabled) {
            (true, true) => enabled_path(&s.path).map(str::to_string),
            (false, false) if is_mod_jar(&s.path) => Some(disabled_path(&s.path)),
            _ => None,
        };
        let Some(target) = target else {
            bail!(
                "{} ist schon {}.",
                s.path,
                if s.enabled { "an" } else { "aus" }
            );
        };
        if instance.join(&target).exists() || !targets.insert(target.clone()) {
            bail!(
                "{} lässt sich nicht umschalten: {target} gibt es schon (eine zweite Kopie?).",
                s.path
            );
        }
        moves.push((s.path.clone(), target));
    }

    // After the switch: no modId may be declared by two switched-on jars.
    let moving: BTreeSet<&str> = moves.iter().map(|(from, _)| from.as_str()).collect();
    let on_after: Vec<&LocalFile> = local
        .iter()
        .filter(|l| {
            let switched = switches.iter().find(|s| s.path == l.path);
            switched.map_or(!l.disabled, |s| s.enabled)
        })
        .collect();
    for s in switches.iter().filter(|s| s.enabled) {
        let lf = local
            .iter()
            .find(|l| l.path == s.path)
            .expect("checked above");
        for id in &lf.mod_ids {
            if let Some(other) = on_after
                .iter()
                .find(|o| o.path != lf.path && o.mod_ids.contains(id))
            {
                return Err(Clash {
                    file: lf.file_name.clone(),
                    mod_id: id.clone(),
                    other: other.file_name.clone(),
                }
                .into());
            }
        }
    }

    for from in &moving {
        let p = instance.join(from);
        ensure_movable(&p).with_context(|| format!("{from} ist gesperrt"))?;
    }
    let mut done: Vec<(&str, &str)> = Vec::new();
    for (from, to) in &moves {
        if let Err(e) = move_with_retry(&instance.join(from), &instance.join(to)) {
            let mut problems = Vec::new();
            for (f, t) in done.iter().rev() {
                if let Err(e) = move_with_retry(&instance.join(t), &instance.join(f)) {
                    problems.push(format!("{t} → {f}: {e}"));
                }
            }
            let what = format!("{from} umbenennen");
            return Err(if problems.is_empty() {
                anyhow::Error::new(e).context(format!("{what} – es wurde nichts verändert"))
            } else {
                anyhow::Error::new(e).context(format!(
                    "{what} – Rücknahme unvollständig: {}",
                    problems.join("; ")
                ))
            });
        }
        done.push((from, to));
    }
    Ok(moves.into_iter().map(|(_, to)| to).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bonegrader_core::manifest::{Loader, Manifest};
    use std::io::Write;

    fn jar(mod_id: &str, name: &str, deps: &[&str]) -> Vec<u8> {
        let mut toml =
            format!("[[mods]]\nmodId=\"{mod_id}\"\nversion=\"1\"\ndisplayName=\"{name}\"\n");
        for d in deps {
            toml.push_str(&format!("[[dependencies.{mod_id}]]\nmodId=\"{d}\"\n"));
        }
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        z.start_file(
            "META-INF/neoforge.mods.toml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        z.write_all(toml.as_bytes()).unwrap();
        z.finish().unwrap().into_inner()
    }

    fn instance(files: &[(&str, Vec<u8>)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, bytes) in files {
            let p = dir.path().join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
        dir
    }

    fn entry(path: &str, mod_id: &str, required: Option<bool>) -> FileEntry {
        FileEntry {
            category: Category::Mod,
            path: path.into(),
            file_name: path.rsplit('/').next().unwrap().into(),
            size: 1,
            sha1: "0".repeat(40),
            sha256: None,
            mod_id: Some(mod_id.into()),
            mod_version: None,
            mod_name: None,
            required,
            url: String::new(),
        }
    }

    fn manifest(files: Vec<FileEntry>) -> Manifest {
        Manifest {
            schema_version: 1,
            pack_name: "P".into(),
            channel: "main".into(),
            generated_at: "t".into(),
            loader: Loader {
                loader_type: "neoforge".into(),
                mc_version: "1.21.1".into(),
                loader_version: "21.1.234".into(),
            },
            files,
            client: None,
            server: None,
            seeds: vec![],
        }
    }

    fn sw(path: &str, enabled: bool) -> Switch {
        Switch {
            path: path.into(),
            enabled,
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir.join("mods"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn lists_pack_and_own_mods_with_their_marking() {
        let inst = instance(&[
            ("mods/create.jar", jar("create", "Create", &[])),
            ("mods/sodium.jar.disabled", jar("sodium", "Sodium", &[])),
            ("mods/mine.jar", jar("mine", "Mine", &["sodium"])),
        ]);
        let m = manifest(vec![
            entry("mods/create.jar", "create", Some(true)),
            entry("mods/sodium-0.6.jar", "sodium", Some(false)),
        ]);
        let l = list(inst.path(), Some(&m)).unwrap();
        let got: Vec<(&str, bool, bool, Option<bool>)> = l
            .mods
            .iter()
            .map(|r| (r.name.as_deref().unwrap(), r.enabled, r.pack, r.required))
            .collect();
        assert_eq!(
            got,
            vec![
                ("Create", true, true, Some(true)),
                ("Mine", true, false, None),
                ("Sodium", false, true, Some(false)),
            ]
        );
        assert_eq!(l.health.len(), 1, "mine needs the switched-off sodium");
        assert_eq!(
            l.health[0].fix,
            vec!["mods/sodium.jar.disabled".to_string()]
        );

        let offline = list(inst.path(), None).unwrap();
        assert!(offline.mods.iter().all(|r| r.required.is_none() && !r.pack));
    }

    #[test]
    fn switching_off_and_on_renames_and_checks_dependents() {
        let inst = instance(&[
            ("mods/lib.jar", jar("lib", "Lib", &[])),
            ("mods/a.jar", jar("a", "A", &["lib"])),
        ]);
        let i = inst.path();
        let c = check_switch(i, &["mods/lib.jar".into()], &[]).unwrap();
        assert_eq!(c.dependents.len(), 1);
        assert_eq!(c.dependents[0].path, "mods/a.jar");

        let new = switch(i, &[sw("mods/lib.jar", false), sw("mods/a.jar", false)]).unwrap();
        assert_eq!(new, vec!["mods/lib.jar.disabled", "mods/a.jar.disabled"]);
        assert_eq!(names(i), vec!["a.jar.disabled", "lib.jar.disabled"]);

        let c = check_switch(i, &[], &["mods/a.jar.disabled".into()]).unwrap();
        assert_eq!(c.requirements, vec!["mods/lib.jar.disabled".to_string()]);
        switch(
            i,
            &[
                sw("mods/a.jar.disabled", true),
                sw("mods/lib.jar.disabled", true),
            ],
        )
        .unwrap();
        assert_eq!(names(i), vec!["a.jar", "lib.jar"]);
    }

    #[test]
    fn a_batch_changes_all_or_nothing() {
        // The second switch can't happen (its target exists): the first one
        // must not stay done.
        let inst = instance(&[
            ("mods/a.jar", jar("a", "A", &[])),
            ("mods/b.jar", jar("b", "B", &[])),
            ("mods/b.jar.disabled", jar("b", "B old", &[])),
        ]);
        let i = inst.path();
        let e = switch(i, &[sw("mods/a.jar", false), sw("mods/b.jar", false)]).unwrap_err();
        assert!(e.to_string().contains("gibt es schon"), "{e}");
        assert_eq!(names(i), vec!["a.jar", "b.jar", "b.jar.disabled"]);
    }

    #[test]
    fn a_second_copy_of_an_active_mod_is_not_switched_on() {
        let inst = instance(&[
            ("mods/jei-pack.jar", jar("jei", "JEI", &[])),
            ("mods/jei-mine.jar.disabled", jar("jei", "JEI", &[])),
        ]);
        let i = inst.path();
        let e = switch(i, &[sw("mods/jei-mine.jar.disabled", true)]).unwrap_err();
        assert!(e.downcast_ref::<Clash>().is_some(), "{e}");
        assert_eq!(names(i), vec!["jei-mine.jar.disabled", "jei-pack.jar"]);

        // Swapping them in one batch is fine.
        switch(
            i,
            &[
                sw("mods/jei-pack.jar", false),
                sw("mods/jei-mine.jar.disabled", true),
            ],
        )
        .unwrap();
        assert_eq!(names(i), vec!["jei-mine.jar", "jei-pack.jar.disabled"]);
    }

    #[test]
    fn only_existing_mods_are_switched_and_only_the_other_way() {
        let inst = instance(&[("mods/a.jar", jar("a", "A", &[]))]);
        let i = inst.path();
        for bad in [
            sw("mods/a.jar", true),
            sw("mods/missing.jar", false),
            sw("../mods/a.jar", false),
            sw("config/a.jar", false),
        ] {
            assert!(switch(i, std::slice::from_ref(&bad)).is_err(), "{bad:?}");
        }
        assert_eq!(names(i), vec!["a.jar"]);
    }
}
