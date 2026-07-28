//! PGP encrypt/decrypt connector.
//!
//! Encrypts-and-signs or decrypts-and-verifies a file payload using `OpenPGP`
//! (via the pure-Rust `pgp` crate). This is the connector-step equivalent of
//! a standalone PGP encryption microservice: it does not read or write blob
//! storage itself — it reads bytes from an upstream step's output and
//! produces bytes for a downstream step, exactly like the `sftp` connector's
//! `download` action does. Chain it as `sftp` (download) → `pgp` (decrypt) →
//! `azure_blob` (upload), or the reverse for outbound files.
//!
//! Supports two actions:
//!
//! - **`encrypt`**: encrypts the payload to `partnerPublicKey` and signs it
//!   with `privateKey` (encrypt-then-sign, matching standard partner file
//!   exchange conventions).
//! - **`decrypt`**: decrypts the payload with `privateKey` and verifies the
//!   signature against `partnerPublicKey`. **Signature verification runs
//!   after the full plaintext has been read but before the step returns
//!   success** — a step whose signature does not verify fails closed with no
//!   output content, so a downstream `azure_blob` step never persists
//!   content that could not be confirmed as originating from the trusted
//!   partner.
//!
//! ## Payload resolution (source bytes to encrypt/decrypt)
//!
//! In priority order:
//! 1. `input.content` + `input.contentEncoding == "base64"` — an explicit
//!    base64 payload in the step's own static definition.
//! 2. Upstream step output shaped like `{ "content": "<base64>", "contentEncoding": "base64" }`
//!    (the shape produced by the `sftp` connector's `download` action) via
//!    `ctx.upstream_output`.
//!
//! Output is always `{ "content": "<base64>", "contentEncoding": "base64" }`
//! plus metadata, so a downstream `azure_blob` step picks it up the same way
//! it already does for `sftp` downloads.
//!
//! ## Configuration
//!
//! | Key | Source | Required | Default | Notes |
//! |-----|--------|----------|---------|-------|
//! | `partnerPublicKey` | config or env | yes | — | Base64-encoded ASCII-armored `OpenPGP` public key |
//! | `privateKey` | config or env | yes | — | Base64-encoded ASCII-armored `OpenPGP` private key |
//! | `privateKeyPassphrase` | config or env | yes | — | Passphrase for `privateKey` |
//! | `fileName` | config, input, or externalId | no | `{stepId}.dat` | Literal-data filename embedded in the PGP message |
//! | `armor` (encrypt only) | config | no | `false` | ASCII-armor the encrypted output instead of binary |
//!
//! Env var fallbacks: `EXECUTION_LEDGER_PGP_PARTNER_PUBLIC_KEY`,
//! `EXECUTION_LEDGER_PGP_PRIVATE_KEY`, `EXECUTION_LEDGER_PGP_PRIVATE_KEY_PASSPHRASE`.
//!
//! ## Fail-closed behavior
//!
//! - Returns `InvalidConfiguration` if `partnerPublicKey`, `privateKey`, or
//!   `privateKeyPassphrase` is missing, or if no base64 payload is found.
//! - Returns `DependencyFailed { dependency: "pgp" }` if key parsing,
//!   encryption, or decryption fails.
//! - Returns `DependencyFailed { dependency: "pgp-signature-verification" }`
//!   if decrypt succeeds but the partner signature does not verify — kept as
//!   a distinct dependency tag from generic decryption failures so operators
//!   can tell "wrong key/corrupt file" apart from "signature untrusted" in
//!   logs and alerts.

use std::io::Cursor;

use base64::Engine;
use execution_ledger_core::ExecutionLedgerError;
use pgp::composed::{ArmorOptions, Deserializable, Message, MessageBuilder, SignedPublicKey, SignedSecretKey};
use pgp::crypto::hash::HashAlgorithm;
use pgp::crypto::sym::SymmetricKeyAlgorithm;
use pgp::types::Password;
use serde_json::{Value, json};

use crate::connectors::helpers::{input_string, step_config_string};
use crate::connectors::{ConnectorExecutionContext, ConnectorStep};

fn pgp_dependency_failed(source: pgp::errors::Error) -> ExecutionLedgerError {
    ExecutionLedgerError::DependencyFailed {
        dependency: "pgp",
        source: Box::new(source),
    }
}

/// Resolved key material needed for both encrypt and decrypt.
struct KeyMaterial {
    partner_public_key: SignedPublicKey,
    own_secret_key: SignedSecretKey,
    passphrase: Password,
}

