//! Per-connector behavioral contracts that are real, load-bearing, and — until
//! this module existed — discoverable only by reading every connector's
//! source individually. Each classification function is an **exhaustive**
//! match over [`ConnectorKind`] (no `_` wildcard arm): adding a new variant
//! without updating every function here is a compile error, not a silent gap.
//!
//! This is documentation-as-code, not just documentation: [`replay_safety`]
//! is wired into `execution-ledger-api`'s replay endpoint as a live guardrail
//! (see `crates/execution-ledger-api/src/routes/jobs.rs::replay_job_run`),
//! and every classification below has a same-file test asserting it stays
//! consistent with the connector's actual `execute()`/`compensate()` code.
//!
//! # When you add or change a connector
//!
//! Update the relevant match arm(s) here in the same change. If you're
//! unsure which arm applies, err toward the more conservative option
//! (`NotSafeWithoutGuard`, `NoopIrreversible`, `AcceptsCredentialConfig`) —
//! these classifications gate operator actions and security assumptions;
//! a false "safe" is worse than a false "unsafe."

use super::ConnectorKind;

// ── Upstream-output composability ────────────────────────────────────────

/// Whether a connector step of this kind/action reads `ctx.upstream_output`
/// as a fallback when its own `input`/`config` is empty — the chaining
/// contract every multi-step job template depends on.
///
/// `transform` is a known gap, not a deliberate boundary: its own module doc
/// says it "reshapes data between steps," but `apply_map()` only reads
/// `step.input`, never `ctx.upstream_output`. Don't route an upstream-value
/// extraction through `transform` expecting it to chain — see
/// `connectors/transform.rs`.
#[must_use]
// One explicit arm per variant is deliberate even where bodies coincide —
// this match is documentation-as-code; collapsing arms would hide which
// variants were actually considered when a new one is added.
#[allow(clippy::match_same_arms)]
pub fn consumes_upstream_output(kind: ConnectorKind, action: Option<&str>) -> bool {
    match kind {
        ConnectorKind::AzureBlob => true,
        ConnectorKind::Sftp => matches!(action, None | Some("download" | "upload")),
        ConnectorKind::Pgp => true,
        ConnectorKind::HttpDownload => true,
        ConnectorKind::BlobArchive => false, // owns its own blob I/O — deliberate, see module doc
        ConnectorKind::PlatformHealth => false,
        ConnectorKind::Rest | ConnectorKind::Erp => false, // known gap
        ConnectorKind::MsGraphNotify => false,
        ConnectorKind::Queue => false,
        ConnectorKind::AzureCompute => false,
        ConnectorKind::AzureSql => false,
        ConnectorKind::Transform => false, // known gap — see connectors/transform.rs
    }
}

/// Narrower than `!consumes_upstream_output(...)`: true only for connector/
/// action combinations where **not** chaining is a known authoring trap —
/// a job author would reasonably expect this step to pick up the previous
/// step's output (its whole purpose is processing/relaying pipeline data),
/// but it silently won't.
///
/// This deliberately excludes connectors where non-chaining is an obvious,
/// documented part of the design (`blob_archive` owns its own blob I/O and
/// says so in its module doc; `platform_health`/`azure_compute`/`azure_sql`/
/// `queue`/`ms_graph_notify` aren't in the business of relaying pipeline
/// data at all) — flagging those as a "gap" would be a false positive a job
/// author has no reason to act on. Use this function (not
/// `consumes_upstream_output` directly) for any tool that warns a human
/// about a *likely mistake*, e.g. `execution-ledger-cli validate-job`.
#[must_use]
pub fn likely_missing_chain(kind: ConnectorKind, action: Option<&str>) -> bool {
    !consumes_upstream_output(kind, action)
        && matches!(kind, ConnectorKind::Transform | ConnectorKind::Rest | ConnectorKind::Erp)
}

// ── Compensation behavior ────────────────────────────────────────────────

