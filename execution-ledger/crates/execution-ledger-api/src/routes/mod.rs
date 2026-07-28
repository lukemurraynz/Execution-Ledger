//! Route handler modules, one per resource group.
//!
//! Each module owns its request/response types and handler functions.
//! Shared types (like `ListRunsQuery`) live in the parent `main.rs`.

pub(super) mod health;
pub(super) mod jobs;
pub(super) mod platform;
pub(super) mod recovery;
pub(super) mod schedules;