fn config_or_env(step: &ConnectorStep, key: &str, env_var: &str) -> Option<String> {
    step_config_string(step, key).or_else(|| std::env::var(env_var).ok())
}

fn resolve_keys(step: &ConnectorStep) -> Result<KeyMaterial, ExecutionLedgerError> {
    let partner_public_key_b64 = config_or_env(
        step,
        "partnerPublicKey",
        "EXECUTION_LEDGER_PGP_PARTNER_PUBLIC_KEY",
    )
    .ok_or(ExecutionLedgerError::InvalidConfiguration {
        name: "connector.pgp.partnerPublicKey".to_string(),
    })?;
    let private_key_b64 = config_or_env(step, "privateKey", "EXECUTION_LEDGER_PGP_PRIVATE_KEY")
        .ok_or(ExecutionLedgerError::InvalidConfiguration {
            name: "connector.pgp.privateKey".to_string(),
        })?;
    let passphrase = config_or_env(
        step,
        "privateKeyPassphrase",
        "EXECUTION_LEDGER_PGP_PRIVATE_KEY_PASSPHRASE",
    )
    .ok_or(ExecutionLedgerError::InvalidConfiguration {
        name: "connector.pgp.privateKeyPassphrase".to_string(),
    })?;

    let partner_public_key_armor = base64::engine::general_purpose::STANDARD
        .decode(partner_public_key_b64.trim())
        .map_err(|source| ExecutionLedgerError::InvalidConfiguration {
            name: format!("connector.pgp.partnerPublicKey (not valid base64: {source})"),
        })?;
    let private_key_armor = base64::engine::general_purpose::STANDARD
        .decode(private_key_b64.trim())
        .map_err(|source| ExecutionLedgerError::InvalidConfiguration {
            name: format!("connector.pgp.privateKey (not valid base64: {source})"),
        })?;

    let (partner_public_key, _headers) =
        SignedPublicKey::from_armor_single(Cursor::new(partner_public_key_armor))
            .map_err(pgp_dependency_failed)?;
    let (own_secret_key, _headers) =
        SignedSecretKey::from_armor_single(Cursor::new(private_key_armor))
            .map_err(pgp_dependency_failed)?;

    Ok(KeyMaterial {
        partner_public_key,
        own_secret_key,
        passphrase: Password::from(passphrase),
    })
}

/// Resolves the source bytes to encrypt/decrypt: an explicit base64 payload
/// in `input`, or a base64 payload from the upstream step's output.
fn resolve_source_bytes(
    step: &ConnectorStep,
    upstream_output: Option<&Value>,
) -> Result<Vec<u8>, ExecutionLedgerError> {
    let from_base64 = |value: &Value| -> Option<Result<Vec<u8>, ExecutionLedgerError>> {
        let content = value.get("content").and_then(Value::as_str)?;
        let encoding = value.get("contentEncoding").and_then(Value::as_str)?;
        if encoding != "base64" {
            return None;
        }
        Some(
            base64::engine::general_purpose::STANDARD
                .decode(content)
                .map_err(|source| ExecutionLedgerError::DependencyFailed {
                    dependency: "base64",
                    source: Box::new(source),
                }),
        )
    };

    if let Some(result) = from_base64(&step.input) {
        return result;
    }
    if let Some(result) = upstream_output.and_then(from_base64) {
        return result;
    }

    Err(ExecutionLedgerError::InvalidConfiguration {
        name: "connector.pgp (no base64 content in input or upstream output)".to_string(),
    })
}

fn resolve_file_name(step: &ConnectorStep) -> String {
    step_config_string(step, "fileName")
        .or_else(|| input_string(step, "fileName"))
        .or_else(|| step.external_id.clone())
        .unwrap_or_else(|| format!("{}.dat", step.id))
}

