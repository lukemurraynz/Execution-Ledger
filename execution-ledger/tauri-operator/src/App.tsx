import { useEffect, useMemo, useRef, useState } from "react";
import {
  compensateRun,
  drainLocalPoison,
  drainServiceBusPoison,
  getApiHealth,
  getAppHealth,
  getRun,
  getRunSteps,
  listRuns,
  recoverRunLeases,
  recoverScheduleLeases,
  replayRun,
  skipRun,
  triggerJob,
} from "./api";
import "./App.css";
import { Pill } from "./components/Pill";
import { SectionHead } from "./components/SectionHead";
import { Modal } from "./components/Modal";
import { SchedulePanel } from "./components/SchedulePanel";
import { ToastProvider, useToast } from "./hooks/useToast";
import { useTheme } from "./hooks/useTheme";
import { connectorSteps, jobTemplates } from "./scenarios";
import type {
  ApiSession,
  JobRun,
  JobRunStep,
  RunLeaseRecoveryResult,
  JobTemplate,
  TriggerResult,
} from "./types";

// ── Helpers ──

function formatTimestamp(value?: string) {
  if (!value) return "—";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

function formatStatusLabel(value?: string) {
  return value ? value.replace(/_/g, " ") : "unknown";
}

function statusTone(value?: string) {
  const n = (value ?? "").toLowerCase();
  if (/(completed|running|active|healthy|ok|accepted)/.test(n)) return "success";
  if (/(failed|error|cancelled|blocked|dead)/.test(n)) return "danger";
  if (/(queued|pending|compensated|replaying|skipped)/.test(n)) return "warning";
  return "neutral";
}

function authLabel(session: ApiSession) {
  const hasBearer = session.bearerToken.trim().length > 0;
  const hasSecret = session.gatewaySecret.trim().length > 0;
  if (hasBearer && hasSecret) return "Bearer + APIM";
  if (hasSecret) return "APIM";
  if (hasBearer) return "Bearer";
  return "Not configured";
}

function ensureAuth(session: ApiSession) {
  if (!session.baseUrl.trim()) throw new Error("API base URL is required.");
  const hasBearer = session.bearerToken.trim().length > 0;
  const hasSecret = session.gatewaySecret.trim().length > 0;
  if (!hasBearer && !hasSecret) throw new Error("Set a bearer token or gateway secret.");
}

function scenarioMeta(scenario: JobTemplate): Array<[string, string]> {
  const m = scenario.payload.metadata;
  if (!m || typeof m !== "object" || Array.isArray(m)) return [];
  return Object.entries(m).map(([k, v]) => [k, typeof v === "string" ? v : JSON.stringify(v)]);
}

// ── SVG Icons (inline for zero-dependency) ──

const Icons = {
  dashboard:   <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8"><rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/></svg>,
  runs:        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8"><path d="M4 6h16M4 12h16M4 18h12"/></svg>,
  trigger:     <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8"><polygon points="5,3 19,12 5,21"/></svg>,
  actions:     <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/></svg>,
  recovery:    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8"><path d="M21.5 2v6h-6M2.5 22v-6h6M2 11.5a10 10 0 0 1 18.8-4.3M22 12.5a10 10 0 0 1-18.8 4.2"/></svg>,
  templates:   <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><polyline points="14,2 14,8 20,8"/><line x1="16" y1="13" x2="8" y2="13"/><line x1="16" y1="17" x2="8" y2="17"/><polyline points="10,9 9,9 8,9"/></svg>,
  connectors:  <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8"><rect x="2" y="2" width="8" height="8" rx="1"/><rect x="14" y="2" width="8" height="8" rx="1"/><rect x="8" y="14" width="8" height="8" rx="1"/><line x1="6" y1="10" x2="6" y2="18"/><line x1="18" y1="10" x2="18" y2="18"/></svg>,
  health:      <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8"><path d="M22 12h-4l-3 9L9 3l-3 9H2"/></svg>,
  search:      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/></svg>,
  chevronDown: <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><polyline points="6,9 12,15 18,9"/></svg>,
};

// ── Layout ──
function Sidebar({
  session, setSession, apiHealth, busy, activeSection, scrollTo,
  runAction, theme, toggleTheme,
}: {
  session: ApiSession; setSession: (s: ApiSession) => void;
  apiHealth: string; busy: string;
  activeSection: string; scrollTo: (id: string) => void;
  runAction: (id: string, fn: () => Promise<void>) => Promise<void>;
  theme: "light" | "dark"; toggleTheme: () => void;
}) {
  const navItems = [
    { id: "overview",    label: "Dashboard",   icon: Icons.dashboard },
    { id: "templates",   label: "Job templates", icon: Icons.templates },
    { id: "trigger",     label: "Manual trigger", icon: Icons.trigger },
    { id: "runs",        label: "Runs",          icon: Icons.runs },
    { id: "schedules",   label: "Schedules",     icon: Icons.trigger },
    { id: "actions",     label: "Actions",       icon: Icons.actions },
    { id: "recovery",    label: "Recovery",      icon: Icons.recovery },
  ];

  return (
    <aside className="sidebar">
      <div className="sidebar-content">
        <div className="brand-lockup">
          <svg className="brand-mark" viewBox="0 0 24 24" role="img" aria-label="Execution Ledger">
            <rect x="3" y="3" width="18" height="18" rx="4" />
            <path d="M8 8h8v2H8zm0 4h8v2H8zm0 4h5v2H8z" />
          </svg>
          <span className="brand-name">Execution Ledger</span>
          <button
            className="theme-toggle"
            onClick={toggleTheme}
            aria-label={theme === "light" ? "Switch to dark mode" : "Switch to light mode"}
            title={theme === "light" ? "Dark mode" : "Light mode"}
          >
            {theme === "light"
              ? <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/></svg>
              : <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><circle cx="12" cy="12" r="5"/><line x1="12" y1="1" x2="12" y2="3"/><line x1="12" y1="21" x2="12" y2="23"/><line x1="4.22" y1="4.22" x2="5.64" y2="5.64"/><line x1="18.36" y1="18.36" x2="19.78" y2="19.78"/><line x1="1" y1="12" x2="3" y2="12"/><line x1="21" y1="12" x2="23" y2="12"/><line x1="4.22" y1="19.78" x2="5.64" y2="18.36"/><line x1="18.36" y1="5.64" x2="19.78" y2="4.22"/></svg>
            }
          </button>
        </div>
        <p className="brand-subtitle">Operator console</p>

        <div className="pill-row" style={{ marginTop: 4, marginBottom: 4 }}>
          <Pill tone={statusTone(apiHealth)}>API {apiHealth || "idle"}</Pill>
        </div>

        {/* Navigation */}
        <div className="sidebar-section-title">Navigate</div>
        <nav className="rail-nav">
          {navItems.map((item) => (
            <button
              key={item.id}
              className={`rail-nav-item${activeSection === item.id ? " active" : ""}`}
              onClick={() => scrollTo(item.id)}
            >
              <span className="nav-icon">{item.icon}</span>
              {item.label}
            </button>
          ))}
        </nav>

        {/* Quick actions */}
        <div className="sidebar-section-title">Quick actions</div>
        <div className="rail-nav">
          <button
            className="rail-nav-item"
            disabled={busy !== ""}
            onClick={() => runAction("health", async () => {
              await getAppHealth();
            })}
          >
            <span className="nav-icon">{Icons.health}</span>
            {busy === "health" ? "Checking..." : "Check runtime"}
          </button>
          <button
            className="rail-nav-item"
            disabled={busy !== ""}
            onClick={() => runAction("api-check", async () => {
              await getApiHealth(session);
            })}
          >
            <span className="nav-icon">{Icons.health}</span>
            {busy === "api-check" ? "Checking..." : "Check API"}
          </button>
        </div>
      </div>

      {/* Connection settings */}
      <div className="sidebar-footer">
        <details className="collapsible">
          <summary>Connection</summary>
          <div className="collapsible-body settings-panel">
            <label>
              API URL
              <input
                value={session.baseUrl}
                onChange={(e) => setSession({ ...session, baseUrl: e.target.value })}
                placeholder="https://..."
              />
            </label>
            <label>
              Bearer token
              <input
                type="password"
                value={session.bearerToken}
                onChange={(e) => setSession({ ...session, bearerToken: e.target.value })}
                placeholder="eyJ..."
              />
            </label>
            <label>
              Gateway secret
              <input
                value={session.gatewaySecret}
                onChange={(e) => setSession({ ...session, gatewaySecret: e.target.value })}
                placeholder="Optional"
              />
            </label>
            <div className="auth-hint" data-danger={authLabel(session) === "Not configured"}>
              <strong>Auth:</strong> {authLabel(session)}
            </div>
          </div>
        </details>
      </div>
    </aside>
  );
}

// ── Dashboard ──
function Dashboard({
  sortedRuns, activeRuns, terminalRuns, templateCount, latestRun,
  activeTemplate, session,
}: {
  sortedRuns: JobRun[]; activeRuns: number; terminalRuns: number;
  templateCount: number; latestRun: JobRun | null;
  activeTemplate: JobTemplate; session: ApiSession;
}) {
  return (
    <section className="panel hero-panel" id="overview">
      <div className="hero-copy">
        <div className="eyebrow">Dashboard</div>
        <h1>Execution Ledger</h1>
        <p>
          Durable job orchestration for Azure. Manage runs, inspect step-level state,
          and control replay, skip, and compensation from one console.
        </p>
      </div>

      <div className="stats-grid">
        <article className="stat-card">
          <span>Total runs</span>
          <strong>{sortedRuns.length}</strong>
        </article>
        <article className="stat-card">
          <span>Active</span>
          <strong>{activeRuns}</strong>
        </article>
        <article className="stat-card">
          <span>Terminal</span>
          <strong>{terminalRuns}</strong>
        </article>
        <article className="stat-card">
          <span>Templates</span>
          <strong>{templateCount}</strong>
        </article>
      </div>

      <div className="pill-row">
        <Pill tone={statusTone(latestRun?.status)}>
          Latest: {latestRun ? formatStatusLabel(latestRun.status) : "none"}
        </Pill>
        <Pill>{activeTemplate.title}</Pill>
        <Pill tone={authLabel(session) === "Not configured" ? "danger" : "success"}>
          {authLabel(session)}
        </Pill>
        <Pill tone="neutral">{session.baseUrl}</Pill>
      </div>
    </section>
  );
}

// ── Job Templates ──
function JobTemplates({
  scenarios, activeId, busy, runAction, launchScenario,
}: {
  scenarios: JobTemplate[]; activeId: string; busy: string;
  runAction: (id: string, fn: () => Promise<void>) => Promise<void>;
  launchScenario: (s: JobTemplate) => Promise<void>;
}) {
  const active = scenarios.find((s) => s.id === activeId) ?? scenarios[0];

  return (
    <section className="panel" id="templates">
      <SectionHead
        eyebrow="Job templates"
        title="Pre-configured workflows"
        note="Each template exercises the full execution lifecycle with connector steps, replay, and compensation."
      />
      <div className="card-grid cols-2">
        {scenarios.map((s) => (
          <article key={s.id} className="card" data-active={s.id === active.id}>
            <div className="card-top">
              <h3>{s.title}</h3>
              {s.id === active.id && <Pill tone="success">Active</Pill>}
            </div>
            <p>{s.description}</p>
            <div className="pill-row">
              {scenarioMeta(s).map(([k, v]) => (
                <Pill key={`${s.id}-${k}`}>{k}: {v}</Pill>
              ))}
            </div>
            <p className="small">
              Job: <code>{s.jobId}</code>
            </p>
            <div className="step-chips">
              {connectorSteps(s).map((step) => (
                <span key={step.id} className="step-chip">
                  <code>{step.id}</code>
                  <em>{step.connector}</em>
                </span>
              ))}
            </div>
            <button
              disabled={busy !== ""}
              onClick={() => runAction(`template:${s.id}`, () => launchScenario(s))}
            >
              {busy === `template:${s.id}` ? "Launching..." : "Launch template"}
            </button>
          </article>
        ))}
      </div>
    </section>
  );
}

// ── Manual Trigger ──
function TriggerPanel({
  jobId, setJobId, payloadText, setPayloadText,
  idempotencyKey, setIdempotencyKey, correlationId, setCorrelationId,
  busy, runAction, triggerJobAction,
}: {
  jobId: string; setJobId: (v: string) => void;
  payloadText: string; setPayloadText: (v: string) => void;
  idempotencyKey: string; setIdempotencyKey: (v: string) => void;
  correlationId: string; setCorrelationId: (v: string) => void;
  busy: string; runAction: (id: string, fn: () => Promise<void>) => Promise<void>;
  triggerJobAction: () => Promise<void>;
}) {
  return (
    <section className="panel" id="trigger">
      <SectionHead
        eyebrow="Manual trigger"
        title="Start a run with raw JSON"
        note="Swap in a custom request body for ad hoc testing."
      />
      <div className="field-stack">
        <div className="field-grid">
          <label>
            Job ID
            <input value={jobId} onChange={(e) => setJobId(e.target.value)} />
          </label>
          <label>
            Idempotency key
            <input
              value={idempotencyKey}
              onChange={(e) => setIdempotencyKey(e.target.value)}
              placeholder="Auto-generated if empty"
            />
          </label>
          <label>
            Correlation ID
            <input
              value={correlationId}
              onChange={(e) => setCorrelationId(e.target.value)}
              placeholder="Run ID if empty"
            />
          </label>
        </div>
        <label className="textarea-label">
          Payload
          <textarea value={payloadText} onChange={(e) => setPayloadText(e.target.value)} />
        </label>
      </div>
      <div className="actions" style={{ marginTop: 12 }}>
        <button disabled={busy !== ""} onClick={() => runAction("trigger", triggerJobAction)}>
          {busy === "trigger" ? "Triggering..." : "Trigger job"}
        </button>
      </div>
    </section>
  );
}

// ── Run Table ──
function RunTable({
  visibleRuns, hasRuns, selectedRun, busy, runStatusFilter, setRunStatusFilter,
  runSearch, setRunSearch, loadRun, runAction,
}: {
  visibleRuns: JobRun[]; hasRuns: boolean;
  selectedRun: JobRun | null; busy: string;
  runStatusFilter: string; setRunStatusFilter: (v: "all" | "active" | "terminal") => void;
  runSearch: string; setRunSearch: (v: string) => void;
  loadRun: (id: string) => Promise<void>;
  runAction: (id: string, fn: () => Promise<void>) => Promise<void>;
}) {
  return (
    <section className="panel" id="runs">
      <SectionHead
        eyebrow="Runs"
        title="Recent executions"
        note="Open a run to inspect steps, replay, skip, or compensate."
      />
      <div className="run-controls">
        <label>
          Status
          <select
            value={runStatusFilter}
            onChange={(e) => setRunStatusFilter(e.target.value as "all" | "active" | "terminal")}
          >
            <option value="all">All runs</option>
            <option value="active">Active</option>
            <option value="terminal">Terminal</option>
          </select>
        </label>
        <label>
          Search
          <input
            value={runSearch}
            onChange={(e) => setRunSearch(e.target.value)}
            placeholder="Filter by run ID, job, status, or correlation ID"
          />
        </label>
      </div>

      {!hasRuns ? (
        <div className="empty">
          <div className="empty-icon">{Icons.runs}</div>
          <p>No runs loaded</p>
          <p className="empty-hint">Configure your connection and refresh, or launch a job template.</p>
        </div>
      ) : visibleRuns.length === 0 ? (
        <div className="empty">
          <p>No runs match the current filter</p>
        </div>
      ) : (
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Run ID</th>
                <th>Job</th>
                <th>Status</th>
                <th>Correlation</th>
                <th>Updated</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {visibleRuns.map((run) => (
                <tr key={run.id} className={selectedRun?.id === run.id ? "selected" : ""}>
                  <td><code>{run.id}</code></td>
                  <td>{run.jobId}</td>
                  <td><Pill tone={statusTone(run.status)}>{formatStatusLabel(run.status)}</Pill></td>
                  <td>{run.correlationId || "—"}</td>
                  <td>{formatTimestamp(run.updatedAt)}</td>
                  <td>
                    <button className="secondary" disabled={busy !== ""}
                      onClick={() => runAction(`open:${run.id}`, () => loadRun(run.id))}>
                      Open
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

// ── Actions Panel ──
function ActionPanel({
  selectedRun, steps, busy, replayStepIds, setReplayStepIds,
  compensateReason, setCompensateReason, compensateDataText, setCompensateDataText,
  runAction, refreshRuns, loadRun,
}: {
  selectedRun: JobRun | null; steps: JobRunStep[]; busy: string;
  replayStepIds: string; setReplayStepIds: (v: string) => void;
  compensateReason: string; setCompensateReason: (v: string) => void;
  compensateDataText: string; setCompensateDataText: (v: string) => void;
  runAction: (id: string, fn: () => Promise<void>) => Promise<void>;
  refreshRuns: () => Promise<void>; loadRun: (id: string) => Promise<void>;
}) {
  if (!selectedRun) {
    return (
      <section className="panel" id="actions">
        <SectionHead eyebrow="Actions" title="Replay, skip, compensate" note="Select a run first." />
        <div className="empty">
          <p>Open a run from the table above to control it</p>
        </div>
      </section>
    );
  }

  return (
    <section className="panel" id="actions">
      <SectionHead
        eyebrow="Actions"
        title="Replay, skip, compensate"
        note={`Operating on ${selectedRun.id}`}
      />
      <div className="card-grid cols-2">
        {/* Run detail */}
        <div className="card">
          <div className="card-top">
            <code>{selectedRun.id}</code>
            <Pill tone={statusTone(selectedRun.status)}>{formatStatusLabel(selectedRun.status)}</Pill>
          </div>
          <div className="detail-grid">
            <div><span>Job</span><strong>{selectedRun.jobId}</strong></div>
            <div><span>Correlation</span><strong>{selectedRun.correlationId || "—"}</strong></div>
            <div><span>Created</span><strong>{formatTimestamp(selectedRun.createdAt)}</strong></div>
            <div><span>Updated</span><strong>{formatTimestamp(selectedRun.updatedAt)}</strong></div>
            <div><span>Attempt</span><strong>{selectedRun.currentAttempt ?? "—"}</strong></div>
            <div><span>Max attempts</span><strong>{selectedRun.maxAttempts ?? "—"}</strong></div>
          </div>
          <label>
            Replay step IDs
            <input
              value={replayStepIds}
              onChange={(e) => setReplayStepIds(e.target.value)}
              placeholder="Comma-separated step IDs"
            />
          </label>
          <div className="actions">
            <button disabled={busy !== ""}
              onClick={() => runAction("replay", async () => {
                ensureAuth(appSession);
                const ids = replayStepIds.split(",").map((s) => s.trim()).filter(Boolean);
                await replayRun(appSession, selectedRun.id, ids);
                await refreshRuns();
                await loadRun(selectedRun.id);
              })}>
              {busy === "replay" ? "Replaying..." : "Replay"}
            </button>
            <button className="secondary" disabled={busy !== ""}
              onClick={() => runAction("skip", async () => {
                ensureAuth(appSession);
                await skipRun(appSession, selectedRun.id);
                await refreshRuns();
                await loadRun(selectedRun.id);
              })}>
              {busy === "skip" ? "Skipping..." : "Skip"}
            </button>
          </div>
        </div>

        {/* Compensate */}
        <div className="card">
          <div className="card-top"><h3>Compensate</h3></div>
          <label>
            Reason
            <input value={compensateReason} onChange={(e) => setCompensateReason(e.target.value)} />
          </label>
          <label className="textarea-label">
            Payload
            <textarea value={compensateDataText} onChange={(e) => setCompensateDataText(e.target.value)} />
          </label>
          <div className="actions">
            <button className="danger" disabled={busy !== ""}
              onClick={() => runAction("compensate", async () => {
                ensureAuth(appSession);
                const data = JSON.parse(compensateDataText.trim() || "{}");
                await compensateRun(appSession, selectedRun.id, compensateReason.trim() || undefined, data);
                await refreshRuns();
                await loadRun(selectedRun.id);
              })}>
              {busy === "compensate" ? "Compensating..." : "Compensate"}
            </button>
          </div>
        </div>
      </div>

      {/* Steps table */}
      {steps.length > 0 && (
        <div style={{ marginTop: 12 }}>
          <h3 style={{ fontSize: "0.88rem", color: "var(--el-color-text-muted)", marginBottom: 8 }}>Run steps</h3>
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>#</th><th>Step</th><th>Status</th><th>Attempt</th><th>Idempotency key</th>
                </tr>
              </thead>
              <tbody>
                {steps.map((step) => (
                  <tr key={`${step.stepId}-${step.idx}`}>
                    <td>{step.idx}</td>
                    <td><code>{step.stepId}</code></td>
                    <td>{step.status}</td>
                    <td>{step.attempt}</td>
                    <td><code>{step.stepIdempotencyKey || "—"}</code></td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </section>
  );
}

// ── Recovery Panel ──
function RecoveryPanel({
  busy, refreshRuns, steps, selectedRun, setReplayStepIds,
}: {
  busy: string;
  refreshRuns: () => Promise<void>;
  steps: JobRunStep[]; selectedRun: JobRun | null;
  setReplayStepIds: (v: string) => void;
}) {
  const [showConfirm, setShowConfirm] = useState<{ title: string; message: string; action: () => Promise<void> } | null>(null);

  return (
    <section className="panel" id="recovery">
      <SectionHead
        eyebrow="Recovery"
        title="Incident response"
        note="Lease recovery, poison queue drain, and guided troubleshooting."
      />
      <div className="card-grid cols-2">
        {[
          {
            title: "Run stuck",
            desc: "Worker may have crashed with locked leases.",
            action: () => setShowConfirm({
              title: "Recover run leases",
              message: "Release expired run leases for up to 500 runs? This can requeue or fail stuck runs.",
              action: async () => {
                ensureAuth(appSession);
                const r = await recoverRunLeases(appSession, 500);
                addToast(`Recovered ${(r as RunLeaseRecoveryResult).requeuedPending ?? 0} pending, ${(r as RunLeaseRecoveryResult).failedTerminal ?? 0} terminal`, "success");
                await refreshRuns();
              },
            }),
            btn: "Recover run leases",
          },
          {
            title: "Worker crashed",
            desc: "Both run and schedule leases may be locked.",
            action: () => setShowConfirm({
              title: "Full lease recovery",
              message: "Recover both run and schedule leases?",
              action: async () => {
                ensureAuth(appSession);
                await Promise.all([recoverRunLeases(appSession, 500), recoverScheduleLeases(appSession, 200)]);
                addToast("Leases recovered", "success");
                await refreshRuns();
              },
            }),
            btn: "Recover all leases",
          },
          {
            title: "Dead-letter queue",
            desc: "Poison messages need draining.",
            action: () => setShowConfirm({
              title: "Drain poison queues",
              message: "Drain local and Service Bus dead-letter messages? Irreversible.",
              action: async () => {
                ensureAuth(appSession);
                await Promise.all([drainLocalPoison(appSession, 500), drainServiceBusPoison(appSession, 200)]);
                addToast("Poison queues drained", "success");
              },
            }),
            btn: "Drain poison queues",
          },
          {
            title: "Configuration error",
            desc: "Step failed with invalid config.",
            action: () => {
              if (selectedRun) {
                const failed = steps.find((s) => s.status === "failed");
                if (failed) {
                  setReplayStepIds(failed.stepId);
                  document.getElementById("actions")?.scrollIntoView({ behavior: "smooth" });
                  addToast(`Focused failed step: ${failed.stepId}`, "info");
                } else {
                  addToast("No failed steps found in selected run", "info");
                }
              }
            },
            btn: "Find failed step",
          },
        ].map((item) => (
          <article key={item.title} className="card">
            <div className="card-top"><h3>{item.title}</h3></div>
            <p>{item.desc}</p>
            <div className="actions">
              <button className="secondary" disabled={busy !== ""} onClick={item.action}>
                {item.btn}
              </button>
            </div>
          </article>
        ))}
      </div>

      {showConfirm && (
        <Modal
          open={true}
          onClose={() => setShowConfirm(null)}
          title={showConfirm.title}
          footer={
            <>
              <button className="secondary" onClick={() => setShowConfirm(null)}>Cancel</button>
              <button className="danger" onClick={async () => {
                await showConfirm.action();
                setShowConfirm(null);
              }}>Confirm</button>
            </>
          }
        >
          <p>{showConfirm.message}</p>
        </Modal>
      )}
    </section>
  );
}

// ── Command Palette ──
function CommandPalette({
  open, onClose, actions,
}: {
  open: boolean; onClose: () => void;
  actions: Array<{ id: string; label: string; section: string; shortcut?: string; action: () => void }>;
}) {
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);

  const filtered = useMemo(() => {
    if (!query.trim()) return actions;
    const q = query.toLowerCase();
    return actions.filter((a) => a.label.toLowerCase().includes(q) || a.section.toLowerCase().includes(q));
  }, [query, actions]);

  useEffect(() => {
    if (open) {
      setQuery("");
      setSelected(0);
      setTimeout(() => inputRef.current?.focus(), 50);
    }
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
      if (e.key === "ArrowDown") { e.preventDefault(); setSelected((s) => Math.min(s + 1, filtered.length - 1)); }
      if (e.key === "ArrowUp") { e.preventDefault(); setSelected((s) => Math.max(s - 1, 0)); }
      if (e.key === "Enter" && filtered[selected]) {
        filtered[selected].action();
        onClose();
      }
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [open, onClose, filtered, selected]);

  if (!open) return null;

  return (
    <div className="palette-overlay" onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <div className="palette">
        <div className="palette-input-wrap">
          <input
            ref={inputRef}
            className="palette-input"
            placeholder="Type a command..."
            value={query}
            onChange={(e) => { setQuery(e.target.value); setSelected(0); }}
          />
        </div>
        <div className="palette-results">
          {filtered.length === 0 ? (
            <div className="palette-empty">No commands found</div>
          ) : (
            filtered.map((a, i) => (
              <div
                key={a.id}
                className={`palette-item${i === selected ? " selected" : ""}`}
                onClick={() => { a.action(); onClose(); }}
                onMouseEnter={() => setSelected(i)}
              >
                {a.label}
                <span className="small" style={{ marginLeft: 8, color: "var(--el-color-text-muted)" }}>
                  {a.section}
                </span>
                {a.shortcut && <kbd>{a.shortcut}</kbd>}
              </div>
            ))
          )}
        </div>
      </div>
    </div>
  );
}

// ── Module-level session reference (single ref, avoids prop-drilling through actions) ──
let appSession: ApiSession = { baseUrl: "", bearerToken: "", gatewaySecret: "", apimClientId: "execution-ledger-apim" };
let addToast: (msg: string, type?: "success" | "error" | "info") => void = () => {};

// ── Main App ──

function AppInner() {
  const toast = useToast();
  const { theme, toggle: toggleTheme } = useTheme();
  addToast = toast.addToast;

  const [session, setSession] = useState<ApiSession>({
    baseUrl: import.meta.env.VITE_EXECUTION_LEDGER_API_URL ?? "http://localhost:3000",
    bearerToken: "",
    gatewaySecret: "",
    apimClientId: "execution-ledger-apim",
  });
  appSession = session;

  const [apiHealth, setApiHealth] = useState("");
  const [busy, setBusy] = useState("");
  const [activeSection, setActiveSection] = useState("overview");

  const [jobId, setJobId] = useState("migration.biztalk.bank-file");
  const [payloadText, setPayloadText] = useState(JSON.stringify(jobTemplates[0].payload, null, 2));
  const [idempotencyKey, setIdempotencyKey] = useState("");
  const [correlationId, setCorrelationId] = useState("");
  const [replayStepIds, setReplayStepIds] = useState("");
  const [compensateReason, setCompensateReason] = useState("Manual compensate");
  const [compensateDataText, setCompensateDataText] = useState('{"reasonCode":"MANUAL"}');
  const [activeScenarioId, setActiveScenarioId] = useState(jobTemplates[0].id);

  const [runs, setRuns] = useState<JobRun[]>([]);
  const [selectedRun, setSelectedRun] = useState<JobRun | null>(null);
  const [steps, setSteps] = useState<JobRunStep[]>([]);
  const [lastActionResult, setLastActionResult] = useState<TriggerResult | null>(null);
  const [runStatusFilter, setRunStatusFilter] = useState<"all" | "active" | "terminal">("all");
  const [runSearch, setRunSearch] = useState("");
  const [paletteOpen, setPaletteOpen] = useState(false);

  const sortedRuns = useMemo(() => [...runs].sort((a, b) => (b.updatedAt ?? "").localeCompare(a.updatedAt ?? "")), [runs]);
  const activeTemplate = jobTemplates.find((s) => s.id === activeScenarioId) ?? jobTemplates[0];
  const latestRun = sortedRuns[0] ?? null;
  const activeRuns = sortedRuns.filter((r) => /(running|active|queued|pending)/i.test(r.status)).length;
  const terminalRuns = sortedRuns.filter((r) => /(completed|failed|cancelled|compensated|skipped)/i.test(r.status)).length;
  const hasRuns = sortedRuns.length > 0;
  const visibleRuns = sortedRuns.filter((r) => {
    const statusMatch = runStatusFilter === "all" ? true
      : runStatusFilter === "active" ? /(running|active|queued|pending)/i.test(r.status)
        : /(completed|failed|cancelled|compensated|skipped)/i.test(r.status);
    const q = runSearch.trim().toLowerCase();
    if (!statusMatch || !q) return statusMatch;
    return [r.id, r.jobId, r.status, r.correlationId].some((v) => String(v ?? "").toLowerCase().includes(q));
  });

  // Auto-poll runs every 8s
  useEffect(() => {
    const hasAuth = session.bearerToken.trim() || session.gatewaySecret.trim();
    if (!hasAuth || !session.baseUrl.trim()) return;
    const interval = setInterval(async () => {
      try { setRuns(await listRuns(session, 100)); } catch { /* silent */ }
    }, 8000);
    return () => clearInterval(interval);
  }, [session.baseUrl, session.bearerToken, session.gatewaySecret]);

  // Keyboard: Ctrl+K for palette
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key === "k") {
        e.preventDefault();
        setPaletteOpen(true);
      }
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, []);

  // Scroll observer for active section
  useEffect(() => {
    const sections = ["overview", "templates", "trigger", "runs", "schedules", "actions", "recovery"];
    const observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting) { setActiveSection(entry.target.id); break; }
        }
      },
      { rootMargin: "-80px 0px -60% 0px" },
    );
    sections.forEach((id) => {
      const el = document.getElementById(id);
      if (el) observer.observe(el);
    });
    return () => observer.disconnect();
  }, [hasRuns]);

  async function runAction(id: string, fn: () => Promise<void>) {
    setBusy(id);
    try {
      await fn();
    } catch (e) {
      toast.addToast(e instanceof Error ? e.message : String(e), "error");
    } finally {
      setBusy("");
    }
  }

  async function refreshRuns() {
    ensureAuth(session);
    setRuns(await listRuns(session, 100));
  }

  async function loadRun(runId: string) {
    ensureAuth(session);
    const [run, stepRows] = await Promise.all([getRun(session, runId), getRunSteps(session, runId)]);
    setSelectedRun(run);
    setSteps(stepRows);
    document.getElementById("actions")?.scrollIntoView({ behavior: "smooth" });
  }

  async function launchScenario(s: JobTemplate) {
    ensureAuth(session);
    setActiveScenarioId(s.id);
    setJobId(s.jobId);
    setPayloadText(JSON.stringify(s.payload, null, 2));
    setReplayStepIds(s.replayStepIds.join(","));
    setCompensateReason(s.compensate.reason);
    setCompensateDataText(JSON.stringify(s.compensate.data, null, 2));
    const result = await triggerJob(session, s.jobId, s.payload, idempotencyKey.trim() || undefined, correlationId.trim() || undefined);
    setLastActionResult(result);
    toast.addToast(`Run ${result.runId} triggered`, "success");
    await refreshRuns();
    await loadRun(result.runId);
  }

  async function triggerJobAction() {
    ensureAuth(session);
    const payload = JSON.parse(payloadText.trim() || "{}");
    const result = await triggerJob(session, jobId, payload, idempotencyKey.trim() || undefined, correlationId.trim() || undefined);
    setLastActionResult(result);
    toast.addToast(`Run ${result.runId} triggered`, "success");
    await refreshRuns();
    await loadRun(result.runId);
  }

  function scrollTo(id: string) {
    document.getElementById(id)?.scrollIntoView({ behavior: "smooth" });
  }

  // Palette actions
  const paletteActions = useMemo(() => [
    { id: "refresh", label: "Refresh runs", section: "Runs", shortcut: "Ctrl+R", action: () => runAction("palette-refresh", refreshRuns) },
    { id: "trigger", label: "Trigger job", section: "Actions", action: () => runAction("palette-trigger", triggerJobAction) },
    { id: "goto-templates", label: "Job templates", section: "Navigate", action: () => scrollTo("templates") },
    { id: "goto-runs", label: "Run list", section: "Navigate", action: () => scrollTo("runs") },
    { id: "goto-actions", label: "Actions panel", section: "Navigate", action: () => scrollTo("actions") },
    { id: "goto-recovery", label: "Recovery panel", section: "Navigate", action: () => scrollTo("recovery") },
    { id: "goto-overview", label: "Dashboard", section: "Navigate", action: () => scrollTo("overview") },
    { id: "check-health", label: "Check API health", section: "Diagnostics", action: () => runAction("palette-health", async () => {
      ensureAuth(session);
      const r = await getApiHealth(session);
      setApiHealth(String(r.status ?? "ok"));
      toast.addToast(`API health: ${r.status}`, "success");
    })},
    { id: "recover-runs", label: "Recover run leases", section: "Recovery", action: () => runAction("palette-recover", async () => {
      ensureAuth(session);
      await recoverRunLeases(session, 500);
      toast.addToast("Run leases recovered", "success");
      await refreshRuns();
    })},
  ], [refreshRuns, triggerJobAction, session]);

  return (
    <>
      <a className="skip-link" href="#overview">Skip to content</a>
      <div className="app-shell">
        <Sidebar
          session={session} setSession={setSession}
          apiHealth={apiHealth} busy={busy}
          activeSection={activeSection} scrollTo={scrollTo}
          runAction={runAction}
          theme={theme} toggleTheme={toggleTheme}
        />
        <main className="workspace">
          <Dashboard
            sortedRuns={sortedRuns} activeRuns={activeRuns} terminalRuns={terminalRuns}
            templateCount={jobTemplates.length} latestRun={latestRun}
            activeTemplate={activeTemplate} session={session}
          />

          <JobTemplates
            scenarios={jobTemplates} activeId={activeScenarioId}
            busy={busy} runAction={runAction} launchScenario={launchScenario}
          />

          <TriggerPanel
            jobId={jobId} setJobId={setJobId}
            payloadText={payloadText} setPayloadText={setPayloadText}
            idempotencyKey={idempotencyKey} setIdempotencyKey={setIdempotencyKey}
            correlationId={correlationId} setCorrelationId={setCorrelationId}
            busy={busy} runAction={runAction} triggerJobAction={triggerJobAction}
          />

          <SchedulePanel
            session={session} busy={busy} runAction={runAction}
          />

          <RunTable
            visibleRuns={visibleRuns} hasRuns={hasRuns}
            selectedRun={selectedRun} busy={busy}
            runStatusFilter={runStatusFilter} setRunStatusFilter={setRunStatusFilter}
            runSearch={runSearch} setRunSearch={setRunSearch}
            loadRun={loadRun} runAction={runAction}
          />

          <ActionPanel
            selectedRun={selectedRun} steps={steps} busy={busy}
            replayStepIds={replayStepIds} setReplayStepIds={setReplayStepIds}
            compensateReason={compensateReason} setCompensateReason={setCompensateReason}
            compensateDataText={compensateDataText} setCompensateDataText={setCompensateDataText}
            runAction={runAction} refreshRuns={refreshRuns} loadRun={loadRun}
          />

          <RecoveryPanel
            busy={busy} refreshRuns={refreshRuns}
            steps={steps} selectedRun={selectedRun} setReplayStepIds={setReplayStepIds}
          />

          {/* Connector reference */}
          <section className="panel" id="connectors">
            <details className="collapsible">
              <summary>Connector reference</summary>
              <div className="collapsible-body">
                <SectionHead
                  eyebrow="Connectors"
                  title="Execution paths"
                  note="Each card maps to the connector execution boundaries."
                />
                <div className="card-grid cols-2">
                  {jobTemplates.map((s) => (
                    <article key={`${s.id}-conn`} className="card">
                      <h3>{s.title}</h3>
                      <div className="step-chips">
                        {connectorSteps(s).map((step) => (
                          <span key={step.id} className="step-chip">
                            <code>{step.id}</code>
                            <em>{step.connector}</em>
                          </span>
                        ))}
                      </div>
                    </article>
                  ))}
                </div>
              </div>
            </details>
          </section>

          {lastActionResult && (
            <section className="panel">
              <h2>Last result</h2>
              <pre>{JSON.stringify(lastActionResult, null, 2)}</pre>
            </section>
          )}
        </main>
      </div>

      <CommandPalette
        open={paletteOpen}
        onClose={() => setPaletteOpen(false)}
        actions={paletteActions}
      />
    </>
  );
}

// ── Root ──
export default function App() {
  return (
    <ToastProvider>
      <AppInner />
    </ToastProvider>
  );
}
