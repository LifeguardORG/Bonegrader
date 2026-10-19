//! Which pack mods players need to join the server ("Pflicht").
//!
//! The server mod `bonegraderexport` (bab-perf) writes
//! `bonegrader/client-requirements.json` at every server start: for each mod
//! the server loaded, whether a client must have it — a non-optional network
//! channel, entries of synchronized registries, a synchronized registry type or
//! a network-checked enum extension — and why. This module matches that export
//! to the pack's jars, decides the marking written into the manifest and
//! compares pack and server.
//!
//! The marking is the admin's: an explicit choice wins, then the marking of
//! the manifest published before (by modId), then the export's suggestion; a
//! mod nothing knows about counts as mandatory. A suggested mod's required
//! dependencies are suggested too (a library registers nothing, but a client
//! without it can't load the mod that needs it).

use anyhow::{bail, Result};
use bonegrader_core::manifest::{Category, FileEntry, Manifest};
use bonegrader_core::scan::LocalFile;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Export format this build understands.
pub const EXPORT_FORMAT: u32 = 1;

/// Marking of a mod nothing is known about (no earlier marking, no export).
pub const DEFAULT_REQUIRED: bool = true;

/// The server's `client-requirements.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerExport {
    pub format: u32,
    pub complete: bool,
    #[serde(default)]
    pub problems: Vec<String>,
    pub generated_at: String,
    #[serde(default)]
    pub minecraft: Option<String>,
    #[serde(default)]
    pub neoforge: Option<String>,
    pub mods: Vec<ServerMod>,
}

/// One mod the server loaded.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerMod {
    pub mod_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    /// The jar on the server's disk (the bundling jar for jar-in-jar mods).
    pub file: String,
    #[serde(default)]
    pub sha1: Option<String>,
    /// Name of the bundled jar if the mod comes as jar-in-jar.
    #[serde(default)]
    pub nested: Option<String>,
    pub required: bool,
    #[serde(default)]
    pub channels: Vec<Channel>,
    #[serde(default)]
    pub synced_entries: BTreeMap<String, u64>,
    #[serde(default)]
    pub synced_registry_types: Vec<String>,
    #[serde(default)]
    pub networked_enums: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Channel {
    pub id: String,
    #[serde(default)]
    pub version: Option<String>,
    pub optional: bool,
}

/// Parse an export, refusing formats newer than this build knows.
pub fn parse_export(bytes: &[u8]) -> Result<ServerExport> {
    #[derive(Deserialize)]
    struct Probe {
        format: u32,
    }
    let probe: Probe = serde_json::from_slice(bytes)
        .map_err(|e| anyhow::anyhow!("Server-Export ist kein gültiges JSON ({e})"))?;
    if probe.format > EXPORT_FORMAT {
        bail!(
            "Der Server-Export hat Format {}, diese Bonegrader-Version kennt nur {EXPORT_FORMAT} – bitte Bonegrader aktualisieren.",
            probe.format
        );
    }
    serde_json::from_slice(bytes).map_err(|e| anyhow::anyhow!("Server-Export unlesbar ({e})"))
}

/// The key a marking is remembered by: the modId (stable across versions),
/// else the path.
pub fn mark_key(e: &FileEntry) -> String {
    e.mod_id.clone().unwrap_or_else(|| e.path.clone())
}

/// One jar on the server with every mod it brings (own and bundled).
struct ServerJar<'a> {
    file: &'a str,
    sha1: Option<&'a str>,
    mods: Vec<&'a ServerMod>,
}

