//! Platform metadata handlers: route definitions.
//!
//! `/platform/definitions` requires `platform:read` scope (enforced by router layer).

use axum::Json;
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RouteDefinition {
    id: &'static str,
    method: &'static str,
    path: &'static str,
}

pub(crate) async fn platform_definitions() -> Json<Vec<RouteDefinition>> {
    Json(vec![
        RouteDefinition {
            id: "platform.health",
            method: "GET",
            path: "/platform/health",
        },
        RouteDefinition {
            id: "platform.definitions",
            method: "GET",
            path: "/platform/definitions",
        },
        RouteDefinition {
            id: "platform.jobs.trigger",
            method: "POST",
            path: "/platform/jobs/trigger",
        },
        RouteDefinition {
            id: "platform.jobs.get",
            method: "GET",
            path: "/jobs/{id}",
        },
        RouteDefinition {
            id: "platform.jobs.list",
            method: "GET",
            path: "/platform/jobs",
        },
        RouteDefinition {
            id: "platform.jobs.steps",
            method: "GET",
            path: "/platform/jobs/{id}/steps",
        },
        RouteDefinition {
            id: "platform.jobs.replay",
            method: "POST",
            path: "/platform/jobs/{id}/replay",
        },
        RouteDefinition {
            id: "platform.jobs.skip",
            method: "POST",
            path: "/platform/jobs/{id}/skip",
        },
        RouteDefinition {
            id: "platform.jobs.compensate",
            method: "POST",
            path: "/platform/jobs/{id}/compensate",
        },
        RouteDefinition {
            id: "platform.gc",
            method: "POST",
            path: "/platform/gc",
        },
        RouteDefinition {
            id: "platform.recovery.runs",
            method: "POST",
            path: "/platform/recovery/runs",
        },
        RouteDefinition {
            id: "platform.recovery.schedules",
            method: "POST",
            path: "/platform/recovery/schedules",
        },
        RouteDefinition {
            id: "platform.recovery.localPoison",
            method: "POST",
            path: "/platform/recovery/local-poison",
        },
        RouteDefinition {
            id: "platform.recovery.serviceBusPoison",
            method: "POST",
            path: "/platform/recovery/servicebus-poison",
        },
    ])
}
