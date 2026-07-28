import { useState, useEffect } from "react";
import { listSchedules, registerSchedule, enableSchedule, disableSchedule } from "../api";
import type { ApiSession } from "../types";
import { SectionHead } from "./SectionHead";

export function SchedulePanel({
  session, busy, runAction,
}: {
  session: ApiSession; busy: string;
  runAction: (id: string, fn: () => Promise<void>) => Promise<void>;
}) {
  const [schedules, setSchedules] = useState<Record<string, unknown>[]>([]);
  const [isRegistering, setIsRegistering] = useState(false);
  const [schedulerId, setSchedulerId] = useState("");
  const [jobId, setJobId] = useState("");
  const [cronExpression, setCronExpression] = useState("0 */4 * * *");
  const [timezone, setTimezone] = useState("UTC");
  const [loadingSchedules, setLoadingSchedules] = useState(false);

  async function refreshSchedules() {
    setLoadingSchedules(true);
    try {
      const result = await listSchedules(session);
      setSchedules(result.schedules || []);
    } finally {
      setLoadingSchedules(false);
    }
  }

  useEffect(() => {
    if (session.baseUrl && (session.bearerToken || session.gatewaySecret)) {
      refreshSchedules();
    }
  }, [session.baseUrl, session.bearerToken, session.gatewaySecret]);

  async function handleRegister() {
    if (!schedulerId.trim() || !jobId.trim() || !cronExpression.trim()) {
      alert("Scheduler ID, Job ID, and Cron expression are required.");
      return;
    }
    await runAction("schedule-register", async () => {
      await registerSchedule(session, schedulerId, jobId, cronExpression, timezone);
      setSchedulerId("");
      setJobId("");
      setCronExpression("0 */4 * * *");
      setTimezone("UTC");
      setIsRegistering(false);
      await refreshSchedules();
    });
  }

  async function handleToggle(schedulerId: string, enabled: boolean) {
    await runAction(`schedule-toggle-${schedulerId}`, async () => {
      if (enabled) {
        await disableSchedule(session, schedulerId);
      } else {
        await enableSchedule(session, schedulerId);
      }
      await refreshSchedules();
    });
  }

  return (
    <section className="panel" id="schedules">
      <SectionHead
        eyebrow="Automation"
        title="Job schedules"
        note="Register cron jobs to trigger automatically."
      />

      {/* Schedule list */}
      <div style={{ marginBottom: "2rem" }}>
        <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: "1rem" }}>
          <h3>Active schedules</h3>
          <button
            className="secondary"
            disabled={busy !== "" || loadingSchedules}
            onClick={() => refreshSchedules()}
          >
            {loadingSchedules ? "Loading..." : "Refresh"}
          </button>
        </div>

        {schedules.length === 0 ? (
          <p style={{ color: "var(--text-secondary)", fontSize: "0.875rem" }}>No schedules registered yet.</p>
        ) : (
          <table style={{ width: "100%", borderCollapse: "collapse" }}>
            <thead>
              <tr style={{ borderBottom: "1px solid var(--border-color)" }}>
                <th style={{ textAlign: "left", padding: "0.75rem", fontWeight: "600" }}>Scheduler ID</th>
                <th style={{ textAlign: "left", padding: "0.75rem", fontWeight: "600" }}>Job ID</th>
                <th style={{ textAlign: "left", padding: "0.75rem", fontWeight: "600" }}>Cron</th>
                <th style={{ textAlign: "left", padding: "0.75rem", fontWeight: "600" }}>Status</th>
                <th style={{ textAlign: "center", padding: "0.75rem", fontWeight: "600" }}>Action</th>
              </tr>
            </thead>
            <tbody>
              {schedules.map((schedule, idx) => {
                const sId = String(schedule.scheduler_id || "");
                const jId = String(schedule.job_id || "");
                const cron = String(schedule.cron_expression || "");
                const enabled = schedule.enabled !== false;
                return (
                  <tr key={idx} style={{ borderBottom: "1px solid var(--border-color)" }}>
                    <td style={{ padding: "0.75rem" }}><code>{sId}</code></td>
                    <td style={{ padding: "0.75rem" }}><code>{jId}</code></td>
                    <td style={{ padding: "0.75rem" }}><code>{cron}</code></td>
                    <td style={{ padding: "0.75rem" }}>
                      <span
                        style={{
                          display: "inline-block",
                          padding: "0.25rem 0.75rem",
                          borderRadius: "0.25rem",
                          fontSize: "0.75rem",
                          fontWeight: "600",
                          backgroundColor: enabled ? "var(--success-bg)" : "var(--neutral-bg)",
                          color: enabled ? "var(--success-text)" : "var(--neutral-text)",
                        }}
                      >
                        {enabled ? "ENABLED" : "DISABLED"}
                      </span>
                    </td>
                    <td style={{ padding: "0.75rem", textAlign: "center" }}>
                      <button
                        className="secondary"
                        disabled={busy !== ""}
                        onClick={() => handleToggle(sId, enabled)}
                        style={{ fontSize: "0.75rem", padding: "0.5rem 1rem" }}
                      >
                        {enabled ? "Disable" : "Enable"}
                      </button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>

      {/* Register form */}
      {!isRegistering ? (
        <button
          className="primary"
          disabled={busy !== ""}
          onClick={() => setIsRegistering(true)}
          style={{ marginTop: "1rem" }}
        >
          Register new schedule
        </button>
      ) : (
        <div
          style={{
            padding: "1.5rem",
            backgroundColor: "var(--panel-secondary-bg)",
            borderRadius: "0.5rem",
            marginTop: "1rem",
          }}
        >
          <h4 style={{ marginTop: 0 }}>Register schedule</h4>
          <label>
            Scheduler ID
            <input
              value={schedulerId}
              onChange={(e) => setSchedulerId(e.target.value)}
              placeholder="my-schedule-1"
              disabled={busy !== ""}
            />
          </label>
          <label>
            Job ID
            <input
              value={jobId}
              onChange={(e) => setJobId(e.target.value)}
              placeholder="migration.biztalk.bank-file"
              disabled={busy !== ""}
            />
          </label>
          <label>
            Cron expression
            <input
              value={cronExpression}
              onChange={(e) => setCronExpression(e.target.value)}
              placeholder="0 */4 * * *"
              disabled={busy !== ""}
            />
          </label>
          <label>
            Timezone
            <input
              value={timezone}
              onChange={(e) => setTimezone(e.target.value)}
              placeholder="UTC"
              disabled={busy !== ""}
            />
          </label>
          <div style={{ display: "flex", gap: "0.75rem", marginTop: "1rem" }}>
            <button
              className="primary"
              disabled={busy !== ""}
              onClick={handleRegister}
            >
              {busy === "schedule-register" ? "Registering..." : "Register"}
            </button>
            <button
              className="secondary"
              disabled={busy !== ""}
              onClick={() => setIsRegistering(false)}
            >
              Cancel
            </button>
          </div>
        </div>
      )}
    </section>
  );
}