impl ServerJar<'_> {
    fn required(&self) -> bool {
        self.mods.iter().any(|m| m.required)
    }

    /// Why players need it (German, for the admin app), or what it has.
    fn reasons(&self) -> Vec<String> {
        let required: Vec<&&ServerMod> = self.mods.iter().filter(|m| m.required).collect();
        if required.is_empty() {
            let optional = self.mods.iter().map(|m| m.channels.len()).sum::<usize>();
            return if optional > 0 {
                vec![format!("nur optionale Netzwerk-Kanäle ({optional})")]
            } else {
                vec!["auf dem Server, aber nichts, was Spieler brauchen".into()]
            };
        }
        let mut out = Vec::new();
        let channels: Vec<&str> = required
            .iter()
            .flat_map(|m| m.channels.iter())
            .filter(|c| !c.optional)
            .map(|c| c.id.as_str())
            .collect();
        match channels.as_slice() {
            [] => {}
            [one] => out.push(format!("Pflicht-Kanal {one}")),
            many => out.push(format!("{} Pflicht-Kanäle", many.len())),
        }
        let mut entries: BTreeMap<&str, u64> = BTreeMap::new();
        for m in &required {
            for (reg, n) in &m.synced_entries {
                *entries.entry(reg.as_str()).or_default() += n;
            }
        }
        if !entries.is_empty() {
            let mut by_count: Vec<(&str, u64)> = entries.into_iter().collect();
            by_count.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            let shown: Vec<String> = by_count
                .iter()
                .take(3)
                .map(|(reg, n)| format!("{n} {}", registry_label(reg)))
                .collect();
            let more = if by_count.len() > 3 { " …" } else { "" };
            out.push(format!("registriert {}{more}", shown.join(", ")));
        }
        let types: BTreeSet<&str> = required
            .iter()
            .flat_map(|m| m.synced_registry_types.iter().map(String::as_str))
            .collect();
        if !types.is_empty() {
            out.push(format!(
                "eigene Registry ({})",
                types.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        if required.iter().any(|m| !m.networked_enums.is_empty()) {
            out.push("erweitert eine netzwerkgeprüfte Enum".into());
        }
        let bundled: Vec<&str> = required
            .iter()
            .filter(|m| m.nested.is_some())
            .map(|m| m.mod_id.as_str())
            .collect();
        if !bundled.is_empty() {
            out.push(format!("mitgeliefert: {}", bundled.join(", ")));
        }
        out
    }

    fn has_network_content(&self) -> bool {
        self.mods
            .iter()
            .any(|m| !m.channels.is_empty() || !m.synced_entries.is_empty())
    }
}

/// Plural names of the registries players notice.
fn registry_label(reg: &str) -> &str {
    match reg.strip_prefix("minecraft:").unwrap_or(reg) {
        "block" => "Blöcke",
        "item" => "Items",
        "entity_type" => "Entities",
        "block_entity_type" => "Block-Entities",
        "particle_type" => "Partikel",
        "sound_event" => "Sounds",
        "mob_effect" => "Statuseffekte",
        "menu" => "Menüs",
        "data_component_type" => "Item-Datentypen",
        "potion" => "Tränke",
        "fluid" => "Flüssigkeiten",
        "recipe_serializer" => "Rezept-Typen",
        "attribute" => "Attribute",
        "villager_profession" => "Berufe",
        "command_argument_type" => "Befehlsargumente",
        "armor_material" => "Rüstungsmaterialien",
        _ => reg,
    }
}

fn server_jars(export: &ServerExport) -> Vec<ServerJar<'_>> {
    let mut by_file: BTreeMap<&str, ServerJar> = BTreeMap::new();
    for m in &export.mods {
        by_file
            .entry(m.file.as_str())
            .or_insert_with(|| ServerJar {
                file: &m.file,
                sha1: m.sha1.as_deref(),
                mods: Vec::new(),
            })
            .mods
            .push(m);
    }
    by_file.into_values().collect()
}

/// How a pack jar was found on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ServerMatch {
    /// The very same jar (same SHA-1).
    SameFile,
    /// Same modId, but another file (another version).
    SameMod,
    /// Same file name, but other content.
    SameName,
}

/// The server jar for a pack jar: same content, else the jar whose own
/// (not bundled) mod has the entry's modId, else the same file name.
fn find<'a, 'b>(
    e: &FileEntry,
    jars: &'b [ServerJar<'a>],
) -> Option<(&'b ServerJar<'a>, ServerMatch)> {
    if let Some(j) = jars.iter().find(|j| j.sha1 == Some(e.sha1.as_str())) {
        return Some((j, ServerMatch::SameFile));
    }
    if let Some(id) = &e.mod_id {
        let own = |j: &&ServerJar| j.mods.iter().any(|m| m.nested.is_none() && &m.mod_id == id);
        if let Some(j) = jars.iter().find(own) {
            return Some((j, ServerMatch::SameMod));
        }
    }
    jars.iter()
        .find(|j| j.file == e.file_name)
        .map(|j| (j, ServerMatch::SameName))
}

