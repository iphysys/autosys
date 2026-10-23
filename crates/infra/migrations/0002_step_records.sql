-- Step records (schemas/jobs/step-record.schema.json, ADR-0026): one row per
-- tool call, keyed by its idempotency key. A row without an outcome is a call
-- that started and has not finished. Timestamps come from autosys_now()
-- (ADR-0023).
CREATE TABLE step_records (
    idempotency_key text COLLATE "C" PRIMARY KEY,
    run_id text COLLATE "C" NOT NULL,
    step_id text COLLATE "C" NOT NULL,
    tool text COLLATE "C" NOT NULL,
    request_sha256 text NOT NULL CHECK (request_sha256 ~ '^sha256:[0-9a-f]{64}$'),
    started_at timestamptz NOT NULL,
    completed_at timestamptz,
    outcome jsonb,
    -- The outcome is written once, together with its time.
    CHECK ((completed_at IS NULL) = (outcome IS NULL))
);

CREATE INDEX step_records_run ON step_records (run_id, started_at);
