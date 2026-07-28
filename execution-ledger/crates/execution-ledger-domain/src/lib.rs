//! Domain services, mirroring `packages/business/src/services`.
//!
//! - `connectors`: connector-step execution layer (each connector in its own module).
//! - `platform`: platform health and readiness checks.

pub mod connectors;
pub mod platform;