/// Where a marking came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MarkSource {
    /// Chosen in this preview (or by `--required`/`--optional`).
    Choice,
    /// The manifest published before.
    Previous,
    /// The server export.
    Server,
    /// Nothing known: [`DEFAULT_REQUIRED`].
    Default,
}

/// The marking of one pack mod, with what the server says about it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequiredRow {
    pub key: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// What is published.
    pub required: bool,
    pub source: MarkSource,
    /// What the server export suggests (`None` without an export).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested: Option<bool>,
    /// How the jar was found on the server (`None`: not on it, or no export).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_server: Option<ServerMatch>,
    pub reasons: Vec<String>,
    /// Not in the manifest published before.
    pub new: bool,
}

/// Decide every mod's marking (see the module docs for the precedence).
/// Per modId, the rows (indices) whose jar provides it, in path order.
fn providers<'a>(
    rows: &[RequiredRow],
    local: &BTreeMap<&str, &'a LocalFile>,
) -> BTreeMap<&'a str, Vec<usize>> {
    let mut out: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, r) in rows.iter().enumerate() {
        if let Some(lf) = local.get(r.path.as_str()) {
            for id in lf.mod_ids.iter().chain(&lf.provides) {
                out.entry(id.as_str()).or_default().push(i);
            }
        }
    }
    out
}

fn label(r: &RequiredRow) -> String {
    r.name
        .clone()
        .unwrap_or_else(|| r.path.rsplit('/').next().unwrap_or(&r.path).to_string())
}

/// Decide every mod's marking (see the module docs for the precedence).
/// `local` are the scanned files the manifest was built from (for their
/// dependencies).
pub fn resolve(
    manifest: &Manifest,
    local: &[LocalFile],
    previous: Option<&Manifest>,
    export: Option<&ServerExport>,
    choices: &BTreeMap<String, bool>,
) -> Vec<RequiredRow> {
    let mut previous_marks: BTreeMap<String, Option<bool>> = BTreeMap::new();
    for e in previous.into_iter().flat_map(|m| m.mods()) {
        previous_marks.insert(mark_key(e), e.required);
    }
    let jars = export.map(server_jars).unwrap_or_default();
    let mut rows: Vec<RequiredRow> = manifest
        .mods()
        .map(|e| {
            let key = mark_key(e);
            let found = export.and_then(|_| find(e, &jars));
            let (suggested, reasons) = match (export, found) {
                (None, _) => (None, Vec::new()),
                (Some(_), Some((jar, _))) => (Some(jar.required()), jar.reasons()),
                (Some(_), None) => (
                    Some(false),
                    vec!["nicht auf dem Server – Spieler können ohne ihn beitreten".into()],
                ),
            };
            RequiredRow {
                path: e.path.clone(),
                name: e.mod_name.clone(),
                version: e.mod_version.clone(),
                required: DEFAULT_REQUIRED,
                source: MarkSource::Default,
                suggested,
                on_server: found.map(|(_, how)| how),
                reasons,
                new: !previous_marks.contains_key(&key),
                key,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.path.cmp(&b.path));

    // What a suggested mod needs is suggested too (unless some provider of it
    // is suggested already); the first provider in path order is picked.
    let local: BTreeMap<&str, &LocalFile> = local.iter().map(|l| (l.path.as_str(), l)).collect();
    let provided_by = providers(&rows, &local);
    let mut todo: Vec<usize> = (0..rows.len())
        .filter(|&i| rows[i].suggested == Some(true))
        .collect();
    while let Some(i) = todo.pop() {
        let Some(lf) = local.get(rows[i].path.as_str()) else {
            continue;
        };
        for dep in &lf.dependencies {
            let Some(ps) = provided_by.get(dep.as_str()) else {
                continue;
            };
            if ps.iter().any(|&p| rows[p].suggested == Some(true)) {
                continue;
            }
            let p = ps[0];
            let reason = format!("gebraucht von {}", label(&rows[i]));
            if rows[p].suggested == Some(false) {
                rows[p].reasons = vec![reason];
            } else {
                rows[p].reasons.push(reason);
            }
            rows[p].suggested = Some(true);
            todo.push(p);
        }
    }

    for r in &mut rows {
        let previous = previous_marks.get(&r.key).copied().flatten();
        (r.required, r.source) = if let Some(&c) = choices.get(&r.key) {
            (c, MarkSource::Choice)
        } else if let Some(p) = previous {
            (p, MarkSource::Previous)
        } else if let Some(s) = r.suggested {
            (s, MarkSource::Server)
        } else {
            (DEFAULT_REQUIRED, MarkSource::Default)
        };
    }
    rows
}

/// A mandatory mod that needs a mod marked optional: a player who switches
/// that off can't load the mandatory one.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionalNeed {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The modId it needs.
    pub dependency: String,
    /// The pack mods providing it (all marked optional).
    pub providers: Vec<String>,
    /// Their mark keys (to mark them mandatory).
    pub provider_keys: Vec<String>,
}

/// Mandatory mods whose dependencies are only provided by optional ones.
pub fn needs_optional(rows: &[RequiredRow], local: &[LocalFile]) -> Vec<OptionalNeed> {
    let local: BTreeMap<&str, &LocalFile> = local.iter().map(|l| (l.path.as_str(), l)).collect();
    let provided_by = providers(rows, &local);
    let mut out = Vec::new();
    for r in rows.iter().filter(|r| r.required) {
        let Some(lf) = local.get(r.path.as_str()) else {
            continue;
        };
        for dep in &lf.dependencies {
            let Some(ps) = provided_by.get(dep.as_str()) else {
                continue;
            };
            if ps.iter().all(|&p| !rows[p].required) {
                out.push(OptionalNeed {
                    path: r.path.clone(),
                    name: r.name.clone(),
                    dependency: dep.clone(),
                    providers: ps.iter().map(|&p| rows[p].path.clone()).collect(),
                    provider_keys: ps.iter().map(|&p| rows[p].key.clone()).collect(),
                });
            }
        }
    }
    out
}

/// Write the markings into the manifest's mod entries.
pub fn apply(manifest: &mut Manifest, rows: &[RequiredRow]) {
    let by_path: BTreeMap<&str, bool> =
        rows.iter().map(|r| (r.path.as_str(), r.required)).collect();
    for e in manifest
        .files
        .iter_mut()
        .filter(|e| e.category == Category::Mod)
    {
        e.required = by_path.get(e.path.as_str()).copied();
    }
}

/// The marking of a build, with the server comparison.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Marking {
    pub rows: Vec<RequiredRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<ServerCheck>,
    /// Mandatory mods that need a mod marked optional.
    pub needs_optional: Vec<OptionalNeed>,
}

