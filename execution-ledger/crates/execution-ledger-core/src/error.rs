//! Domain error type shared across API, worker, and scheduler crates.
//!
//! Every crate maps its own infrastructure errors (sqlx, reqwest, drasi-lib)
//! into this type at the boundary, per the error-handling instructions:
//! callers see one typed error, never a leaked infrastructure error.

#[derive(Debug, thiserror::Error)]
pub enum ExecutionLedgerError {
    #[error("configuration value `{name}` is missing or invalid")]
    InvalidConfiguration { name: String },

    #[error("dependency `{dependency}` failed: {source}")]
    DependencyFailed {
        dependency: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },

    #[error("request not authenticated: {reason}")]
    Unauthenticated { reason: &'static str },

    #[error("tenant `{tenant_id}` is not authorized")]
    TenantNotAuthorized { tenant_id: uuid::Uuid },

    #[error("resource conflict: {resource}")]
    Conflict { resource: &'static str },

    #[error("payload too large: {resource}")]
    PayloadTooLarge { resource: &'static str },

    #[error("step `{step_id}` {direction} failed schema validation: {errors}")]
    SchemaValidation {
        step_id: String,
        direction: &'static str,
        errors: String,
    },
}

pub type Result<T> = std::result::Result<T, ExecutionLedgerError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_configuration_error_names_the_missing_field() {
        let err = ExecutionLedgerError::InvalidConfiguration {
            name: "DATABASE_URL".to_string(),
        };

        assert_eq!(
            err.to_string(),
            "configuration value `DATABASE_URL` is missing or invalid"
        );
    }
}
