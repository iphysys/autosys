-- The scheduler (ADR-0031). Beside each job: the run's continuation record,
-- written with the job by the fenced save; a requeue's not_before, on the
-- database clock; and the sequence number a wake-up scan starts after, set
-- by the save that parks the job.
ALTER TABLE jobs
    ADD COLUMN continuation jsonb,
    ADD COLUMN not_before timestamptz,
    ADD COLUMN wake_checked_seq bigint NOT NULL DEFAULT 0 CHECK (wake_checked_seq >= 0);

-- The wake-up scan reads parked jobs, least recently updated first.
CREATE INDEX jobs_parked ON jobs (updated_at, id) WHERE state IN ('waiting', 'suspended');

-- Job schema 0.2 gains failed_attempts and attempt_history.
UPDATE jobs SET document = document || '{"failed_attempts": 0, "attempt_history": []}'::jsonb
    WHERE NOT document ? 'failed_attempts';

-- A completion event is stored once per run (contracts::completion_key): an
-- append whose key exists returns the stored event. Events already stored
-- get their keys here; a run that already holds a duplicate fails the
-- unique index, and has to be repaired by hand.
--
-- The kind is generated from the payload, for reads of some kinds only.
ALTER TABLE events
    ADD COLUMN dedupe_key text,
    ADD COLUMN kind text GENERATED ALWAYS AS (document #>> '{payload,type}') STORED;

UPDATE events SET dedupe_key = CASE document #>> '{payload,type}'
    WHEN 'run_started' THEN 'run_started'
    WHEN 'tool_call_completed' THEN 'tool_call_completed:' || (document #>> '{payload,step_id}')
    WHEN 'sdr_transition' THEN 'sdr_transition:' || (document #>> '{payload,transition_id}')
    WHEN 'approval_requested' THEN 'approval_requested:' || (document #>> '{payload,approval_id}')
END;

CREATE UNIQUE INDEX events_completion ON events (run_id, dedupe_key) WHERE dedupe_key IS NOT NULL;
CREATE INDEX events_kind ON events (run_id, kind, seq);
