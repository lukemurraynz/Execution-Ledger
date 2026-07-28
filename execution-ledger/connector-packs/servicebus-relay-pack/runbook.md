# Service Bus Relay Pack

## Purpose

Demonstrates the Service Bus relay pattern: receive a message from a queue, validate it, and forward to a downstream REST endpoint. Exercises the queue connector's `receive` action alongside the REST connector.

## How receive works

`action` is a step-level field (a sibling of `connector` and `config`, not nested inside `config`). Set `"action": "receive"` on a `queue` step to receive and remove one message; omit it (or set `"action": "send"`) for the default send/enqueue behavior.

The `queue` connector completes (removes) a received message from the queue immediately, within the same step — it does not wait to see whether a later step in the job succeeds. If a downstream step fails and the run is compensated, `compensate()` re-publishes the message from the captured `stateBlob`, so it isn't silently lost — but note this means the message is off the queue as soon as `receive-message` succeeds, not only once the whole job succeeds. See the `queue` connector's module docs ("Receive and compensation") for the full reasoning.

The `forward-to-api` step does **not** automatically receive the message content — unlike `azure_blob`, the `rest`/`erp` connectors don't consume `ctx.upstream_output` (a REST payload's shape is a business decision the `rest` connector can't infer). Reference the received message explicitly, e.g. via your job-orchestration tooling templating the prior step's `body` output into this step's `data.body`, or restructure the payload for your specific downstream API's contract.

## Use this pack to

1. Validate Service Bus managed-identity connectivity (both send and receive roles)
2. Test message receive-and-forward patterns with idempotent delivery
3. Exercise the queue connector's `receive` action alongside the REST connector

## Prerequisites

- Service Bus namespace with an `incoming-messages` queue (or custom queue name)
- Managed identity configured for Service Bus `Azure Service Bus Data Receiver` and `Azure Service Bus Data Sender` roles (receiver for `receive-message`, sender in case a message needs to be re-published by `compensate()`)
- Downstream REST endpoint configured and accessible

## Configuration

Set the following environment variables or App Configuration keys:

| Variable | Required | Description |
|---|---|---|
| `EXECUTION_LEDGER_SERVICEBUS_NAMESPACE` | Yes | Service Bus namespace (e.g., `myservicebus.servicebus.windows.net`) |
| `EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME` | Yes | Default queue name if not set per-step |

## Run it

```bash
cargo run -p execution-ledger-cli -- job-launch <tenant-id> servicebus-relay.validate-and-forward '{"steps":[{"id":"health-check","connector":"platform_health"},{"id":"receive-message","connector":"queue","action":"receive","config":{"queueName":"incoming-messages"}},{"id":"forward-to-api","connector":"rest","config":{"url":"https://downstream.example.com/api","method":"POST"}}]}'
```

The `receive-message` step's output includes `"received": true/false` — check this before assuming a message was actually available. An empty queue is a normal outcome, not a failure.

## Failure modes

| Symptom | Likely cause | Fix |
|---|---|---|
| `receive-message` fails with `InvalidConfiguration` | Service Bus namespace or queue name not set | Check `EXECUTION_LEDGER_SERVICEBUS_NAMESPACE` and `EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME`, or the step's `queueName`/`namespace` config |
| `receive-message` fails with authentication error | Managed identity not assigned Service Bus roles | Assign `Azure Service Bus Data Receiver` (and `Azure Service Bus Data Sender`, for compensate re-publish) roles |
| `receive-message` returns `"received": false` every time | Queue is genuinely empty, or pointed at the wrong queue name | Confirm messages are landing on the queue named in `queueName`, not a different queue |
| `forward-to-api` fails | Downstream endpoint unreachable | Verify URL and network connectivity from Container Apps environment |
| `forward-to-api` doesn't see the received message content | `rest`/`erp` don't auto-consume the prior step's output | Pass the message content explicitly in `data.body`, templated from the `receive-message` step's `body` output |
