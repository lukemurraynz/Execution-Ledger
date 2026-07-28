# Drasi Sources for Metric-Reactive Scaling

Drasi can monitor three source types for autoscaling signals. Each has different latency, cost, and operational complexity.

## Source Comparison

| Source | Latency | Cost | Complexity | Best for |
|--------|---------|------|------------|----------|
| **Event Hub** (Azure Monitor alerts) | ~30s | Low (per event) | Medium | Cloud-native metric alerts |
| **Event Hub** (app events) | <1s | Low | Medium | Application-level load signals |
| **Postgres** (control table) | ~5s | Near zero | Low | Ops manual override signals |
| **Post Hub** (metric polling) | 1–5min | Higher | High | Legacy systems without event export |

## Event Hub: Azure Monitor alerts

Azure Monitor can stream metric alert events to Event Hub. Configure a metric alert:

```bash
az monitor alert create \
  --resource-group <rg> \
  --alert-name "LowCpuScaleDown" \
  --description "CPU < 5% for 5 minutes" \
  --condition "avg Percentage CPU < 5" \
  --time 5 \
  --evaluation-frequency 1 \
  --severity 3 \
  --notification-target '{"eventHub": {"namespace": "<namespace>", "name": "<hub-name>"}}'
```

The alert fires an event like:
```json
{
  "schemaId": "Microsoft.Insights/MetricAlert",
  "data": {
    "time": "2026-07-26T10:00:00Z",
    "level": "Informational",
    "operationName": "MetricAlert",
    "alertName": "LowCpuScaleDown",
    "condition": "avg Percentage CPU < 5",
    "dimension": [],
    "threshold": 5
  }
}
```

## Drasi continuous query: Event Hub source

In `drasi/source-eventhubs.yaml`:
```yaml
apiVersion: v1
kind: Source
metadata:
  name: azure-monitor-alerts
spec:
  containerName: <event-hub-namespace>
  eventHubName: <hub-name>
  consumerGroup: $Default
  startTime: "2026-07-01T00:00:00Z"
```

Reaction (HTTP trigger):
```yaml
apiVersion: v1
kind: Reaction
metadata:
  name: scale-down-on-low-cpu
spec:
  trigger:
    kind: Continuous
    query: |
      SELECT COUNT(*) as eventCount
      FROM AzureMonitorAlerts
      WHERE data.alertName = 'LowCpuScaleDown'
        AND data.time > DATEADD(minute, -30, GetCurrentTimestamp())
  actions:
    - kind: HttpPost
      endpoint: https://<apim-gateway>/platform/jobs/trigger
      headers:
        Authorization: Bearer <token>
      body:
        jobId: azure-compute-scale.metric-reactive-scale-down
        tenantId: <tenant-id>
```

## Event Hub: application events

For application-level signals (queue depth, active connections), publish directly to Event Hub from your app:

```csharp
await using var eventBatch = new EventDataBatch();
eventBatch.Add(new EventData(Encoding.UTF8.GetBytes(JsonSerializer.Serialize(new {
    metric = "queue_depth",
    value = currentDepth,
    timestamp = DateTime.UtcNow
})));
await producerClient.SendAsync(eventBatch);
```

Drasi query:
```sql
SELECT AVG(CAST(e.data.value AS float)) as avgQueueDepth
FROM EventHubStream e
WHERE e.data.metric = 'queue_depth'
  AND e.data.timestamp > DATEADD(minute, -5, GetCurrentTimestamp())
```

## Postgres: control table for ops override

Simpler for manual override: ops writes to a table, Drasi polls it:

```sql
CREATE TABLE scale_override (
  id         int         PRIMARY KEY DEFAULT 1 CHECK (id = 1),
  mode       text        NOT NULL DEFAULT 'none',  -- 'none' | 'force-down' | 'force-up'
  reason     text,
  updated_by  text,
  updated_at  timestamptz NOT NULL DEFAULT now()
);
```

```yaml
# Drasi continuous query on Postgres
SELECT * FROM scale_override
WHERE mode != 'none'
  AND updated_at > DATEADD(hour, -1, GetCurrentTimestamp())
```

HTTP reaction body:
```json
{
  "jobId": "azure-compute-scale.emergency-scale-up",
  "tenantId": "<tenant-id>"
}
```

## Recommended Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Azure Monitor                                           │
│  → Metric Alerts → Event Hub                             │
└──────────────────┬──────────────────────────────────────┘
                   │ LowCpu / HighQueue events
                   ▼
┌─────────────────────────────────────────────────────────────┐
│  Drasi Source (Event Hub)                                │
│  → Continuous query: CPU < 5% for 30min                  │
└──────────────────┬──────────────────────────────────────┘
                   │ Continuous query result
                   ▼
┌─────────────────────────────────────────────────────────────┐
│  Drasi HTTP Reaction                                     │
│  → POST /platform/jobs/trigger                          │
│     jobId: azure-compute-scale.metric-reactive-scale-down│
└──────────────────┬──────────────────────────────────────┘
                   │
                   ▼
┌─────────────────────────────────────────────────────────────┐
│  Execution Ledger Worker                                 │
│  → azure_compute connector (MSI → ARM API)               │
│  → VMSS resize to Standard_B2s, capacity 1              │
└─────────────────────────────────────────────────────────────┘
```

This gives you <1 minute reaction time from metric threshold breach to VMSS resize, without any polling loops.