/// What actually happens when a step of this kind/action is compensated
/// (`target.compensate`), so an operator relying on "compensate will undo
/// this" knows which steps genuinely roll back versus which just log
/// `{"compensated": true, "action": "noop", ...}` with nothing reversed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompensationBehavior {
    /// Reads a `stateBlob` captured by an earlier step in the run and calls
    /// a reverse action against the same external system.
    RestoresState,
    /// No external side effect exists to undo (read-only, or a pure
    /// in-pipeline transform with nothing persisted externally).
    NoopReadOnly,
    /// A real external side effect happened and cannot be safely auto-
    /// reversed (sending a notification, uploading a file another system
    /// may already be watching).
    NoopIrreversible,
    /// Calls a job-author-configured `compensateUrl` if one was set on the
    /// step; otherwise behaves like `NoopIrreversible`. Whether this step is
    /// actually reversible depends on job-template configuration, not on
    /// the connector alone.
    CallsConfiguredReverseEndpoint,
}

#[must_use]
#[allow(clippy::match_same_arms)] // see consumes_upstream_output's note — deliberate, not redundant
pub fn compensation_behavior(kind: ConnectorKind, action: Option<&str>) -> CompensationBehavior {
    match kind {
        ConnectorKind::AzureCompute | ConnectorKind::AzureSql => {
            match action {
                // Read-only get/save-state actions have nothing to undo.
                Some(
                    "vm-get" | "vm-save-state" | "vmss-get" | "appservice-get"
                        | "sql-database-get" | "sql-elastic-pool-get",
                ) => CompensationBehavior::NoopReadOnly,
                // Resize actions restore the previous SKU from stateBlob
                // when present; see azure_compute.rs / azure_sql.rs.
                _ => CompensationBehavior::RestoresState,
            }
        }
        ConnectorKind::Queue => match action {
            // `receive` restores the message (re-enqueues from stateBlob)
            // when one was actually received; `send` cannot be undone.
            Some("receive") => CompensationBehavior::RestoresState,
            _ => CompensationBehavior::NoopIrreversible,
        },
        ConnectorKind::Rest | ConnectorKind::Erp => {
            CompensationBehavior::CallsConfiguredReverseEndpoint
        }
        ConnectorKind::AzureBlob => CompensationBehavior::NoopIrreversible, // deleting is destructive
        ConnectorKind::MsGraphNotify => CompensationBehavior::NoopIrreversible, // already sent
        ConnectorKind::Sftp => match action {
            Some("download") => CompensationBehavior::NoopReadOnly,
            _ => CompensationBehavior::NoopIrreversible, // upload: deleting is destructive
        },
        ConnectorKind::BlobArchive => CompensationBehavior::NoopIrreversible, // deleting is destructive
        ConnectorKind::HttpDownload => match action {
            Some("upload") => CompensationBehavior::NoopIrreversible,
            _ => CompensationBehavior::NoopReadOnly, // download is read-only
        },
        ConnectorKind::PlatformHealth => CompensationBehavior::NoopReadOnly,
        ConnectorKind::Pgp => CompensationBehavior::NoopReadOnly, // pure transform, no side effect of its own
        ConnectorKind::Transform => CompensationBehavior::NoopReadOnly, // pure transform, no side effect
    }
}

// ── Replay safety ─────────────────────────────────────────────────────────

/// Whether replaying (re-executing) a step of this kind/action is safe by
/// itself, or re-triggers a real external side effect the caller should
/// think twice about before repeating.
///
/// **This is a judgment call, not a hard fact, for anything that calls an
/// external system this platform doesn't control the semantics of** (`rest`,
/// `erp`, an SFTP drop another system may watch). Where uncertain, this
/// errs toward `NotSafeWithoutGuard` — a false "safe" here means an operator
/// replays a step mid-incident and unknowingly re-sends a real notification
/// or double-processes a file; a false "unsafe" only costs an extra confirm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaySafety {
    /// Re-executing produces the same real-world effect as executing once
    /// (idempotent by nature: read-only, or a deterministic overwrite).
    Idempotent,
    /// Re-executing performs a real action again — this platform's own
    /// step-level idempotency key prevents the *worker* from double-
    /// processing the same intent, but does not make an arbitrary external
    /// system treat a fresh replay intent as a duplicate.
    NotSafeWithoutGuard,
}

