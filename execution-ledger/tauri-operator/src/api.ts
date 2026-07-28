import type {
  ApiSession,
  JobRun,
  JobRunStep,
  PoisonDrainResult,
  RunLeaseRecoveryResult,
  ScheduleLeaseRecoveryResult,
  TriggerResult,
} from "./types";

const desktopRuntimeMessage =
  "Desktop runtime required. Open the desktop app to run operator commands.";

async function loadTauriCore() {
  const hasTauriRuntime =
    typeof window !== "undefined" &&
    "__TAURI_INTERNALS__" in window &&
    window.__TAURI_INTERNALS__ !== undefined;
  if (!hasTauriRuntime) {
    throw new Error(
      desktopRuntimeMessage,
    );
  }

  try {
    return await import("@tauri-apps/api/core");
  } catch {
    throw new Error(desktopRuntimeMessage);
  }
}

async function tauriInvoke<T>(command: string, payload?: Record<string, unknown>): Promise<T> {
  const tauriCore = await loadTauriCore();
  return tauriCore.invoke<T>(command, payload);
}

export async function getAppHealth(): Promise<Record<string, unknown>> {
  return tauriInvoke<Record<string, unknown>>("app_health");
}

export async function getApiHealth(session: ApiSession): Promise<Record<string, unknown>> {
  return tauriInvoke<Record<string, unknown>>("api_health", { session });
}

export async function listRuns(session: ApiSession, limit = 50): Promise<JobRun[]> {
  return tauriInvoke<JobRun[]>("api_list_runs", { session, limit });
}

export async function getRun(session: ApiSession, runId: string): Promise<JobRun> {
  return tauriInvoke<JobRun>("api_get_run", { session, runId });
}

export async function getRunSteps(session: ApiSession, runId: string): Promise<JobRunStep[]> {
  return tauriInvoke<JobRunStep[]>("api_get_run_steps", { session, runId });
}

export async function triggerJob(
  session: ApiSession,
  jobId: string,
  data: unknown,
  idempotencyKey?: string,
  correlationId?: string,
): Promise<TriggerResult> {
  return tauriInvoke<TriggerResult>("api_trigger_job", {
    args: { session, jobId, data, idempotencyKey, correlationId },
  });
}

export async function replayRun(
  session: ApiSession,
  runId: string,
  stepIds: string[],
): Promise<TriggerResult> {
  return tauriInvoke<TriggerResult>("api_replay_run", {
    args: { session, runId, stepIds },
  });
}

export async function skipRun(session: ApiSession, runId: string): Promise<Record<string, unknown>> {
  return tauriInvoke<Record<string, unknown>>("api_skip_run", { args: { session, runId } });
}

export async function compensateRun(
  session: ApiSession,
  runId: string,
  reason: string | undefined,
  data: unknown,
): Promise<TriggerResult> {
  return tauriInvoke<TriggerResult>("api_compensate_run", {
    args: { session, runId, reason, data },
  });
}

export async function recoverRunLeases(
  session: ApiSession,
  limit: number,
): Promise<RunLeaseRecoveryResult> {
  return tauriInvoke<RunLeaseRecoveryResult>("api_recover_run_leases", { session, limit });
}

export async function recoverScheduleLeases(
  session: ApiSession,
  limit: number,
): Promise<ScheduleLeaseRecoveryResult> {
  return tauriInvoke<ScheduleLeaseRecoveryResult>("api_recover_schedule_leases", { session, limit });
}

export async function drainLocalPoison(
  session: ApiSession,
  limit: number,
): Promise<PoisonDrainResult> {
  return tauriInvoke<PoisonDrainResult>("api_drain_local_poison", { session, limit });
}

export async function drainServiceBusPoison(
  session: ApiSession,
  limit: number,
): Promise<PoisonDrainResult> {
  return tauriInvoke<PoisonDrainResult>("api_drain_servicebus_poison", { session, limit });
}

export async function listSchedules(
  session: ApiSession,
  limit = 50,
): Promise<{ schedules: Record<string, unknown>[] }> {
  return tauriInvoke<{ schedules: Record<string, unknown>[] }>(
    "api_list_schedules",
    { session, limit },
  );
}

export async function registerSchedule(
  session: ApiSession,
  schedulerId: string,
  jobId: string,
  cronExpression: string,
  timezone?: string,
  nextRun?: string,
): Promise<{ scheduler_id: string; version: number }> {
  return tauriInvoke<{ scheduler_id: string; version: number }>(
    "api_register_schedule",
    {
      session,
      scheduler_id: schedulerId,
      job_id: jobId,
      cron_expression: cronExpression,
      timezone,
      next_run: nextRun,
    },
  );
}

export async function enableSchedule(
  session: ApiSession,
  schedulerId: string,
): Promise<Record<string, unknown>> {
  return tauriInvoke<Record<string, unknown>>("api_enable_schedule", {
    session,
    scheduler_id: schedulerId,
  });
}

export async function disableSchedule(
  session: ApiSession,
  schedulerId: string,
): Promise<Record<string, unknown>> {
  return tauriInvoke<Record<string, unknown>>("api_disable_schedule", {
    session,
    scheduler_id: schedulerId,
  });
}
