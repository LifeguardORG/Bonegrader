//! Per-instance client state: the record of which files Bonegrader manages.
//!
//! Anything present in a managed folder but *not* recorded here is treated as
//! the player's own file and is never touched without explicit consent.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A file Bonegrader has installed or adopted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedEntry {
    pub sha1: String,
    /// Mod ids declared by the jar (empty for packs / metadata-less libs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mod_ids: Vec<String>,
}

/// The full client state for one bound instance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientState {
    pub instance_path: String,
    pub launcher_type: String,
    pub channel: String,
    /// Pack this instance was last updated to (lets a switch to a different
    /// pack be flagged before anything is replaced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_manifest_generated_at: Option<String>,
    /// Map of client-relative path -> managed metadata.
    #[serde(default)]
    pub managed: BTreeMap<String, ManagedEntry>,
    /// Seed files already placed (or found present) once. They are never
    /// touched again, so a player's edits or deletions stick.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub seeded: BTreeSet<String>,
    /// Server address Bonegrader added to `servers.dat`; it is only added
    /// again if the pack's address changes, so a player's removal sticks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_added: Option<String>,
}

impl ClientState {
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// True on the very first run against an instance (nothing adopted yet).
    pub fn is_first_run(&self) -> bool {
        self.managed.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_files_from_older_versions_still_load() {
        let old = r#"{"instancePath":"/x","launcherType":"manual","channel":"main",
            "managed":{"mods/a.jar":{"sha1":"abc","modIds":["a"]}}}"#;
        let st = ClientState::from_json(old).unwrap();
        assert!(st.seeded.is_empty() && st.server_added.is_none() && st.pack_name.is_none());
        assert_eq!(st.managed["mods/a.jar"].mod_ids, vec!["a".to_string()]);
    }
}
