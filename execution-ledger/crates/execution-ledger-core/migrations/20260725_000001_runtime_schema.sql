CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE IF NOT EXISTS EXECUTION_LEDGER_tenants (
    id UUID PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    status VARCHAR(50) NOT NULL DEFAULT 'active',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS EXECUTION_LEDGER_jobs_runs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    idempotency_key UUID NOT NULL,
    correlation_id TEXT NOT NULL DEFAULT (gen_random_uuid()::text),
    job_id VARCHAR(255) NOT NULL,
    job_description VARCHAR(500),
    status VARCHAR(50) NOT NULL DEFAULT 'running'
        CHECK (status IN ('pending', 'running', 'completed', 'failed', 'cancelled')),
    input_data JSONB NOT NULL DEFAULT '{}',
    state_data JSONB NOT NULL DEFAULT '{}',
    state_version INTEGER NOT NULL DEFAULT 0,
    current_attempt INTEGER NOT NULL DEFAULT 1,
    max_attempts INTEGER NOT NULL DEFAULT 3,
    last_error JSONB,
    tenant_id UUID NOT NULL
        REFERENCES EXECUTION_LEDGER_tenants(id) ON DELETE RESTRICT,
    lease_owner VARCHAR(255),
    lease_expires_at TIMESTAMPTZ,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_jobs_runs_idempotency
    ON EXECUTION_LEDGER_jobs_runs (idempotency_key);
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_jobs_runs_job_id ON EXECUTION_LEDGER_jobs_runs (job_id);
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_jobs_runs_tenant_id ON EXECUTION_LEDGER_jobs_runs (tenant_id);
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_jobs_runs_correlation_id ON EXECUTION_LEDGER_jobs_runs (correlation_id);
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_jobs_runs_expires_at ON EXECUTION_LEDGER_jobs_runs (expires_at);
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_jobs_runs_lease_expires_at_running
    ON EXECUTION_LEDGER_jobs_runs (lease_expires_at)
    WHERE status = 'running';

CREATE TABLE IF NOT EXISTS EXECUTION_LEDGER_job_attempts (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    job_run_id UUID NOT NULL REFERENCES EXECUTION_LEDGER_jobs_runs(id) ON DELETE CASCADE,
    attempt_number INTEGER NOT NULL,
    status VARCHAR(50) NOT NULL,
    error JSONB,
    output_data JSONB,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    runner_id VARCHAR(255),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (job_run_id, attempt_number)
);
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_job_attempts_job_run_id ON EXECUTION_LEDGER_job_attempts (job_run_id);
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_job_attempts_status ON EXECUTION_LEDGER_job_attempts (status);

CREATE TABLE IF NOT EXISTS EXECUTION_LEDGER_steps_runs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    job_run_id UUID NOT NULL REFERENCES EXECUTION_LEDGER_jobs_runs(id) ON DELETE CASCADE,
    step_idempotency_key TEXT,
    step_id VARCHAR(255) NOT NULL,
    external_id VARCHAR(255),
    correlation_id TEXT,
    status VARCHAR(50) NOT NULL,
    output JSONB,
    error JSONB,
    output_status VARCHAR(50) NOT NULL DEFAULT 'computed'
        CHECK (output_status IN ('computed', 'memoized', 'offloaded', 'ignored')),
    attempt INTEGER NOT NULL DEFAULT 0,
    idx INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (job_run_id, step_id, external_id, attempt, idx)
);

CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_steps_runs_job_run_id ON EXECUTION_LEDGER_steps_runs (job_run_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_steps_runs_idempotency_key
    ON EXECUTION_LEDGER_steps_runs (step_idempotency_key)
    WHERE step_idempotency_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS EXECUTION_LEDGER_schedules (
    scheduler_id VARCHAR(64) PRIMARY KEY,
    schedule_version BIGINT NOT NULL CHECK (schedule_version > 0),
    job_id VARCHAR(255) NOT NULL,
    cron_expression VARCHAR(255) NOT NULL,
    timezone VARCHAR(64) NOT NULL DEFAULT 'UTC',
    next_run TIMESTAMPTZ NOT NULL,
    lease_until TIMESTAMPTZ,
    lease_owner VARCHAR(255),
    misfire_policy VARCHAR(32) NOT NULL DEFAULT 'SKIP',
    retry_count INTEGER NOT NULL DEFAULT 0,
    max_retries INTEGER NOT NULL DEFAULT 20,
    last_run_at TIMESTAMPTZ,
    last_queued_at TIMESTAMPTZ,
    last_error TEXT,
    status VARCHAR(32) NOT NULL DEFAULT 'ACTIVE',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (job_id, cron_expression, schedule_version)
);
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_schedules_next_run_active
    ON EXECUTION_LEDGER_schedules (next_run) WHERE status = 'ACTIVE';
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_schedules_lease_until_active
    ON EXECUTION_LEDGER_schedules (lease_until) WHERE status = 'ACTIVE';
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_schedules_status ON EXECUTION_LEDGER_schedules (status);

CREATE TABLE IF NOT EXISTS EXECUTION_LEDGER_scheduler_control (
    id BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id = TRUE),
    active_version BIGINT NOT NULL DEFAULT 1 CHECK (active_version > 0),
    next_version BIGINT NOT NULL DEFAULT 2 CHECK (next_version > active_version),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO EXECUTION_LEDGER_scheduler_control (id, active_version, next_version)
VALUES (TRUE, 1, 2)
ON CONFLICT (id) DO NOTHING;

CREATE TABLE IF NOT EXISTS EXECUTION_LEDGER_scheduler_ticks (
    id VARCHAR(36) PRIMARY KEY,
    lease_owner VARCHAR(255) NOT NULL,
    database_now TIMESTAMPTZ NOT NULL,
    due_count INTEGER NOT NULL DEFAULT 0,
    enqueued_count INTEGER NOT NULL DEFAULT 0,
    carryover_count INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_scheduler_ticks_created_at
    ON EXECUTION_LEDGER_scheduler_ticks (created_at);

CREATE TABLE IF NOT EXISTS EXECUTION_LEDGER_kv (
    key VARCHAR(255) PRIMARY KEY,
    value_json JSONB NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,
    expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_kv_expires_at ON EXECUTION_LEDGER_kv (expires_at);

CREATE TABLE IF NOT EXISTS EXECUTION_LEDGER_api_clients (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    client_id TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'disabled')),
    scopes JSONB NOT NULL CHECK (jsonb_typeof(scopes) = 'array'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS EXECUTION_LEDGER_local_intent_queue (
    id BIGSERIAL PRIMARY KEY,
    queue_name TEXT NOT NULL,
    intent_id TEXT NOT NULL UNIQUE,
    payload JSONB NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    available_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    lease_until TIMESTAMPTZ,
    dead_lettered_at TIMESTAMPTZ,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_local_intent_queue_created_at
    ON EXECUTION_LEDGER_local_intent_queue (created_at);
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_local_intent_queue_name_id
    ON EXECUTION_LEDGER_local_intent_queue (queue_name, id);
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_local_intent_queue_available
    ON EXECUTION_LEDGER_local_intent_queue (queue_name, available_at)
    WHERE dead_lettered_at IS NULL;
