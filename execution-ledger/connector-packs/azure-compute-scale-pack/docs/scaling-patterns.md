# Scaling Patterns: Cron vs Metric-Reactive

## When to Use Each Pattern

| Pattern | Use when | Don't use when |
|---------|----------|-----------------|
| **Cron** | You know the schedule in advance | Load is unpredictable |
| **Metric-reactive** | Load signals come from the system | Signals are noisy or delayed |
| **Control table + Drasi** | Ops needs a manual kill-switch | You need full automation |

## Pattern 1: Pure Cron

Best for: Dev/test environments, predictable workloads, cost-focused orgs.

```
Friday 8pm  → scale-down
Monday 7am → scale-up
```

**Pros**: Simple, predictable, auditable.
**Cons**: No reaction to real load. Can scale down during unexpected high load if a batch job fires.

Execution Ledger config:
```bash
# Schedule the weekend scale-down
az execution-ledger schedule create \
  --job-id azure-compute-scale.weekend-scale-down \
  --cron "0 20 * * 5" \
  --timezone "Pacific Standard Time"

# Schedule the weekday scale-up
az execution-ledger schedule create \
  --job-id azure-compute-scale.weekend-scale-up \
  --cron "0 7 * * 1" \
  --timezone "Pacific Standard Time"
```

## Pattern 2: Cron Base + Drasi Override

Best for: Production environments where you want a schedule but need to handle exceptions.

The cron job runs on Friday at 8pm. But Drasi watches Event Hub and cancels or reverses the scale-down if it sees unusual activity.

```
┌──────────────────────────────────────────────┐
│ Cron: Friday 8pm                           │
│ → scale-down step fires                     │
│   BUT: check a control table first          │
│   AND: Drasi watches Event Hub              │
│       → if queue_depth > 0: cancel scale-down│
└──────────────────────────────────────────────┘
```

Drasi continuous query for cancellation:
```sql
SELECT TOP 1 * FROM scale_override
WHERE mode = 'force-down'
  AND updated_at > DATEADD(hour, -2, GetCurrentTimestamp())
```

If this query returns a row, Drasi triggers an emergency-scale-up instead.

## Pattern 3: Pure Metric-Reactive

Best for: Always-on systems where you want to scale to zero or near-zero during any period of inactivity, regardless of day of week.

```
Metric: CPU < 5% AND queue_depth = 0 for 30 consecutive minutes
  → scale-down to 1 replica (Standard_B2s)

Metric: CPU > 60% for 5 consecutive minutes
  → scale-up to 5 replicas (Standard_D8s_v3)
```

**Pros**: Reacts to actual load. Near-zero when genuinely idle.
**Cons**: Risk of oscillation (scale up → scale down → scale up). No guaranteed schedule.

**Oscillation mitigation**:
- Use a **hysteresis window**: scale down only after 30 min of low load, scale up after 5 min of high load.
- Set `scaleUpCooldownMinutes: 15` and `scaleDownCooldownMinutes: 60` in the reactive job data.

## Pattern 4: Scheduled Baseline + Reactive Burst

Best for: Workloads with predictable base load + unpredictable spikes.

```
┌──────────────────────────────────────────────┐
│ Baseline: Cron-scaled to D8s_v3, 3 replicas │
│           (Monday 7am → Friday 8pm)         │
│                                              │
│ Burst: Drasi reactive scale to D16s_v3       │
│        (queue_depth > 50 for 5 min)          │
└──────────────────────────────────────────────┘
```

The reactive job checks the current SKU before scaling: if it's already at D16s_v3, it skips the step (idempotency).

## Decision Matrix

```
                    │ Predictable schedule  │ Unpredictable load
────────────────────┼───────────────────────┼─────────────────────
Noisy signals       │ Pattern 1 (Cron)     │ Pattern 2 (Cron+Drasi)
────────────────────┼───────────────────────┼─────────────────────
Clean signals       │ Pattern 2 (Cron+Drasi│ Pattern 3 (Reactive)
                    │                       │ + hysteresis
────────────────────┼───────────────────────┼─────────────────────
Cost-critical       │ Pattern 1 (Cron)     │ Pattern 4 (Baseline+Burst)
                    │ + Drasi override     │ + Pattern 3
```

## Observability Checklist

- [ ] Every scale step emits `correlationId`, `targetSku`, `previousSku`, `provisioningState`
- [ ] KQL alert on scale events: `| where Action_s == "vmss-resize"`
- [ ] Log Analytics view: scale events over time grouped by `TargetSku_s`
- [ ] Alert if scale-up runs more than 3 times in 1 hour (oscillation detection)
- [ ] Runbook step for "VMSS won't scale: common causes and fixes"
