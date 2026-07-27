//! Bonegrader shared core.
//!
//! This crate is deliberately free of any I/O policy decisions (no networking,
//! no destructive filesystem operations). It provides:
//!
//! * [`manifest`] — the on-server manifest data model (source of truth).
//! * [`state`] — the per-instance client state (what Bonegrader manages).
//! * [`hash`] — SHA-1 hashing of files/bytes (the discrepancy check).
//! * [`modinfo`] — extracting a stable mod identity (`modId`) from a jar.
//! * [`scan`] — reading the managed folders of a local instance.
//! * [`diff`] — the pure, deterministic update planner (the heart of the app).
//!
//! The [`diff::compute_plan`] function only *plans* actions; applying them
//! (downloading, moving, deleting) is the caller's job and must honour the
//! safety invariants documented on [`diff::UpdatePlan`].
#![forbid(unsafe_code)]

pub mod diff;
pub mod hash;
pub mod manifest;
pub mod modinfo;
pub mod paths;
pub mod scan;
pub mod state;
