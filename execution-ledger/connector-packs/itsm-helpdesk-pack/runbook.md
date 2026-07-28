# ITSM helpdesk pack runbook

## Intent

This pack orchestrates IT service desk ticket workflows using Microsoft Foundry agents. It covers two common scenarios:

1. **Ticket creation** — invoke a Foundry agent, persist the ticket record, and notify a Teams channel via Adaptive Card.
2. **Ticket escalation** — invoke the agent with escalation context, publish an event to Service Bus for async worker processing, and send a follow-up message to the user's conversation.

Both flows compose existing `rest` and `queue` connectors. No new core connectors are required.

## Included job templates

- `jobs/helpdesk.ticket.create.json` — synchronous ticket creation with Teams notification
- `jobs/helpdesk.ticket.escalate.json` — async escalation with Service Bus event dispatch

## Required environment variables

Before running this pack, ensure these variables are set in App Configuration or Container Apps:

| Variable | Used by | Notes |
|----------|---------|-------|
| `EXECUTION_LEDGER_FOUNDRY_PROJECT_ENDPOINT` | REST connector | Foundry project endpoint (e.g. `https://<project>.services.api.azureml.ms`) |
| `EXECUTION_LEDGER_FOUNDRY_AGENT_ID` | REST connector | Registered agent ID (e.g. `helpdesk-agent`) |
| `EXECUTION_LEDGER_TEAMS_WEBHOOK_URL` | REST connector | Power Automate HTTP-triggered flow webhook URL |
| `EXECUTION_LEDGER_SERVICEBUS_NAMESPACE` | Queue connector | Service Bus namespace (managed identity must have send rights) |
| `EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME` | Queue connector | Target queue name (e.g. `ticket-worker`) |
| `EXECUTION_LEDGER_TICKET_STORAGE_URL` | REST connector | Ticket persistence endpoint (Table Storage or equivalent API) |

## Prerequisites

1. **Foundry agent registered** — the agent must be pre-registered with a File Search tool (over IT documentation) and `create_ticket` / `get_ticket_status` function tools. Registration is outside Execution Ledger's scope; use your own Foundry agent-provisioning tooling (script, Terraform, or the Foundry portal) to create and register the agent, then note its agent ID for `EXECUTION_LEDGER_FOUNDRY_AGENT_ID` below.
2. **Custom Foundry role** — the built-in `Foundry Agent Consumer` role is insufficient. A custom role granting `endpoints/interact/action` + `agents/read` + `agents/write` is required. Define this role in your own IaC (Bicep or Terraform) alongside your Foundry project deployment.
3. **Teams webhook** — a Power Automate HTTP-triggered flow that renders an Adaptive Card. The webhook URL is the only unavoidable secret; store in Key Vault.
4. **Service Bus queue** — the `ticket-worker` queue must exist with the worker's managed identity having `Azure Service Bus Data Receiver` role.

## Preflight checks before first run

1. Confirm all required pack config from `pack.json` is present in App Configuration.
2. Confirm Foundry agent endpoint is reachable and the managed identity has the custom agent-runtime role.
3. Confirm Teams webhook URL is valid and the Power Automate flow is active.
4. Confirm Service Bus queue exists and the worker identity has receive rights.
5. Confirm the worker has outbound network access to all targets.

## Launch from CLI

Run from `execution-ledger/`:

```bash
# Ticket creation (requires all steps to have config set)
cargo run -p execution-ledger-cli -- job-launch <tenant-id> helpdesk.ticket.create '{"steps":[{"id":"invoke-foundry-agent","connector":"rest","externalId":"foundry-agent-conversation","data":{"method":"POST","body":{"action":"invoke_agent","conversationId":"conv-001","message":"My VPN keeps dropping every hour"}},"config":{"url":"${EXECUTION_LEDGER_FOUNDRY_PROJECT_ENDPOINT}/agents/${EXECUTION_LEDGER_FOUNDRY_AGENT_ID}/invoke","method":"POST","timeoutMs":60000}},{"id":"persist-ticket-record","connector":"rest","externalId":"ticket-storage-write","data":{"method":"POST","body":{"action":"create_ticket","ticketId":"TK-2025-001","conversationId":"conv-001","title":"VPN drops hourly","description":"User reports VPN connection drops every hour","priority":"medium","status":"New"}},"config":{"url":"${EXECUTION_LEDGER_TICKET_STORAGE_URL}","method":"POST","timeoutMs":15000}},{"id":"notify-teams-channel","connector":"rest","externalId":"teams-adaptive-card","data":{"method":"POST","body":{"type":"message","attachments":[{"contentType":"application/vnd.microsoft.card.adaptive","content":{"type":"AdaptiveCard","version":"1.4","body":[{"type":"TextBlock","text":"New IT Ticket Created","weight":"Bolder","size":"Medium"},{"type":"FactSet","facts":[{"title":"Ticket ID","value":"TK-2025-001"},{"title":"Priority","value":"medium"},{"title":"Status","value":"New"}]}]}}]}},"config":{"url":"${EXECUTION_LEDGER_TEAMS_WEBHOOK_URL}","method":"POST","timeoutMs":15000}}]}'

# Ticket escalation
cargo run -p execution-ledger-cli -- job-launch <tenant-id> helpdesk.ticket.escalate '{"steps":[{"id":"invoke-foundry-agent","connector":"rest","externalId":"foundry-agent-escalation","data":{"method":"POST","body":{"action":"invoke_agent","conversationId":"conv-002","message":"I need access to the finance shared drive for an audit","escalationContext":{"reason":"agent_determined_human_needed","category":"access_request"}}},"config":{"url":"${EXECUTION_LEDGER_FOUNDRY_PROJECT_ENDPOINT}/agents/${EXECUTION_LEDGER_FOUNDRY_AGENT_ID}/invoke","method":"POST","timeoutMs":60000}},{"id":"publish-escalation-event","connector":"queue","externalId":"ticket-escalation-events","data":{"priority":"high","body":{"ticketId":"TK-2025-002","conversationId":"conv-002","title":"Finance drive access","description":"User needs finance shared drive access for audit","priority":"high","category":"access_request"}},"config":{"provider":"servicebus","queueName":"ticket-worker","namespace":"${EXECUTION_LEDGER_SERVICEBUS_NAMESPACE}"}},{"id":"acknowledge-escalation","connector":"rest","externalId":"escalation-ack","data":{"method":"POST","body":{"action":"acknowledge_escalation","ticketId":"TK-2025-002","conversationId":"conv-002","status":"Escalated","message":"Your issue has been escalated to the IT helpdesk team."}},"config":{"url":"${EXECUTION_LEDGER_FOUNDRY_PROJECT_ENDPOINT}/agents/${EXECUTION_LEDGER_FOUNDRY_AGENT_ID}/conversations/conv-002/messages","method":"POST","timeoutMs":30000}}]}'
```

## Launch from API

```bash
# Ticket creation
curl -X POST https://<apim-gateway>/platform/jobs/trigger \
  -H "Authorization: Bearer <token-with-jobs:write:own-scope>" \
  -H "Content-Type: application/json" \
  -d '{
    "tenantId": "<tenant-id>",
    "jobId": "helpdesk.ticket.create",
    "steps": [
      {"id": "invoke-foundry-agent", "connector": "rest", "externalId": "foundry-agent-conversation", "data": {"method": "POST", "body": {"action": "invoke_agent", "conversationId": "conv-001", "message": "My VPN keeps dropping every hour"}}, "config": {"url": "${EXECUTION_LEDGER_FOUNDRY_PROJECT_ENDPOINT}/agents/${EXECUTION_LEDGER_FOUNDRY_AGENT_ID}/invoke", "method": "POST", "timeoutMs": 60000}},
      {"id": "persist-ticket-record", "connector": "rest", "externalId": "ticket-storage-write", "data": {"method": "POST", "body": {"action": "create_ticket", "ticketId": "TK-2025-001", "conversationId": "conv-001", "title": "VPN drops hourly", "priority": "medium", "status": "New"}}, "config": {"url": "${EXECUTION_LEDGER_TICKET_STORAGE_URL}", "method": "POST", "timeoutMs": 15000}},
      {"id": "notify-teams-channel", "connector": "rest", "externalId": "teams-adaptive-card", "data": {"method": "POST", "body": {"type": "message"}}, "config": {"url": "${EXECUTION_LEDGER_TEAMS_WEBHOOK_URL}", "method": "POST", "timeoutMs": 15000}}
    ]
  }'
```

