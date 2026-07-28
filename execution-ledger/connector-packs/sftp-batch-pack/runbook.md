# SFTP Batch Ingestion Pack

## Purpose

Demonstrates the SFTP batch ingestion pattern: download a file from an SFTP server, then upload it to blob storage. Exercises SFTP and blob connectors with idempotent delivery.

## Use this pack to

1. Validate SFTP connectivity with managed identity or key-based auth
2. Test blob upload with container-level idempotency
3. Exercise multi-step file transfer workflows with correlation tracking

## Prerequisites

- SFTP server accessible from the Container Apps environment
- Blob storage account with `batch-ingestion` container created
- SFTP credentials configured via Key Vault reference in App Configuration

## Configuration

Set the following environment variables or App Configuration keys:

| Variable | Required | Description |
|---|---|---|
| `EXECUTION_LEDGER_SFTP_HOST` | Yes | SFTP server hostname |
| `EXECUTION_LEDGER_SFTP_USERNAME` | Yes | SFTP username |
| `EXECUTION_LEDGER_SFTP_PASSWORD` | Via Key Vault | SFTP password (use Key Vault reference) |

## How the download-to-blob chain works

`action` is a step-level field (a sibling of `connector` and `config`, not nested inside `config`). Set `"action": "download"` on an `sftp` step to read a remote file instead of writing one; omit it (or set `"action": "upload"`) for the default write behavior.

The `download-file` step returns the file content base64-encoded in its own output — it never touches the worker's local filesystem, so there is no `localPath` config key. Leave the `upload-to-blob` step's `data`/`input` empty (no `body` field): the `azure_blob` connector automatically falls back to reading the previous step's output when its own input has no explicit body, decodes the base64 content, and uploads the original bytes. Note the connector name is `azure_blob`, not `blob`.

## Run it

```bash
cargo run -p execution-ledger-cli -- job-launch <tenant-id> sftp-batch.download-and-store '{"steps":[{"id":"health-check","connector":"platform_health"},{"id":"download-file","connector":"sftp","action":"download","config":{"remotePath":"/inbox/data-export.csv"}},{"id":"upload-to-blob","connector":"azure_blob","config":{"container":"batch-ingestion","blobName":"imports/data-export.csv"}}]}'
```

## Failure modes

| Symptom | Likely cause | Fix |
|---|---|---|
| `download-file` fails with `InvalidConfiguration` | SFTP host or username not set | Check `EXECUTION_LEDGER_SFTP_HOST` and `EXECUTION_LEDGER_SFTP_USERNAME` |
| `download-file` fails with connection error | SFTP server unreachable | Verify network connectivity and firewall rules |
| `upload-to-blob` fails | Blob container does not exist or managed identity lacks write access | Create container and assign `Storage Blob Data Contributor` role |
| `upload-to-blob` writes a JSON-wrapped payload instead of the CSV | An explicit `body` was set in the step's `data`, which takes priority over the upstream download content | Remove `body`/`input` from the `upload-to-blob` step so it falls back to reading the prior step's output |
