-- Batch-window aggregation primitive: correlates arrivals for a shared
-- window_key (e.g. "farmlands:branch-014") over a configurable time window
-- (or until an expected_count is reached), then releases the collected
-- items as a single job trigger. Reusable equivalent of BizTalk's
-- long-running-orchestration correlation timer.
CREATE TABLE IF NOT EXISTS EXECUTION_LEDGER_batch_windows (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID REFERENCES EXECUTION_LEDGER_tenants(id) ON DELETE SET NULL,
    window_key VARCHAR(255) NOT NULL,
    job_id VARCHAR(255) NOT NULL,
    window_minutes INTEGER NOT NULL CHECK (window_minutes > 0),
    expected_count INTEGER CHECK (expected_count IS NULL OR expected_count > 0),
    item_count INTEGER NOT NULL DEFAULT 0,
    items JSONB NOT NULL DEFAULT '[]',
    first_arrival_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    window_ends_at TIMESTAMPTZ NOT NULL,
    status VARCHAR(32) NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'released')),
    released_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Only one open window per window_key at a time — concurrent arrivals for
-- the same branch correlate into one row instead of racing to create
-- duplicates. The key is reusable once released (next day's window reopens).
CREATE UNIQUE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_batch_windows_open_key
    ON EXECUTION_LEDGER_batch_windows (window_key)
    WHERE status = 'open';

-- Drasi's due-batch-windows query scans exactly this predicate.
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_batch_windows_due
    ON EXECUTION_LEDGER_batch_windows (window_ends_at)
    WHERE status = 'open';