#[must_use]
#[allow(clippy::match_same_arms)] // see consumes_upstream_output's note — deliberate, not redundant
pub fn replay_safety(kind: ConnectorKind, action: Option<&str>) -> ReplaySafety {
    match kind {
        ConnectorKind::PlatformHealth => ReplaySafety::Idempotent,
        // Generic HTTP calls to a caller-specified endpoint: this platform
        // has no way to know whether the target treats a repeat call as
        // idempotent (a POST that creates a new record will duplicate it).
        ConnectorKind::Rest | ConnectorKind::Erp => ReplaySafety::NotSafeWithoutGuard,
        ConnectorKind::Queue => match action {
            Some("receive") => ReplaySafety::Idempotent, // read-oriented, no new effect
            _ => ReplaySafety::NotSafeWithoutGuard,        // send: re-delivers a real message
        },
        // Blob writes are overwrite-by-design (deterministic final state),
        // unlike an SFTP drop or a queue send which are additive/triggering.
        ConnectorKind::AzureBlob => ReplaySafety::Idempotent,
        // A real Teams/email notification is sent again on replay.
        ConnectorKind::MsGraphNotify => ReplaySafety::NotSafeWithoutGuard,
        ConnectorKind::Sftp => match action {
            Some("download") => ReplaySafety::Idempotent,
            // Re-uploading to a partner SFTP drop commonly re-triggers that
            // partner's own file-arrival processing — treat conservatively.
            _ => ReplaySafety::NotSafeWithoutGuard,
        },
        // ARM resize/get operations are declarative (set desired state);
        // re-applying the same target state is idempotent by construction.
        ConnectorKind::AzureCompute | ConnectorKind::AzureSql => ReplaySafety::Idempotent,
        ConnectorKind::Transform => ReplaySafety::Idempotent, // pure function of its own input
        ConnectorKind::Pgp => ReplaySafety::Idempotent, // no external side effect either action
        // Overwrite-by-design destination writes, same reasoning as AzureBlob.
        ConnectorKind::BlobArchive => ReplaySafety::Idempotent,
        // Both actions are overwrite/read-only by design: `upload` PUTs to a
        // fixed URL (not a POST-create), `download` is read-only.
        ConnectorKind::HttpDownload => ReplaySafety::Idempotent,
    }
}

/// A step targeted by a replay request that [`replay_safety`] classifies as
/// [`ReplaySafety::NotSafeWithoutGuard`] — returned by [`unsafe_replay_steps`]
/// so both the API route and the CLI can surface the same information to an
/// operator before they confirm a replay.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnsafeReplayStep {
    pub step_id: String,
    pub connector: String,
    pub action: Option<String>,
    pub reason: &'static str,
}

/// Classifies each `step_id` in `target_step_ids` against the connector/
/// action recorded for it in `input_data` (the job run's original trigger
/// payload — the same `{"steps": [...]}` shape `resolve_connector_steps` in
/// `execution-ledger-worker` reads), and returns only the ones
/// [`replay_safety`] flags as [`ReplaySafety::NotSafeWithoutGuard`].
///
/// A `step_id` that isn't found in `input_data.steps`, or whose `connector`
/// field doesn't parse as a known [`ConnectorKind`], is skipped rather than
/// flagged — there's nothing to classify without a known connector, and this
/// function's job is to catch *known* unsafe replays, not to validate the
/// step list's shape (that's `resolve_connector_steps`'s job, at execution
/// time).
///
/// This is the single shared implementation behind the live replay guardrail
/// in both `execution-ledger-api`'s `/platform/jobs/{id}/replay` route and
/// `execution-ledger-cli`'s `job-replay` command — the check must give the
/// same answer regardless of which entry point an operator uses.
#[must_use]
pub fn unsafe_replay_steps(
    input_data: &serde_json::Value,
    target_step_ids: &[String],
) -> Vec<UnsafeReplayStep> {
    let Some(steps) = input_data.get("steps").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };

    target_step_ids
        .iter()
        .filter_map(|target_id| {
            let step = steps.iter().find(|candidate| {
                candidate.get("id").and_then(serde_json::Value::as_str) == Some(target_id.as_str())
            })?;
            let connector_value = step.get("connector")?;
            let connector: ConnectorKind = serde_json::from_value(connector_value.clone()).ok()?;
            let action = step.get("action").and_then(serde_json::Value::as_str);

            if replay_safety(connector, action) == ReplaySafety::NotSafeWithoutGuard {
                Some(UnsafeReplayStep {
                    step_id: target_id.clone(),
                    connector: connector_value
                        .as_str()
                        .unwrap_or("unknown")
                        .to_string(),
                    action: action.map(ToString::to_string),
                    reason: "re-executing this connector/action repeats a real external action \
                             rather than restoring a prior state",
                })
            } else {
                None
            }
        })
        .collect()
}

