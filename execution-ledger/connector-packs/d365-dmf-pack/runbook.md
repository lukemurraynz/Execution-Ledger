# D365 DMF Export/Import Pack

## Purpose

Runs a Dynamics 365 Finance & Operations Data Management Framework (DMF) package export and import end-to-end: start the export, wait for D365 to finish building the package, download and unpack it into blob storage; or zip staged blobs, hand them to D365, and trigger the import. Four connector-step jobs plus a scheduled poll instead of an in-workflow blocking loop.

## Use this pack to

1. Get idempotency, replay, and audit trail on a DMF export/import without a dedicated orchestration workflow.
2. Kick off a DMF export, poll for completion without tying up a worker's bounded concurrency for up to an hour, and land the result in blob storage.
3. Validate a specific stage in isolation, for example replay just `unzip-package` against an already-downloaded staging blob if extraction needs to be re-run.

## Prerequisites

- An existing D365 credential-broker HTTP endpoint that accepts the `PartURL`/`Data` request shape shown in the job templates and proxies to the D365 OData `DataManagementDefinitionGroups` actions (`ExportToPackage`, `GetExecutionSummaryStatus`, `GetExportedPackageUrl`). This pack calls that broker rather than authenticating to D365 directly; see `pack.json`'s notes for why.
- Blob storage account with `dmf-export-staging` and `dmf-export-extracted` containers (or your chosen names, kept consistent across `export-transfer.json`'s steps; see that file's `_comment`).
- A way to schedule `d365-dmf.export-poll` on a recurring cadence (for example every 2 minutes) rather than launching it once; see `docs/drasi-integration.md` for the recommended change-driven trigger for `export-transfer` once a poll succeeds.

## Configuration

| Variable                            | Required | Description                                                                              |
| ----------------------------------- | -------- | ---------------------------------------------------------------------------------------- |
| `EXECUTION_LEDGER_D365_BROKER_URL`  | Yes      | URL of the D365 credential-broker endpoint                                               |
| `EXECUTION_LEDGER_BLOB_ACCOUNT_URL` | Yes      | e.g. `https://<account>.blob.core.windows.net/`, used by `azure_blob` and `blob_archive` |

## The export jobs and how they chain

1. **`d365-dmf.export-start`** calls `ExportToPackage`. The response's `response.value` is the D365 `executionId`. Read it from this run's persisted step output (`jobs.get_steps` or the MCP `execution_ledger.jobs.get_steps` tool) and carry it forward yourself into the next job's launch input. There is no automatic run-to-run state passing across separate job launches, only step-to-step within one run.
2. **`d365-dmf.export-poll`** calls `GetExecutionSummaryStatus` with that `executionId`. Its `output_schema` only accepts `Succeeded`/`PartiallySucceeded`; `NotRun`/`Failed` fail this step (and therefore this run) by design; see `jobs/export-poll.json`'s `_comment`. Launch this on a schedule; do not launch it once and expect it to wait.
3. **`d365-dmf.export-transfer`** calls `GetExportedPackageUrl`, downloads the package with the `http_download` connector's `download` action (needed because `rest`/`erp` only return parsed JSON/text, never binary), stages the raw zip via `azure_blob`, then unpacks it into per-file blobs via the `blob_archive` connector's `unzip` action.

## The import job

**`d365-dmf.import-package`** runs in one job, with no poll step. A small-package import is synchronous enough that there's nothing worth waiting on across separate launches. It calls `GetAzureWriteUrl`, zips the staged source blobs with `blob_archive`'s `zip` action, PUTs the archive to the writable URL with `http_download`'s `upload` action, then calls `ImportFromPackage`. Because `get-write-url` and `upload-package` aren't adjacent steps, the writable URL has to be carried forward into `upload-package`'s (and `import-package`'s) config explicitly; this is the same constraint as `executionId` in the export flow, see `jobs/import-package.json`'s `_comment`.

If your import volume is large enough to need async status polling the way export does, mirror `export-poll`'s scheduled-job pattern (see `docs/drasi-integration.md`) rather than adding a blocking loop to this job.

## New connectors this pack depends on

| Connector                      | Why it didn't already exist                                 | What it does                                                                                                                                                                     |
| ------------------------------ | ----------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `http_download` (`download`)   | `rest`/`erp` cannot carry binary response bodies downstream | Downloads a URL's bytes, returns them base64-encoded in the same shape `sftp download` uses, so `azure_blob`/`blob_archive` can consume them                                     |
| `http_download` (`upload`)     | Same gap, reverse direction                                 | PUTs upstream base64 bytes (e.g. from `blob_archive` `zip`) to a URL, with `x-ms-blob-type: BlockBlob` by default for Azure Blob SAS URLs                                        |
| `blob_archive` (`unzip`/`zip`) | No connector previously touched zip archive contents        | `unzip` extracts a zip blob's entries to individual destination blobs; `zip` bundles named blobs into one archive, uploads it, and also returns it as base64 for direct chaining |

All three are documented in full in `crates/execution-ledger-domain/src/connectors/http_download.rs` and `blob_archive.rs`.

## Run it

