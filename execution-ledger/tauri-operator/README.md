# Execution Ledger Tauri Operator

Desktop operator surface for Execution Ledger admin operations, run control, and scenario testing.

## What it does

- Connects to the deployed Execution Ledger API via Rust-backed Tauri commands.
- Supports run listing and run detail and step inspection.
- Supports operator actions: replay, skip, compensate.
- Supports admin recovery actions: run-lease recovery, schedule-lease recovery, local dead-letter drain, and Service Bus dead-letter drain.
- Supports manual trigger payload execution.
- Supports schedule management: list, register, enable, disable cron-triggered job runs.
- Includes two showcase scenarios:
  - `migration.biztalk.bank-file`
  - `drasi.orders.change-reaction`

## Run locally

1. Install dependencies:
   ```console
   npm install
   ```
1. Start desktop dev app:
   ```console
   npm run tauri dev
   ```
1. Build installable package:
   ```console
   npm run tauri build -- --debug
   ```

## Validation commands

```console
npm run test
npm run build
```

## Admin onboarding and connection setup

### Prerequisites

1. A running Execution Ledger API endpoint (for example `https://<api-host>`).
1. An Entra access token for that API with the required scopes for the actions you will run.
1. Optional: API gateway secret if your deployment enables gateway header validation.

Scope guidance lives in `../README.md` under `Required Entra scopes`.

### First-time setup in the desktop app

1. Start the app with `npm run tauri dev` (or open an installed build).
1. In the `Connection` panel, set:
   - `API base URL` to your Execution Ledger API host.
   - `Bearer token` to your Entra access token.
   - `Gateway secret` only when your deployment requires it.
   - `APIM client ID` when your APIM setup uses a non-default client id.
1. In `Command bar`, run `Check Tauri` then `Check API`.
1. Run `Refresh runs` to verify authenticated access to platform endpoints.

### Operational flow

1. Use `Scenarios` or `Manual trigger` to start a run.
2. Use `Schedules` to register and enable cron-triggered job runs.
3. Open a run in `Runs`, then apply `Replay`, `Skip`, or `Compensate` in `Actions`.
4. Use `Operations` for lease recovery or poison queue drains when incident runbooks require it.

### Environment default

The UI can prefill API URL from `VITE_EXECUTION_LEDGER_API_URL`, but admins can override it in the `Connection` panel at runtime.

## Notes

- The UI talks to the API through `src-tauri/src/lib.rs` commands (no browser-side CORS dependency).
- Credentials entered in the UI are held in process memory; the app does not persist session secrets to local storage.
- APIM requests include gateway-secret and APIM HMAC headers when a gateway secret is set, enabling APIM-client authorization fallback when bearer tokens are not available.
