# REST Webhook Pack

## Purpose

Demonstrates the REST webhook pattern: call an external API endpoint with idempotent delivery. Exercises the REST connector with retry-safe semantics.

## Use this pack to

1. Validate REST connector configuration and connectivity
2. Test idempotent webhook delivery with step-level replay
3. Exercise the compensation path when a webhook call fails

## Prerequisites

- External webhook endpoint accessible from the Container Apps environment
- No additional connector configuration required for the REST connector

## Configuration

Replace `<your-webhook-endpoint>` in the job template with your actual endpoint URL. The REST connector will fail the step with `InvalidConfiguration` if the URL is missing or invalid.

## Run it

```bash
cargo run -p execution-ledger-cli -- job-launch <tenant-id> rest-webhook.call-and-persist '{"steps":[{"id":"health-check","connector":"platform_health","config":{},"data":{}},{"id":"call-webhook","connector":"rest","config":{"url":"https://your-webhook.example.com/notify","method":"POST"},"data":{"payload":{"event":"test"}}}]}'
```

## Failure modes

| Symptom | Likely cause | Fix |
|---|---|---|
| `call-webhook` fails with `InvalidConfiguration` | URL missing or malformed | Set a valid URL in the step config |
| `call-webhook` fails with timeout | External endpoint unreachable or slow | Verify network connectivity; REST connector caps at 120 seconds |
| `call-webhook` fails with non-2xx response | External endpoint rejecting the request | Check endpoint auth, payload format, and rate limits |