/// Decide the markings, write them into `manifest` and compare it with the
/// server — what the admin app and the CLI do before showing or publishing.
pub fn mark(
    manifest: &mut Manifest,
    local: &[LocalFile],
    previous: Option<&Manifest>,
    export: Option<&ServerExport>,
    choices: &BTreeMap<String, bool>,
) -> Marking {
    let rows = resolve(manifest, local, previous, export, choices);
    apply(manifest, &rows);
    Marking {
        check: export.map(|e| check(manifest, e)),
        needs_optional: needs_optional(&rows, local),
        rows,
    }
}

/// A server mod in another version than the pack's.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionMismatch {
    pub pack_path: String,
    pub server_file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A server jar players need but the pack lacks.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissingJar {
    pub server_file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub reasons: Vec<String>,
}

/// Pack and server compared (only with an export).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCheck {
    pub generated_at: String,
    pub complete: bool,
    pub problems: Vec<String>,
    /// "NeoForge 21.1.234 / MC 1.21.1 auf dem Server, im Pack …" if they differ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader_mismatch: Option<String>,
    /// Needed to join, but not in the pack: nobody could join.
    pub missing_in_pack: Vec<MissingJar>,
    /// Same mod in another version: channels or registries may not match.
    pub version_mismatch: Vec<VersionMismatch>,
    /// Pack mods the server doesn't have (client-only mods; fine).
    pub not_on_server: Vec<String>,
}