```bash
# 1. Start the export, then read executionId from this run's step output
cargo run -p execution-ledger-cli -- job-launch <tenant-id> d365-dmf.export-start '{"steps":[{"id":"start-export","connector":"erp","config":{"url":"<broker-url>"},"data":{"body":{"PartURL":"/data/DataManagementDefinitionGroups/Microsoft.Dynamics.DataEntities.ExportToPackage","Data":{"definitionGroupId":"<pkg>","legalEntityId":"<entity>","packageName":"<pkg>","reExecute":"false"}}}}]}'

# 2. Poll (repeat on a schedule until it succeeds -- see docs/drasi-integration.md)
cargo run -p execution-ledger-cli -- job-launch <tenant-id> d365-dmf.export-poll '{"steps":[{"id":"check-status","connector":"erp","config":{"url":"<broker-url>"},"data":{"body":{"PartURL":"/data/DataManagementDefinitionGroups/Microsoft.Dynamics.DataEntities.GetExecutionSummaryStatus","Data":{"executionId":"<from step 1>"}}},"outputSchema":{"type":"object","properties":{"response":{"type":"object","properties":{"value":{"enum":["Succeeded","PartiallySucceeded"]}},"required":["value"]}},"required":["response"]}}]}'

# 3. Transfer, once step 2 succeeds
cargo run -p execution-ledger-cli -- job-launch <tenant-id> d365-dmf.export-transfer '{"steps":[{"id":"get-package-url","connector":"erp","config":{"url":"<broker-url>"},"data":{"body":{"PartURL":"/data/DataManagementDefinitionGroups/Microsoft.Dynamics.DataEntities.GetExportedPackageUrl","Data":{"executionId":"<from step 1>"}}}},{"id":"download-package","connector":"http_download","config":{}},{"id":"stage-package-blob","connector":"azure_blob","config":{"container":"dmf-export-staging","blobName":"<pkg>.zip"}},{"id":"unzip-package","connector":"blob_archive","action":"unzip","config":{"sourceContainer":"dmf-export-staging","sourceBlobName":"<pkg>.zip","destinationContainer":"dmf-export-extracted","destinationPrefix":"<pkg>/"}}]}'

# 4. Import (independent of 1-3; requires blobs already staged in dmf-import-staging)
cargo run -p execution-ledger-cli -- job-launch <tenant-id> d365-dmf.import-package '{"steps":[{"id":"get-write-url","connector":"erp","config":{"url":"<broker-url>"},"data":{"body":{"PartURL":"/data/DataManagementDefinitionGroups/Microsoft.Dynamics.DataEntities.GetAzureWriteUrl","Data":{"definitionGroupId":"<pkg>","uniqueFileName":"<pkg>.zip"}}}},{"id":"zip-package","connector":"blob_archive","action":"zip","config":{"sourceContainer":"dmf-import-staging","sourceBlobNames":["file-1.csv","file-2.csv"],"destinationContainer":"dmf-import-archive","destinationBlobName":"<pkg>.zip"}},{"id":"upload-package","connector":"http_download","action":"upload","config":{"url":"<from step get-write-url>"}},{"id":"import-package","connector":"erp","config":{"url":"<broker-url>"},"data":{"body":{"PartURL":"/data/DataManagementDefinitionGroups/Microsoft.Dynamics.DataEntities.ImportFromPackage","Data":{"definitionGroupId":"<pkg>","packageUrl":"<same URL as upload-package>","executionId":"","execute":"true","overwrite":"true","legalEntityId":"<entity>"}}}}]}'
```

## Failure modes

| Symptom                                                                 | Likely cause                                                                                                         | Fix                                                                                                                                                                      |
| ----------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `export-poll` fails every tick with a schema-validation error           | This is expected while D365 is still running (`NotRun`), not an incident by itself                                   | Only escalate if it's still failing after D365's typical export duration for this package, or if the underlying value is `Failed` (check the step's logged raw response) |
| `download-package` fails with `InvalidConfiguration` naming `url`       | `get-package-url`'s response didn't contain `response.value`, or the broker call itself failed upstream of this step | Check `get-package-url`'s persisted output first; a broken URL upstream surfaces here as a missing input, not where the real fault is                                    |
| `unzip-package` fails partway through (some files uploaded, others not) | A transient blob-write failure mid-extraction                                                                        | Replay `unzip-package`; it re-extracts and overwrites every entry, so a partial prior run is safe to redo in full (see `blob_archive`'s module doc)                      |
| `export-transfer` never gets triggered after `export-poll` succeeds     | No automation wired between "poll succeeded" and "launch transfer"                                                   | See `docs/drasi-integration.md`; this is the reactive-trigger gap that pack doc addresses                                                                                |
| `upload-package` fails with `InvalidConfiguration` naming "upload"      | Neither `zip-package`'s output nor `upload-package`'s own `input.body` has usable content                            | Confirm `zip-package` ran immediately before `upload-package` (not `get-write-url`; that step is two positions back and won't be seen as upstream output here)           |
| `upload-package` fails with a 4xx from Azure Blob                       | Target isn't actually an Azure Blob SAS URL, or it needs a different blob type                                       | Set `blobTypeHeader` in `upload-package`'s config, or clear it (empty string) if the target isn't Azure Blob at all                                                      |