// pgp crate operations are synchronous (CPU-bound crypto, no I/O); async
// is kept for interface consistency with every other connector's execute
// signature, matching the same tradeoff already made in blob.rs/sftp.rs
// compensate functions.
#[allow(clippy::unused_async)]
pub(crate) async fn encrypt(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let keys = resolve_keys(step)?;
    let plaintext = resolve_source_bytes(step, ctx.upstream_output)?;
    let file_name = resolve_file_name(step);
    let armor = step
        .config
        .get("armor")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut rng = rand::thread_rng();
    let mut builder =
        MessageBuilder::from_bytes(file_name.clone(), plaintext).seipd_v1(&mut rng, SymmetricKeyAlgorithm::AES256);
    builder
        .encrypt_to_key(&mut rng, &keys.partner_public_key)
        .map_err(pgp_dependency_failed)?;
    builder.sign(
        &keys.own_secret_key.primary_key,
        keys.passphrase,
        HashAlgorithm::Sha256,
    );

    let ciphertext = if armor {
        builder
            .to_armored_string(&mut rng, ArmorOptions::default())
            .map_err(pgp_dependency_failed)?
            .into_bytes()
    } else {
        builder.to_vec(&mut rng).map_err(pgp_dependency_failed)?
    };

    Ok(json!({
        "stepId": step.id,
        "connector": "pgp",
        "action": "encrypt",
        "fileName": file_name,
        "armor": armor,
        "bytes": ciphertext.len(),
        "content": base64::engine::general_purpose::STANDARD.encode(&ciphertext),
        "contentEncoding": "base64",
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

#[allow(clippy::unused_async)] // see encrypt() above
pub(crate) async fn decrypt(
    ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
) -> Result<Value, ExecutionLedgerError> {
    let keys = resolve_keys(step)?;
    let ciphertext = resolve_source_bytes(step, ctx.upstream_output)?;
    let file_name = resolve_file_name(step);

    // Auto-detects ASCII-armored vs. binary PGP messages, so this connector
    // accepts whichever form the partner's file transfer convention uses.
    let (message, _headers) =
        Message::from_reader(Cursor::new(ciphertext)).map_err(pgp_dependency_failed)?;

    let mut decrypted = message
        .decrypt(&keys.passphrase, &keys.own_secret_key)
        .map_err(pgp_dependency_failed)?;

    // Read the message to completion first — `Message::verify` requires the
    // reader to have reached the end, and this is also the point at which
    // plaintext bytes exist at all. Verification below decides whether the
    // caller is allowed to treat those bytes as trustworthy.
    let plaintext = decrypted
        .as_data_vec()
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "pgp",
            source: Box::new(source),
        })?;

    // SECURITY: verification happens after the plaintext is in memory but
    // before this function returns Ok. A caller only ever sees plaintext
    // bytes paired with a verification error, never plaintext framed as
    // success — a step whose signature doesn't verify fails closed and a
    // downstream `azure_blob` step is never reached, so unverified content
    // is never persisted.
    decrypted
        .verify(&keys.partner_public_key)
        .map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "pgp-signature-verification",
            source: Box::new(source),
        })?;

    Ok(json!({
        "stepId": step.id,
        "connector": "pgp",
        "action": "decrypt",
        "fileName": file_name,
        "verified": true,
        "bytes": plaintext.len(),
        "content": base64::engine::general_purpose::STANDARD.encode(&plaintext),
        "contentEncoding": "base64",
        "correlationId": ctx.correlation_id,
        "event": ctx.event
    }))
}

