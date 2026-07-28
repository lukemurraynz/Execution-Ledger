//! Platform domain services — the `platform.health` job seen in
//! `package.json`'s `job:launcher:platform_hello` script.

use std::collections::HashMap;

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HealthReport {
    pub status: &'static str,
    pub environment: String,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub services: HashMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub degraded_reasons: Vec<String>,
}

/// Builds the health report for the given environment name.
///
/// Pure function: no I/O, so it needs no async and no error path yet. Wire
/// in real dependency checks (database ping, queue connectivity) only once
/// this job needs to report on them.
#[must_use]
pub fn health_report(environment: &str) -> HealthReport {
    HealthReport {
        status: "ok",
        environment: environment.to_string(),
        services: HashMap::new(),
        degraded_reasons: Vec::new(),
    }
}

/// Builds a degraded health report with explicit service states and reasons.
///
/// Used when the runtime detects partial degradation (e.g., Drasi skipped but
/// queue processing active) so operators can see which services are affected
/// without digging into logs.
#[must_use]
#[allow(clippy::implicit_hasher)] // callers always use default hasher
pub fn degraded_health_report(
    environment: &str,
    services: HashMap<String, String>,
    degraded_reasons: Vec<String>,
) -> HealthReport {
    HealthReport {
        status: if degraded_reasons.is_empty() {
            "ok"
        } else {
            "degraded"
        },
        environment: environment.to_string(),
        services,
        degraded_reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_ok_status_for_given_environment() {
        let report = health_report("local");

        assert_eq!(
            report,
            HealthReport {
                status: "ok",
                environment: "local".to_string(),
                services: HashMap::new(),
                degraded_reasons: Vec::new(),
            }
        );
    }

    #[test]
    fn degraded_report_has_degraded_status() {
        let mut services = HashMap::new();
        services.insert("drasi".to_string(), "skipped".to_string());
        services.insert("queue".to_string(), "active".to_string());

        let report = degraded_health_report(
            "local",
            services.clone(),
            vec!["drasi: replication slot owned by another replica".to_string()],
        );

        assert_eq!(report.status, "degraded");
        assert_eq!(report.services, services);
        assert_eq!(report.degraded_reasons.len(), 1);
    }

    #[test]
    fn degraded_report_with_empty_reasons_is_ok() {
        let report = degraded_health_report("local", HashMap::new(), Vec::new());
        assert_eq!(report.status, "ok");
    }
}
