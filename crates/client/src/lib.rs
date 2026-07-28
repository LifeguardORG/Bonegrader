//! Client-side execution of an update.
//!
//! [`exec::finalize`] turns a [`bonegrader_core::diff::UpdatePlan`] plus the
//! user's answers (which extras to remove, which collisions to keep) into a
//! concrete [`exec::ExecutionPlan`]. [`apply::apply`] then executes it against
//! the instance, honouring the safety invariants: verify every download before
//! anything destructive happens, back up replaced/removed files instead of
//! hard-deleting, and only ever touch validated managed paths.
//!
//! Networking is injected via the [`apply::Fetcher`] trait so this crate — and
//! its tests — stay free of any HTTP dependency.
#![forbid(unsafe_code)]

pub mod apply;
pub mod detect;
pub mod exec;
#[cfg(feature = "http")]
pub mod http;
pub mod install;