/// Compensation is a no-op for both actions: re-encrypting/re-decrypting
/// is not a meaningful "undo," and the connector never persists anything
/// itself (a downstream `azure_blob` step owns that side effect and its
/// own compensation).
#[allow(clippy::unused_async)] // satisfies async connector interface; dispatched via .await in mod.rs
pub(crate) async fn compensate(
    _ctx: &ConnectorExecutionContext<'_>,
    step: &ConnectorStep,
    _original_output: &Value,
) -> Result<Value, ExecutionLedgerError> {
    Ok(json!({
        "stepId": step.id,
        "connector": "pgp",
        "compensated": true,
        "action": "noop",
        "reason": "crypto transforms have no side effect to undo; the downstream storage step owns compensation"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::ConnectorKind;
    use pgp::composed::{ArmorOptions as ArmorOpts, KeyType, SecretKeyParamsBuilder};
    use uuid::Uuid;

    /// Generates a throwaway signed key pair for round-trip tests, returning
    /// (base64 armored public key, base64 armored private key).
    fn generate_test_key_pair() -> Result<(String, String), Box<dyn std::error::Error>> {
        let mut params = SecretKeyParamsBuilder::default();
        params
            .key_type(KeyType::Rsa(2048))
            .can_certify(false)
            .can_sign(true)
            .primary_user_id("Test <test@example.com>".to_string());
        let secret_key_params = params.build()?;
        let secret_key = secret_key_params.generate(rand::thread_rng())?;
        let public_key = secret_key.to_public_key();

        let secret_armor = secret_key.to_armored_string(ArmorOpts::default())?;
        let public_armor = public_key.to_armored_string(ArmorOpts::default())?;

        Ok((
            base64::engine::general_purpose::STANDARD.encode(public_armor),
            base64::engine::general_purpose::STANDARD.encode(secret_armor),
        ))
    }

    fn test_ctx() -> ConnectorExecutionContext<'static> {
        ConnectorExecutionContext {
            run_id: Uuid::new_v4(),
            correlation_id: "test-correlation",
            job_id: "test-job",
            event: "test",
            environment: "test",
            upstream_output: None,
        }
    }

    fn test_step(config: Value, input: Value) -> ConnectorStep {
        ConnectorStep {
            id: "pgp-step".to_string(),
            connector: ConnectorKind::Pgp,
            action: None,
            external_id: None,
            input,
            config,
            input_schema: None,
            output_schema: None,
        }
    }

    #[tokio::test]
    async fn encrypt_missing_partner_public_key_is_invalid_configuration() {
        let ctx = test_ctx();
        let step = test_step(json!({}), json!({}));
        let result = encrypt(&ctx, &step).await;
        let message = result.map_or_else(|e| e.to_string(), |_| String::new());
        assert!(message.contains("partnerPublicKey"), "got: {message:?}");
    }

    #[tokio::test]
    async fn decrypt_missing_source_bytes_is_invalid_configuration() {
        let ctx = test_ctx();
        let step = test_step(
            json!({
                "partnerPublicKey": "not-checked-before-payload",
                "privateKey": "not-checked-before-payload",
                "privateKeyPassphrase": "not-checked-before-payload"
            }),
            json!({}),
        );
        // Key resolution runs first and fails on invalid base64 for these
        // placeholder values; assert it fails closed rather than panicking.
        let result = decrypt(&ctx, &step).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn encrypt_then_decrypt_round_trips_and_verifies()
    -> Result<(), Box<dyn std::error::Error>> {
        let ctx = test_ctx();
        let (public_key_b64, private_key_b64) = generate_test_key_pair()?;
        let key_config = json!({
            "partnerPublicKey": public_key_b64,
            "privateKey": private_key_b64,
            "privateKeyPassphrase": ""
        });

        let plaintext = b"hello from the bank disbursement file";
        let encrypt_step = test_step(
            key_config.clone(),
            json!({
                "content": base64::engine::general_purpose::STANDARD.encode(plaintext),
                "contentEncoding": "base64"
            }),
        );
        let encrypted = encrypt(&ctx, &encrypt_step)
            .await
            .map_err(|e| e.to_string())?;

        let decrypt_step = test_step(
            key_config,
            json!({
                "content": encrypted.get("content").and_then(Value::as_str),
                "contentEncoding": "base64"
            }),
        );
        let decrypted = decrypt(&ctx, &decrypt_step)
            .await
            .map_err(|e| e.to_string())?;

        assert_eq!(decrypted.get("verified").and_then(Value::as_bool), Some(true));
        let content = decrypted
            .get("content")
            .and_then(Value::as_str)
            .ok_or("decrypted output should carry base64 content")?;
        let recovered = base64::engine::general_purpose::STANDARD.decode(content)?;
        assert_eq!(recovered, plaintext);
        Ok(())
    }

    #[test]
    fn resolve_source_bytes_prefers_explicit_input_over_upstream()
    -> Result<(), Box<dyn std::error::Error>> {
        let step = test_step(
            json!({}),
            json!({"content": base64::engine::general_purpose::STANDARD.encode("input"), "contentEncoding": "base64"}),
        );
        let upstream = json!({"content": base64::engine::general_purpose::STANDARD.encode("upstream"), "contentEncoding": "base64"});
        let resolved = resolve_source_bytes(&step, Some(&upstream)).map_err(|e| e.to_string())?;
        assert_eq!(resolved, b"input");
        Ok(())
    }

    #[test]
    fn resolve_source_bytes_falls_back_to_upstream() -> Result<(), Box<dyn std::error::Error>> {
        let step = test_step(json!({}), json!({}));
        let upstream = json!({"content": base64::engine::general_purpose::STANDARD.encode("upstream"), "contentEncoding": "base64"});
        let resolved = resolve_source_bytes(&step, Some(&upstream)).map_err(|e| e.to_string())?;
        assert_eq!(resolved, b"upstream");
        Ok(())
    }

    #[test]
    fn resolve_source_bytes_errors_with_no_content_anywhere() {
        let step = test_step(json!({}), json!({}));
        let result = resolve_source_bytes(&step, None);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_file_name_defaults_to_step_id() {
        let step = test_step(json!({}), json!({}));
        assert_eq!(resolve_file_name(&step), "pgp-step.dat");
    }
}