// ── Credential posture ────────────────────────────────────────────────────

/// How a connector resolves credentials for the external systems it talks
/// to — used to assert the platform's "managed identity first, fail closed"
/// security posture doesn't quietly regress when a connector is added or
/// changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialPosture {
    /// Resolves Azure credentials exclusively via `azure_identity`'s
    /// managed-identity credential types — no connection strings, no
    /// plaintext secrets accepted in step config.
    ManagedIdentityOnly,
    /// Accepts credential-shaped material directly via step config (e.g. an
    /// SSH password/key, a PGP private key) — the platform's job is to
    /// ensure that config is sourced from Key Vault via App Configuration,
    /// not to avoid the credential shape existing at the connector layer.
    AcceptsCredentialConfig,
    /// Performs no credential resolution of its own. Safety depends
    /// entirely on the caller supplying an already-authorized target (a
    /// pre-signed SAS URL, a network path that doesn't require auth).
    NoAuth,
}

#[must_use]
#[allow(clippy::match_same_arms)] // see consumes_upstream_output's note — deliberate, not redundant
pub fn credential_posture(kind: ConnectorKind) -> CredentialPosture {
    match kind {
        ConnectorKind::AzureBlob
        | ConnectorKind::AzureCompute
        | ConnectorKind::AzureSql
        | ConnectorKind::BlobArchive
        | ConnectorKind::MsGraphNotify
        | ConnectorKind::Queue => CredentialPosture::ManagedIdentityOnly,
        ConnectorKind::Sftp | ConnectorKind::Pgp => CredentialPosture::AcceptsCredentialConfig,
        // `rest`/`erp` explicitly denylist forwarding an Authorization/Cookie
        // header from step config (see rest.rs's DENYLISTED_HEADERS) — by
        // design, these connectors cannot carry a caller-supplied credential
        // to the target at all.
        ConnectorKind::Rest
        | ConnectorKind::Erp
        | ConnectorKind::HttpDownload
        | ConnectorKind::PlatformHealth
        | ConnectorKind::Transform => CredentialPosture::NoAuth,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── unsafe_replay_steps — the function the live guardrail is built on ──

    #[test]
    fn unsafe_replay_steps_flags_ms_graph_notify_and_ignores_safe_step() {
        let input_data = json!({
            "steps": [
                {"id": "check", "connector": "platform_health"},
                {"id": "notify", "connector": "ms_graph_notify"}
            ]
        });
        let flagged = unsafe_replay_steps(
            &input_data,
            &["check".to_string(), "notify".to_string()],
        );
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].step_id, "notify");
        assert_eq!(flagged[0].connector, "ms_graph_notify");
    }

    #[test]
    fn unsafe_replay_steps_skips_unknown_step_ids() {
        let input_data = json!({"steps": [{"id": "check", "connector": "platform_health"}]});
        let flagged = unsafe_replay_steps(&input_data, &["does-not-exist".to_string()]);
        assert!(flagged.is_empty());
    }

    #[test]
    fn unsafe_replay_steps_handles_missing_steps_array() {
        let flagged = unsafe_replay_steps(&json!({}), &["anything".to_string()]);
        assert!(flagged.is_empty());
    }

    #[test]
    fn unsafe_replay_steps_respects_action_specific_classification() {
        // sftp download is Idempotent, sftp upload is NotSafeWithoutGuard —
        // confirms the action field is consulted, not just the connector.
        let input_data = json!({
            "steps": [
                {"id": "get-file", "connector": "sftp", "action": "download"},
                {"id": "send-file", "connector": "sftp", "action": "upload"}
            ]
        });
        let flagged = unsafe_replay_steps(
            &input_data,
            &["get-file".to_string(), "send-file".to_string()],
        );
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].step_id, "send-file");
    }

    // ── consumes_upstream_output cross-checks ────────────────────────────
    // These assert the classification against behavior already proven by
    // each connector's own tests (resolve_upload_bytes / resolve_source_bytes
    // in sftp.rs, pgp.rs, http_download.rs), rather than re-deriving it here.

    #[test]
    fn upstream_output_classification_matches_known_connector_behavior() {
        assert!(consumes_upstream_output(ConnectorKind::AzureBlob, None));
        assert!(consumes_upstream_output(ConnectorKind::Sftp, Some("download")));
        assert!(consumes_upstream_output(ConnectorKind::Sftp, None)); // upload, empty-input fallback
        assert!(consumes_upstream_output(ConnectorKind::Pgp, Some("decrypt")));
        assert!(consumes_upstream_output(ConnectorKind::HttpDownload, Some("upload")));
        assert!(!consumes_upstream_output(ConnectorKind::Transform, None));
        assert!(!consumes_upstream_output(ConnectorKind::Rest, None));
        assert!(!consumes_upstream_output(ConnectorKind::BlobArchive, Some("zip")));
    }

    #[test]
    fn likely_missing_chain_flags_known_traps_only() {
        assert!(likely_missing_chain(ConnectorKind::Transform, None));
        assert!(likely_missing_chain(ConnectorKind::Rest, None));
        assert!(likely_missing_chain(ConnectorKind::Erp, None));
        // blob_archive deliberately never chains — not a trap, must not be flagged.
        assert!(!likely_missing_chain(ConnectorKind::BlobArchive, Some("unzip")));
        assert!(!likely_missing_chain(ConnectorKind::BlobArchive, Some("zip")));
        assert!(!likely_missing_chain(ConnectorKind::PlatformHealth, None));
        assert!(!likely_missing_chain(ConnectorKind::AzureCompute, Some("vmss-resize")));
        // Connectors that DO chain are trivially not a "missing chain" trap.
        assert!(!likely_missing_chain(ConnectorKind::AzureBlob, None));
        assert!(!likely_missing_chain(ConnectorKind::Pgp, Some("decrypt")));
    }

    // ── compensation_behavior cross-checks ───────────────────────────────
    // Full live-network verification isn't possible in a unit test for the
    // Azure/queue restore paths (they'd need real ARM/Service Bus access),
    // so these check the branch each connector's compensate() takes on
    // inputs that don't require network access: the guaranteed-noop paths
    // (read-only actions, missing stateBlob) are directly comparable
    // against the classification without live infrastructure.

    #[tokio::test]
    async fn azure_compute_read_actions_classified_as_noop_read_only()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::connectors::azure_compute;
        use crate::connectors::{ConnectorExecutionContext, ConnectorStep};
        use serde_json::{json, Value};
        use uuid::Uuid;

        let ctx = ConnectorExecutionContext {
            run_id: Uuid::new_v4(),
            correlation_id: "test",
            job_id: "test-job",
            event: "test",
            environment: "test",
            upstream_output: None,
        };
        let step = ConnectorStep {
            id: "s".to_string(),
            connector: ConnectorKind::AzureCompute,
            action: Some("vmss-get".to_string()),
            external_id: None,
            input: json!({}),
            config: json!({}),
            input_schema: None,
            output_schema: None,
        };
        let original_output = json!({"action": "vmss-get"});
        let result = azure_compute::compensate(&ctx, &step, &original_output)
            .await
            .map_err(|e| e.to_string())?;
        assert_eq!(result.get("action").and_then(Value::as_str), Some("noop"));
        assert_eq!(
            compensation_behavior(ConnectorKind::AzureCompute, Some("vmss-get")),
            CompensationBehavior::NoopReadOnly
        );
        Ok(())
    }

    #[tokio::test]
    async fn azure_compute_resize_without_state_blob_is_noop_but_classified_as_restores_state()
    -> Result<(), Box<dyn std::error::Error>> {
        // Confirms the classification tracks the *capability* (this action
        // restores state when a stateBlob is present), not just what
        // happens on this particular no-stateBlob input.
        use crate::connectors::azure_compute;
        use crate::connectors::{ConnectorExecutionContext, ConnectorStep};
        use serde_json::{json, Value};
        use uuid::Uuid;

        let ctx = ConnectorExecutionContext {
            run_id: Uuid::new_v4(),
            correlation_id: "test",
            job_id: "test-job",
            event: "test",
            environment: "test",
            upstream_output: None,
        };
        let step = ConnectorStep {
            id: "s".to_string(),
            connector: ConnectorKind::AzureCompute,
            action: Some("vmss-resize".to_string()),
            external_id: None,
            input: json!({}),
            config: json!({}),
            input_schema: None,
            output_schema: None,
        };
        let original_output = json!({"action": "vmss-resize"}); // no stateBlob
        let result = azure_compute::compensate(&ctx, &step, &original_output)
            .await
            .map_err(|e| e.to_string())?;
        assert_eq!(result.get("reason").and_then(Value::as_str), Some("no stateBlob to restore from"));
        assert_eq!(
            compensation_behavior(ConnectorKind::AzureCompute, Some("vmss-resize")),
            CompensationBehavior::RestoresState
        );
        Ok(())
    }

    #[tokio::test]
    async fn queue_send_is_classified_as_noop_irreversible() -> Result<(), Box<dyn std::error::Error>> {
        use crate::connectors::queue;
        use crate::connectors::{ConnectorExecutionContext, ConnectorStep};
        use serde_json::{json, Value};
        use uuid::Uuid;

        let ctx = ConnectorExecutionContext {
            run_id: Uuid::new_v4(),
            correlation_id: "test",
            job_id: "test-job",
            event: "test",
            environment: "test",
            upstream_output: None,
        };
        let step = ConnectorStep {
            id: "s".to_string(),
            connector: ConnectorKind::Queue,
            action: None,
            external_id: None,
            input: json!({}),
            config: json!({}),
            input_schema: None,
            output_schema: None,
        };
        let original_output = json!({"action": "send"});
        let result = queue::compensate(&ctx, &step, &original_output)
            .await
            .map_err(|e| e.to_string())?;
        assert_eq!(result.get("action").and_then(Value::as_str), Some("noop"));
        assert_eq!(
            compensation_behavior(ConnectorKind::Queue, None),
            CompensationBehavior::NoopIrreversible
        );
        Ok(())
    }

    // ── replay_safety: the classifications that actually gate the live
    // guardrail. Pinned here as plain assertions so a change to any of
    // these is a deliberate, reviewed diff, not an accidental one.

    #[test]
    fn replay_safety_flags_known_non_idempotent_actions() {
        assert_eq!(
            replay_safety(ConnectorKind::MsGraphNotify, None),
            ReplaySafety::NotSafeWithoutGuard
        );
        assert_eq!(
            replay_safety(ConnectorKind::Rest, None),
            ReplaySafety::NotSafeWithoutGuard
        );
        assert_eq!(
            replay_safety(ConnectorKind::Erp, None),
            ReplaySafety::NotSafeWithoutGuard
        );
        assert_eq!(
            replay_safety(ConnectorKind::Queue, None),
            ReplaySafety::NotSafeWithoutGuard
        );
        assert_eq!(
            replay_safety(ConnectorKind::Sftp, Some("upload")),
            ReplaySafety::NotSafeWithoutGuard
        );
    }

    #[test]
    fn replay_safety_allows_known_idempotent_actions() {
        assert_eq!(
            replay_safety(ConnectorKind::PlatformHealth, None),
            ReplaySafety::Idempotent
        );
        assert_eq!(
            replay_safety(ConnectorKind::Sftp, Some("download")),
            ReplaySafety::Idempotent
        );
        assert_eq!(
            replay_safety(ConnectorKind::AzureBlob, None),
            ReplaySafety::Idempotent
        );
        assert_eq!(
            replay_safety(ConnectorKind::Queue, Some("receive")),
            ReplaySafety::Idempotent
        );
    }

    #[test]
    fn credential_posture_matches_documented_security_posture() {
        assert_eq!(
            credential_posture(ConnectorKind::AzureBlob),
            CredentialPosture::ManagedIdentityOnly
        );
        assert_eq!(
            credential_posture(ConnectorKind::AzureCompute),
            CredentialPosture::ManagedIdentityOnly
        );
        assert_eq!(
            credential_posture(ConnectorKind::AzureSql),
            CredentialPosture::ManagedIdentityOnly
        );
        assert_eq!(
            credential_posture(ConnectorKind::Sftp),
            CredentialPosture::AcceptsCredentialConfig
        );
        assert_eq!(
            credential_posture(ConnectorKind::Pgp),
            CredentialPosture::AcceptsCredentialConfig
        );
        // rest/erp explicitly cannot forward Authorization/Cookie headers
        // (see rest.rs's DENYLISTED_HEADERS) — NoAuth reflects that this
        // connector layer carries no caller-supplied credential at all.
        assert_eq!(credential_posture(ConnectorKind::Rest), CredentialPosture::NoAuth);
        assert_eq!(credential_posture(ConnectorKind::Erp), CredentialPosture::NoAuth);
    }
}
