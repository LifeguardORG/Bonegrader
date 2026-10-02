//! Client-side execution of an update.
//!
//! [`session::Updater`] is the pipeline both front-ends use: fetch and verify
//! the manifest, scan the instance, plan ([`bonegrader_core::diff`]), then
//! [`exec::finalize`] the player's answers into an [`exec::ExecutionPlan`] that
//! [`apply::apply_with_progress`] executes transactionally — every download is
//! verified before anything changes, replaced/removed files go to a backup, a
//! failure rolls everything back, and the last update can be undone.
//!
//! Networking is injected via the [`fetch::Fetcher`] trait so this crate — and
//! its tests — stay free of any HTTP dependency (see the `http` feature).
#![forbid(unsafe_code)]

use bonegrader_core::manifest::Category;

pub mod apply;
pub mod detect;
pub mod errors;
pub mod exec;
pub mod fetch;
#[cfg(feature = "http")]
pub mod http;
pub mod install;
pub mod servers;
pub mod session;

/// The folders Bonegrader manages.
pub const CATEGORIES: [Category; 3] = [Category::Mod, Category::Resourcepack, Category::Shaderpack];
