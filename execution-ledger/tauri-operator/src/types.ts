export interface ApiSession {
  baseUrl: string;
  bearerToken: string;
  gatewaySecret: string;
  apimClientId: string;
}

export interface JobRun {
  id: string;
  jobId: string;
  status: string;
  correlationId: string;
  createdAt?: string;
  updatedAt?: string;
  currentAttempt?: number;
  maxAttempts?: number;
}

export interface JobRunStep {
  stepId: string;
  externalId: string | null;
  stepIdempotencyKey: string | null;
  status: string;
  attempt: number;
  idx: number;
  output: unknown;
  error: unknown;
  createdAt: string;
  updatedAt: string;
}

export interface TriggerResult {
  accepted: boolean;
  intentId: string;
  runId: string;
  jobId: string;
  status: string;
  correlationId: string;
}

export interface RunLeaseRecoveryResult {
  requeuedPending: number;
  failedTerminal: number;
}

export interface ScheduleLeaseRecoveryResult {
  released: number;
}

export interface PoisonDrainResult {
  drained: number;
  engine: string;
}

export interface ScenarioStep {
  id: string;
  connector: "platform_health" | "rest" | "queue" | "blob" | "azure_blob" | "sftp" | "erp";
  externalId?: string;
  data?: Record<string, unknown>;
}

export interface JobTemplate {
  id: string;
  title: string;
  description: string;
  jobId: string;
  payload: Record<string, unknown>;
  replayStepIds: string[];
  compensate: {
    reason: string;
    data: Record<string, unknown>;
  };
}
