-- Expand status CHECK constraints to include compensation-related values.
-- The worker sets 'compensated' and 'compensation_failed' on jobs_runs
-- and 'compensated' on steps_runs via the compensation path.

-- Drop and recreate the jobs_runs status constraint with the expanded list.
ALTER TABLE EXECUTION_LEDGER_jobs_runs
DROP CONSTRAINT IF EXISTS chk_execution_ledger_jobs_runs_status;

ALTER TABLE EXECUTION_LEDGER_jobs_runs
ADD CONSTRAINT chk_execution_ledger_jobs_runs_status
CHECK (status IN (
    'pending', 'running', 'completed', 'failed', 'cancelled',
    'compensated', 'compensation_failed'
));

-- Add a status CHECK constraint on steps_runs (was missing entirely).
-- Previously only output_status had a CHECK; status had no validation.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'chk_execution_ledger_steps_runs_status'
    ) THEN
        ALTER TABLE EXECUTION_LEDGER_steps_runs
        ADD CONSTRAINT chk_execution_ledger_steps_runs_status
        CHECK (status IN (
            'pending', 'running', 'completed', 'failed', 'skipped',
            'compensated'
        ));
    END IF;
END $$;

-- Add expires_at index for GC queries (may already exist, IF NOT EXISTS safe).
CREATE INDEX IF NOT EXISTS idx_EXECUTION_LEDGER_jobs_runs_expires_at_status
    ON EXECUTION_LEDGER_jobs_runs (expires_at)
    WHERE status IN ('completed', 'compensated', 'failed', 'cancelled', 'compensation_failed');
