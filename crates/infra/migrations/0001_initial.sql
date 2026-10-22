-- Durable storage for jobs, events, SDR versions, evidence and artifact
-- metadata (ADR-0016). Applied by `infra::persistence::Database::migrate`.
-- A migration never changes after it is merged; add a new one instead.
--
-- Identifiers use the "C" collation so they order bytewise, the same way in
-- every database and in the in-memory stores.

-- The stores' only time source (ADR-0023). Lease expiry and every timestamp a
-- store sets come from the database clock, so workers on different hosts
-- agree on time whatever their own clocks say. Tests may redefine it in a
-- throwaway database; nothing else does.
CREATE FUNCTION autosys_now() RETURNS timestamptz
    LANGUAGE sql STABLE
    AS $$ SELECT now() $$;

-- Jobs (schemas/jobs/job.schema.json). `document` holds the job as last
-- written by insert or save. The columns after it are the source of truth for
-- the fields the store manages: state, attempt, updated_at, wake condition and
-- lease.
CREATE TABLE jobs (
    id text COLLATE "C" PRIMARY KEY,
    run_id text COLLATE "C" NOT NULL,
    created_at timestamptz NOT NULL,
    document jsonb NOT NULL,
    state text NOT NULL CHECK (
        state IN ('queued', 'running', 'waiting', 'suspended', 'completed', 'failed', 'cancelled')
    ),
    attempt bigint NOT NULL CHECK (attempt >= 0),
    updated_at timestamptz NOT NULL,
    wake_condition jsonb,
    lease_holder text COLLATE "C",
    lease_acquired_at timestamptz,
    lease_expires_at timestamptz,
    lease_heartbeat_at timestamptz,
    -- A lease is all or nothing, and only a running job holds one.
    CHECK (
        (lease_holder IS NULL AND lease_acquired_at IS NULL
            AND lease_expires_at IS NULL AND lease_heartbeat_at IS NULL)
        OR (state = 'running' AND lease_holder IS NOT NULL AND lease_acquired_at IS NOT NULL
            AND lease_expires_at IS NOT NULL AND lease_heartbeat_at IS NOT NULL)
    )
);

CREATE INDEX jobs_run_id ON jobs (run_id);
CREATE INDEX jobs_claimable ON jobs (created_at, id) WHERE state IN ('queued', 'running');

-- The last sequence number of each run. An append locks its run's row until
-- commit, so a run's events commit in sequence order: a reader never sees
-- n + 1 before n, and a rolled-back append leaves no gap.
CREATE TABLE event_sequences (
    run_id text COLLATE "C" PRIMARY KEY,
    last_seq bigint NOT NULL CHECK (last_seq > 0)
);

-- Events (schemas/events/event.schema.json); append-only.
CREATE TABLE events (
    run_id text COLLATE "C" NOT NULL REFERENCES event_sequences (run_id),
    seq bigint NOT NULL CHECK (seq > 0),
    document jsonb NOT NULL,
    PRIMARY KEY (run_id, seq)
);

-- Every stored event notifies channel `autosys_events` with its run ID. The
-- notification is delivered at commit, so a woken reader finds the event. The
-- channel name is repeated in crates/infra/src/persistence/events.rs.
CREATE FUNCTION notify_event_appended() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM pg_notify('autosys_events', NEW.run_id);
    RETURN NULL;
END
$$;

CREATE TRIGGER events_notify AFTER INSERT ON events
    FOR EACH ROW EXECUTE FUNCTION notify_event_appended();

-- SDR versions (ADR-0020): linear per project, written only through
-- SdrManager. Version 0 has no parent; every later version's parent is the
-- version before it.
CREATE TABLE sdr_versions (
    project_id text COLLATE "C" NOT NULL,
    version bigint NOT NULL CHECK (version >= 0),
    parent_version bigint,
    document jsonb NOT NULL,
    record jsonb NOT NULL,
    PRIMARY KEY (project_id, version),
    FOREIGN KEY (project_id, parent_version) REFERENCES sdr_versions (project_id, version),
    CHECK (
        (version = 0 AND parent_version IS NULL)
        OR (version > 0 AND parent_version = version - 1)
    )
);

-- Evidence (schemas/sdr/evidence.schema.json); immutable once stored.
CREATE TABLE evidence (
    id text COLLATE "C" PRIMARY KEY,
    run_id text COLLATE "C" NOT NULL,
    created_at timestamptz NOT NULL,
    document jsonb NOT NULL
);

CREATE INDEX evidence_run ON evidence (run_id, created_at, id);

-- Artifact metadata (schemas/artifacts/artifact.schema.json). The content is
-- a file on the artifact volume, named by its sha256.
CREATE TABLE artifacts (
    id text COLLATE "C" PRIMARY KEY,
    run_id text COLLATE "C" NOT NULL,
    created_at timestamptz NOT NULL,
    sha256 text NOT NULL CHECK (sha256 ~ '^sha256:[0-9a-f]{64}$'),
    size_bytes bigint NOT NULL CHECK (size_bytes >= 0),
    metadata jsonb NOT NULL
);

CREATE INDEX artifacts_run ON artifacts (run_id, created_at, id);
