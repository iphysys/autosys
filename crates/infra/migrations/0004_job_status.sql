-- Job schema 0.2: a job's status is one tagged object in the document,
-- `status: {state, ...what that state needs}`, and `state` becomes a column
-- generated from its tag, so the index the claims and the wake scan use can
-- never disagree with the document. The wake condition moves into the
-- status, so its column goes.
--
-- Only queued and running jobs can move: every other state's status names a
-- checkpoint, which no job written before this migration has.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM jobs WHERE state NOT IN ('queued', 'running')) THEN
        RAISE EXCEPTION 'jobs in states other than queued and running have no checkpoint '
            'and cannot move to job schema 0.2; delete them first';
    END IF;
END
$$;

UPDATE jobs SET document = (document - 'state' - 'wake_condition' - 'stop_reason')
    || jsonb_build_object('schema_version', '0.2', 'status', jsonb_build_object('state', state));

-- Dropping the column drops the index and the lease check that use it.
ALTER TABLE jobs DROP COLUMN wake_condition;
ALTER TABLE jobs DROP COLUMN state;

ALTER TABLE jobs ADD COLUMN state text GENERATED ALWAYS AS (document #>> '{status,state}') STORED
    NOT NULL CHECK (
        state IN ('queued', 'running', 'waiting', 'suspended', 'completed', 'failed', 'cancelled')
    );

-- A lease is all or nothing, and only a running job holds one.
ALTER TABLE jobs ADD CONSTRAINT jobs_lease_check CHECK (
    (lease_holder IS NULL AND lease_acquired_at IS NULL
        AND lease_expires_at IS NULL AND lease_heartbeat_at IS NULL)
    OR (state = 'running' AND lease_holder IS NOT NULL AND lease_acquired_at IS NOT NULL
        AND lease_expires_at IS NOT NULL AND lease_heartbeat_at IS NOT NULL)
);

CREATE INDEX jobs_claimable ON jobs (created_at, id) WHERE state IN ('queued', 'running');