## Failure handling

### Step 1 (Foundry agent) fails: 403 PermissionDenied

**Symptom:** `DependencyFailed { dependency: "reqwest", source: "rest request failed with status 403: ..." }`

**Cause:** Managed identity lacks the required Foundry agent-runtime role. The built-in `Foundry Agent Consumer` role does not grant `agents/write`.

**Fix:**
1. Verify the custom role is assigned to the API's managed identity.
2. The role must grant: `endpoints/interact/action`, `agents/read`, `agents/write`.
3. Once fixed, replay the step: `cargo run -p execution-ledger-cli -- job-replay <tenant-id> <run-id> invoke-foundry-agent`

### Step 1 (Foundry agent) fails: timeout

**Symptom:** `DependencyFailed { dependency: "reqwest", source: "request or response body took longer than ..." }`

**Cause:** Agent processing exceeded the 60-second timeout.

**Fix:**
1. Check Application Insights for the `agent.invoke` span — look at token counts and tool selection.
2. If the agent is searching a large knowledge base, consider reducing the vector store size or adding filters.
3. Increase `timeoutMs` in the step config if the agent legitimately needs more time.
4. Replay the step after adjusting.

### Step 2 (Ticket persistence) fails: storage unavailable

**Symptom:** `DependencyFailed { dependency: "reqwest", source: "rest request failed with status 503: ..." }`

**Cause:** Table Storage or ticket API is temporarily unavailable.

**Fix:**
1. Check Azure resource health for the storage account.
2. If transient, replay the step — idempotency ensures the same ticket isn't duplicated.
3. If persistent, check the storage account's network rules and the worker's managed identity permissions.

### Step 2 (Ticket persistence) fails: duplicate ticket ID

**Symptom:** `DependencyFailed { dependency: "reqwest", source: "rest request failed with status 409: ..." }`

**Cause:** A ticket with the same ID already exists (idempotency guard).

**Fix:**
1. This is expected behavior on replay — the step already succeeded previously.
2. Check step output via `GET /platform/jobs/{id}/steps` to confirm the ticket record exists.
3. If the ticket needs updating, use a compensate action instead of re-creating.

### Step 3 (Teams notification) fails: webhook URL invalid

**Symptom:** `DependencyFailed { dependency: "reqwest", source: "rest request failed with status 400: ..." }`

**Cause:** Power Automate flow is disabled or webhook URL has changed.

**Fix:**
1. Verify the flow is active in Power Automate.
2. Confirm `EXECUTION_LEDGER_TEAMS_WEBHOOK_URL` in Key Vault matches the flow's trigger URL.
3. Replay only the notification step — the ticket record is already persisted.
4. `cargo run -p execution-ledger-cli -- job-replay <tenant-id> <run-id> notify-teams-channel`

### Step 3 (Teams notification) fails: Adaptive Card rejected

**Symptom:** `DependencyFailed { dependency: "reqwest", source: "rest request failed with status 400: ..." }` with card validation error.

**Cause:** Adaptive Card schema mismatch or missing required fields.

