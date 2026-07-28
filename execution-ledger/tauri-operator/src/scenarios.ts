import type { ScenarioStep, JobTemplate } from "./types";

const bizTalkMigrationSteps: ScenarioStep[] = [
  {
    id: "ingest-bank-file",
    connector: "sftp",
    externalId: "/inbound/payments_2026_07_25.csv",
    data: { remotePath: "/inbound/payments_2026_07_25.csv" },
  },
  {
    id: "transform-to-bank-format",
    connector: "erp",
    externalId: "erp-payment-mapper",
    data: {
      method: "POST",
      body: { format: "AU-ABA", version: "v2", mapper: "erp-payment-mapper" },
    },
  },
  {
    id: "dispatch-to-settlement-queue",
    connector: "queue",
    externalId: "settlement-outbound",
    data: { queueName: "payments-outbound", priority: "high" },
  },
  {
    id: "platform-health-gate",
    connector: "platform_health",
    data: { check: "before-settlement" },
  },
];

const drasiEnhancedSteps: ScenarioStep[] = [
  {
    id: "capture-change-feed",
    connector: "rest",
    externalId: "orders-cdc-gateway",
    data: {
      method: "POST",
      body: { mode: "change-driven", source: "postgres-wal", table: "orders" },
    },
  },
  {
    id: "persist-change-snapshot",
    connector: "azure_blob",
    externalId: "audit-change-store",
    data: { container: "ledger-audit", retentionDays: 30 },
  },
  {
    id: "publish-drasi-signal",
    connector: "queue",
    externalId: "drasi-reaction-events",
    data: { eventType: "order.status.changed" },
  },
  {
    id: "platform-health-gate",
    connector: "platform_health",
    data: { check: "after-drasi-signal" },
  },
];

export const jobTemplates: JobTemplate[] = [
  {
    id: "inbound-payment-workflow",
    title: "Inbound payment workflow",
    description:
      "SFTP file intake, ERP transformation, queue dispatch, and platform health gate in a single replayable run.",
    jobId: "migration.biztalk.bank-file",
    payload: {
      sourceSystem: "BizTalk",
      targetSystem: "Execution Ledger",
      steps: bizTalkMigrationSteps,
      metadata: {
        migrationPattern: "file-drop-to-settlement",
        businessDomain: "payments",
      },
    },
    replayStepIds: ["transform-to-bank-format", "dispatch-to-settlement-queue"],
    compensate: {
      reason: "rollback outbound settlement payload",
      data: { action: "mark-settlement-cancelled", reasonCode: "DEMO_ROLLBACK" },
    },
  },
  {
    id: "order-change-reaction",
    title: "Order change reaction",
    description:
      "Change-capture orchestration with CDC signal publication, audit snapshotting, and downstream notification.",
    jobId: "drasi.orders.change-reaction",
    payload: {
      sourceSystem: "Postgres WAL",
      targetSystem: "Execution Ledger + Drasi",
      steps: drasiEnhancedSteps,
      metadata: {
        drasiQuery: "completed-job-runs",
        enhancement: "reactive-order-lifecycle-signals",
      },
    },
    replayStepIds: ["publish-drasi-signal"],
    compensate: {
      reason: "revoke drasi signal publication",
      data: { action: "publish-retraction-event", channel: "drasi-reaction-events" },
    },
  },
];

export function connectorSteps(template: JobTemplate): ScenarioStep[] {
  const raw = template.payload.steps;
  return Array.isArray(raw) ? (raw as ScenarioStep[]) : [];
}