pub fn check(manifest: &Manifest, export: &ServerExport) -> ServerCheck {
    let jars = server_jars(export);
    let mut matched: BTreeSet<&str> = BTreeSet::new();
    let mut version_mismatch = Vec::new();
    let mut not_on_server = Vec::new();
    for e in manifest.mods() {
        match find(e, &jars) {
            Some((jar, how)) => {
                matched.insert(jar.file);
                if how != ServerMatch::SameFile && jar.has_network_content() {
                    version_mismatch.push(VersionMismatch {
                        pack_path: e.path.clone(),
                        server_file: jar.file.to_string(),
                        name: e.mod_name.clone(),
                    });
                }
            }
            None => not_on_server.push(e.path.clone()),
        }
    }
    let missing_in_pack = jars
        .iter()
        .filter(|j| j.required() && !matched.contains(j.file))
        .map(|j| MissingJar {
            server_file: j.file.to_string(),
            name: j
                .mods
                .iter()
                .find(|m| m.nested.is_none())
                .and_then(|m| m.name.clone()),
            reasons: j.reasons(),
        })
        .collect();
    let l = &manifest.loader;
    let loader_differs = l.loader_type == "neoforge"
        && export
            .neoforge
            .as_deref()
            .is_some_and(|v| v != l.loader_version);
    let mc_differs = export
        .minecraft
        .as_deref()
        .is_some_and(|v| v != l.mc_version);
    let loader_mismatch = (loader_differs || mc_differs).then(|| {
        format!(
            "Server: NeoForge {} / MC {} – Pack: {} {} / MC {}",
            export.neoforge.as_deref().unwrap_or("?"),
            export.minecraft.as_deref().unwrap_or("?"),
            l.loader_type,
            l.loader_version,
            l.mc_version
        )
    });
    ServerCheck {
        generated_at: export.generated_at.clone(),
        complete: export.complete,
        problems: export.problems.clone(),
        loader_mismatch,
        missing_in_pack,
        version_mismatch,
        not_on_server,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bonegrader_core::manifest::Loader;

    fn fe(path: &str, sha1: &str, mod_id: Option<&str>, required: Option<bool>) -> FileEntry {
        FileEntry {
            category: Category::Mod,
            path: path.into(),
            file_name: path.rsplit('/').next().unwrap().into(),
            size: 1,
            sha1: sha1.into(),
            sha256: None,
            mod_id: mod_id.map(str::to_string),
            mod_version: None,
            mod_name: None,
            required,
            url: String::new(),
        }
    }

    fn man(files: Vec<FileEntry>) -> Manifest {
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

    /// Trimmed from a real export of the BonesAndBees server.
    const EXPORT: &str = r#"{
      "format": 1, "complete": true, "problems": [],
      "generatedAt": "2026-10-19T21:36:35Z", "minecraft": "1.21.1", "neoforge": "21.1.234",
      "mods": [
        {"modId": "create", "name": "Create", "version": "6.0.10", "file": "create-6.0.10.jar",
         "sha1": "c1", "nested": null, "required": true,
         "channels": [{"id": "create:main", "version": "1", "optional": false, "protocols": ["play"]}],
         "syncedEntries": {"minecraft:block": 812, "minecraft:item": 1203}, "syncedRegistryTypes": [], "networkedEnums": []},
        {"modId": "ponder", "name": "Ponder", "version": "1.0", "file": "create-6.0.10.jar",
         "sha1": "c1", "nested": "ponder.jar", "required": true, "channels": [],
         "syncedEntries": {"minecraft:item": 1}, "syncedRegistryTypes": [], "networkedEnums": []},
        {"modId": "cosycritters", "name": "Cosy Critters", "version": "0.0.6", "file": "cosy.jar",
         "sha1": "s2", "nested": null, "required": true, "channels": [],
         "syncedEntries": {"minecraft:particle_type": 4}, "syncedRegistryTypes": [], "networkedEnums": []},
        {"modId": "jei", "name": "JEI", "version": "19", "file": "jei.jar", "sha1": "s3", "nested": null,
         "required": false, "channels": [{"id": "jei:cheat_permission", "version": "3", "optional": true}],
         "syncedEntries": {}, "syncedRegistryTypes": [], "networkedEnums": []},
        {"modId": "polymorph", "name": "Polymorph", "version": "1.1", "file": "polymorph.jar", "sha1": "s4",
         "nested": null, "required": true, "channels": [{"id": "polymorph:sync", "version": "1", "optional": false}],
         "syncedEntries": {}, "syncedRegistryTypes": [], "networkedEnums": []},
        {"modId": "lagscope", "name": "LagScope", "version": "0.2", "file": "lagscope.jar", "sha1": "s5",
         "nested": null, "required": false, "channels": [], "syncedEntries": {}, "syncedRegistryTypes": [], "networkedEnums": []}
      ]
    }"#;

    fn export() -> ServerExport {
        parse_export(EXPORT.as_bytes()).unwrap()
    }

    #[test]
    fn newer_export_formats_are_refused() {
        let e = parse_export(br#"{"format": 2, "complete": true, "generatedAt": "t", "mods": []}"#)
            .unwrap_err();
        assert!(e.to_string().contains("aktualisieren"), "{e}");
        assert!(parse_export(b"<html>").is_err());
    }

    #[test]
    fn suggestions_follow_the_server_and_explain_themselves() {
        let m = man(vec![
            fe("mods/create-6.0.10.jar", "c1", Some("create"), None),
            fe("mods/cosy.jar", "s2", Some("cosycritters"), None),
            fe("mods/jei-other-version.jar", "OTHER", Some("jei"), None),
            fe("mods/sodium.jar", "s9", Some("sodium"), None),
        ]);
        let rows = resolve(&m, &[], None, Some(&export()), &BTreeMap::new());
        let get = |k: &str| rows.iter().find(|r| r.key == k).unwrap();

        let create = get("create");
        assert!(create.required && create.source == MarkSource::Server);
        assert_eq!(create.on_server, Some(ServerMatch::SameFile));
        assert_eq!(
            create.reasons,
            vec![
                "Pflicht-Kanal create:main".to_string(),
                "registriert 1204 Items, 812 Blöcke".into(),
                "mitgeliefert: ponder".into()
            ]
        );
        assert_eq!(
            get("cosycritters").reasons,
            vec!["registriert 4 Partikel".to_string()]
        );

        let jei = get("jei");
        assert!(!jei.required, "only optional channels");
        assert_eq!(jei.on_server, Some(ServerMatch::SameMod));
        assert_eq!(
            jei.reasons,
            vec!["nur optionale Netzwerk-Kanäle (1)".to_string()]
        );

        let sodium = get("sodium");
        assert!(!sodium.required && sodium.on_server.is_none());
        assert!(rows.iter().all(|r| r.new));
    }

    #[test]
    fn choices_beat_earlier_markings_which_beat_the_server() {
        let m = man(vec![
            fe("mods/create-6.0.10.jar", "c1", Some("create"), None),
            fe("mods/cosy.jar", "s2", Some("cosycritters"), None),
            fe("mods/polymorph.jar", "s4", Some("polymorph"), None),
        ]);
        // Published before: cosycritters optional (by hand), create unknown.
        let prev = man(vec![
            fe("mods/create-6.0.9.jar", "old", Some("create"), None),
            fe("mods/cosy.jar", "s2", Some("cosycritters"), Some(false)),
        ]);
        let mut choices = BTreeMap::new();
        choices.insert("polymorph".to_string(), false);
        let rows = resolve(&m, &[], Some(&prev), Some(&export()), &choices);
        let get = |k: &str| rows.iter().find(|r| r.key == k).unwrap();

        assert_eq!(
            (get("create").required, get("create").source),
            (true, MarkSource::Server)
        );
        assert!(
            !get("create").new,
            "known from before, only without a marking"
        );
        let cosy = get("cosycritters");
        assert_eq!((cosy.required, cosy.source), (false, MarkSource::Previous));
        assert_eq!(
            cosy.suggested,
            Some(true),
            "the admin app flags the difference"
        );
        assert_eq!(
            (get("polymorph").required, get("polymorph").source),
            (false, MarkSource::Choice)
        );
        assert!(get("polymorph").new);
    }

    fn local(path: &str, ids: &[&str], deps: &[&str], provides: &[&str]) -> LocalFile {
        LocalFile {
            category: Category::Mod,
            path: path.into(),
            file_name: path.rsplit('/').next().unwrap().into(),
            size: 1,
            sha1: String::new(),
            mod_ids: ids.iter().map(|s| s.to_string()).collect(),
            mod_version: None,
            mod_name: None,
            dependencies: deps.iter().map(|s| s.to_string()).collect(),
            provides: provides.iter().map(|s| s.to_string()).collect(),
            disabled: false,
        }
    }

    #[test]
    fn what_a_mandatory_mod_needs_is_suggested_too() {
        // Kotlin for Forge registers nothing, so the server alone would call
        // it optional — but Create (here) can't load without it.
        let mut create = fe("mods/create-6.0.10.jar", "c1", Some("create"), None);
        create.mod_name = Some("Create".into());
        let m = man(vec![
            create,
            fe("mods/kff-all.jar", "k", None, None),
            fe("mods/sodium.jar", "s9", Some("sodium"), None),
        ]);
        let files = vec![
            local(
                "mods/create-6.0.10.jar",
                &["create"],
                &["kotlinforforge", "neoforge"],
                &["ponder"],
            ),
            local("mods/kff-all.jar", &[], &[], &["kotlinforforge"]),
            local("mods/sodium.jar", &["sodium"], &[], &[]),
        ];
        let rows = resolve(&m, &files, None, Some(&export()), &BTreeMap::new());
        let kff = rows.iter().find(|r| r.path == "mods/kff-all.jar").unwrap();
        assert_eq!((kff.required, kff.suggested), (true, Some(true)));
        assert_eq!(kff.reasons, vec!["gebraucht von Create".to_string()]);
        assert!(!rows.iter().find(|r| r.key == "sodium").unwrap().required);
        assert!(needs_optional(&rows, &files).is_empty());

        // Marked optional before (by hand): kept, but flagged.
        let mut prev_kff = fe("mods/kff-all.jar", "k", None, Some(false));
        prev_kff.required = Some(false);
        let prev = man(vec![prev_kff]);
        let rows = resolve(&m, &files, Some(&prev), Some(&export()), &BTreeMap::new());
        let need = needs_optional(&rows, &files);
        assert_eq!(need.len(), 1, "{need:?}");
        assert_eq!(need[0].path, "mods/create-6.0.10.jar");
        assert_eq!(need[0].dependency, "kotlinforforge");
        assert_eq!(need[0].provider_keys, vec!["mods/kff-all.jar".to_string()]);
    }

    #[test]
    fn without_an_export_unknown_mods_are_mandatory() {
        let m = man(vec![
            fe("mods/a.jar", "a", Some("a"), None),
            fe("mods/lib.jar", "l", None, None),
        ]);
        let prev = man(vec![fe("mods/lib.jar", "l", None, Some(false))]);
        let rows = resolve(&m, &[], Some(&prev), None, &BTreeMap::new());
        assert_eq!(
            (rows[0].required, rows[0].source),
            (DEFAULT_REQUIRED, MarkSource::Default)
        );
        assert_eq!(
            rows[1].key, "mods/lib.jar",
            "libraries without modId go by path"
        );
        assert_eq!(
            (rows[1].required, rows[1].source),
            (false, MarkSource::Previous)
        );
        assert!(rows.iter().all(|r| r.suggested.is_none()));

        let mut m = m;
        m.files.push(FileEntry {
            category: Category::Resourcepack,
            ..fe("resourcepacks/p.zip", "p", None, None)
        });
        apply(&mut m, &rows);
        let req: Vec<Option<bool>> = m.files.iter().map(|f| f.required).collect();
        assert_eq!(
            req,
            vec![Some(true), Some(false), None],
            "packs stay unmarked"
        );
    }

    #[test]
    fn the_check_finds_what_keeps_players_out() {
        let mut m = man(vec![
            fe("mods/create-6.0.11.jar", "new", Some("create"), None),
            fe("mods/cosy.jar", "s2", Some("cosycritters"), None),
            fe("mods/sodium.jar", "s9", Some("sodium"), None),
        ]);
        m.loader.loader_version = "21.1.222".into();
        let c = check(&m, &export());
        assert!(c.complete);
        let missing: Vec<&str> = c
            .missing_in_pack
            .iter()
            .map(|j| j.server_file.as_str())
            .collect();
        assert_eq!(
            missing,
            vec!["polymorph.jar"],
            "required, not in the pack; jei/lagscope are not needed"
        );
        assert_eq!(c.version_mismatch.len(), 1);
        assert_eq!(c.version_mismatch[0].server_file, "create-6.0.10.jar");
        assert_eq!(c.not_on_server, vec!["mods/sodium.jar".to_string()]);
        assert!(c.loader_mismatch.as_deref().unwrap().contains("21.1.234"));
    }
}
