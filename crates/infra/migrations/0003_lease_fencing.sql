-- Lease fencing (ADR-0032). Each claim raises the epoch by one; a write fenced by an
-- older epoch is refused in its own transaction. Jobs claimed before this migration
-- start at 0 and get epoch 1 on their next claim.
ALTER TABLE jobs ADD COLUMN lease_epoch bigint NOT NULL DEFAULT 0 CHECK (lease_epoch >= 0);
