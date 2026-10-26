-- Replay records (schemas/replay/model-call.schema.json, ADR-0035): one row
-- per completed model call, keyed by its run and the seq of its
-- context_assembled event. Blobs are content-addressed and shared across
-- calls; content is JSON text whose canonical form hashes to the key.
CREATE TABLE replay_blobs (
    sha256 text COLLATE "C" PRIMARY KEY CHECK (sha256 ~ '^sha256:[0-9a-f]{64}$'),
    content text NOT NULL
);

CREATE TABLE replay_calls (
    run_id text COLLATE "C" NOT NULL,
    context_seq bigint NOT NULL CHECK (context_seq >= 1),
    record jsonb NOT NULL,
    PRIMARY KEY (run_id, context_seq)
);