**Fix:**
1. Validate the card JSON at https://adaptivecards.io/designer.
2. Ensure `$schema` and `version` are present.
3. Fix the card template in the Power Automate flow (not in Execution Ledger — the card is rendered by the flow).
4. Replay the step.

### Step 2 (Service Bus) fails: namespace not found

**Symptom:** `InvalidConfiguration { name: "EXECUTION_LEDGER_SERVICEBUS_NAMESPACE" }`

**Cause:** Service Bus namespace not configured or not reachable.

**Fix:**
1. Confirm `EXECUTION_LEDGER_SERVICEBUS_NAMESPACE` is set in App Configuration.
2. Confirm the worker's managed identity has `Azure Service Bus Data Sender` role.
3. Replay the step after fixing configuration.

### Stale lease (worker crash mid-run)

**Symptom:** Run appears stuck in "running" state with no step progress.

**Fix:**
```bash
# Release stale leases
cargo run -p execution-ledger-cli -- recovery-runs

# Or via API
curl -X POST https://<apim-gateway>/platform/recovery/runs?limit=100 \
  -H "Authorization: Bearer <token-with-platform:write-scope>"
```

### Poison queue (unrecoverable message)

**Symptom:** Intent appears in poison/dead-letter queue after multiple failures.

**Fix:**
```bash
# Drain poison messages
cargo run -p execution-ledger-cli -- recovery-servicebus-poison

# Or via API
curl -X POST https://<apim-gateway>/platform/recovery/servicebus-poison?limit=50 \
  -H "Authorization: Bearer <token-with-platform:write-scope>"
```

## Replay vs. re-trigger

- **Replay a step:** Use when a specific step failed but earlier steps succeeded. Re-executes only the targeted step. Use `job-replay` or `POST /platform/jobs/{id}/replay`.
  - Most common: replay `notify-teams-channel` when the webhook fails but the ticket is already persisted.
  - Replay `persist-ticket-record` if storage was temporarily unavailable.
- **Re-trigger the run:** Use when the entire flow needs to restart. Use `job-launch` or `POST /platform/jobs/trigger` with a new payload.
- **Compensate:** Use when downstream systems need rollback after partial success. Use `job-compensate` or `POST /platform/jobs/{id}/compensate`.

## Monitoring

- **Step status:** `GET /platform/jobs/{id}/steps` — shows each step's state, idempotency key, and output.
- **Correlation tracing:** Every step output includes a `correlationId`. Use this in Log Analytics to trace the full execution path across API, worker, and connector calls.
- **Run listing:** `GET /platform/jobs?limit=50` — shows recent runs with tenant, status, and timestamps.
- **Foundry agent traces:** Check Application Insights for `agent.invoke` custom spans — they include conversation ID, selected tools, and token counts.

## Customization

### Adding a ticket status check step

Add a fourth step to `helpdesk.ticket.create.json` that polls ticket status:

```json
{
  "id": "check-ticket-status",
  "connector": "rest",
  "externalId": "ticket-status-check",
  "data": {
    "method": "GET",
    "params": {
      "ticketId": "${TICKET_ID}"
    }
  },
  "config": {
    "url": "${EXECUTION_LEDGER_TICKET_STORAGE_URL}/tickets/${TICKET_ID}",
    "method": "GET",
    "timeoutMs": 10000
  }
}
```

### Adding email notification

Add a step after `notify-teams-channel` that calls a mail connector or REST endpoint:

```json
{
  "id": "notify-requester-email",
  "connector": "rest",
  "externalId": "email-notification",
  "data": {
    "method": "POST",
    "body": {
      "to": "${REQUESTER_EMAIL}",
      "subject": "Ticket ${TICKET_ID} created",
      "body": "Your IT ticket has been created. Priority: ${TICKET_PRIORITY}"
    }
  },
  "config": {
    "url": "${EXECUTION_LEDGER_EMAIL_API_URL}",
    "method": "POST",
    "timeoutMs": 15000
  }
}
```
