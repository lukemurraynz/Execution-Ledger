//! Cross-runtime primitives shared by every Execution Ledger service.
//!
//! Configuration loading, authentication, database bootstrap, error types,
//! observability, offload, and queue abstraction. Runtime-specific code
//! (API, worker, scheduler) lives in its own crate, not here.

pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod intent;
pub mod observability;
pub mod offload;
pub mod queue;
pub mod vault;

pub use auth::{AuthenticatedTenant, EntraJwtValidator};
pub use config::{ExecutionLedgerConfig, QueueEngine};
pub use error::ExecutionLedgerError;
pub use execution_ledger_types::{
    EnvelopeValidationError, ExecutionLedgerIntent, ExecutionLedgerIntentSource,
    ExecutionLedgerMessage, ExecutionLedgerMessageMeta,
};
pub use intent::{build_intent, looks_like_trace_id};
