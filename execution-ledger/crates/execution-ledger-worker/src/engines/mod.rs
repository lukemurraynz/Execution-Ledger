//! Queue engine implementations for the intent worker.
//!
//! - `local`: in-database queue using `FOR UPDATE SKIP LOCKED` on the
//!   `EXECUTION_LEDGER_local_intent_queue` table. Suitable for dev/test and
//!   single-replica deployments.
//! - `servicebus`: Azure Service Bus dequeue with managed identity auth and
//!   message lock renewal. Used in production with KEDA autoscaling.

pub(super) mod local;
pub(super) mod servicebus;
