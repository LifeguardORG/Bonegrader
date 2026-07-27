//! Per-instance client state: the record of which files Bonegrader manages.
//!
//! Anything present in a managed folder but *not* recorded here is treated as
//! the player's own file and is never touched without explicit consent.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_manifest_generated_at: Option<String>,
    /// Map of client-relative path -> managed metadata.
    #[serde(default)]
    pub managed: BTreeMap<String, ManagedEntry>,
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
