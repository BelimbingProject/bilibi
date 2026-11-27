-- A worker's owner is told when its job ends without finishing (the
-- `base/queue.endings` hook): NULL while a cancelled or discarded job of such
-- a worker still waits to be told, 'told' once the owner recorded it, and
-- 'failed' once the owner's hook kept failing until Queue stopped asking.
-- Jobs of workers without a hook are never read for this.
ALTER TABLE base_queue_jobs
    ADD COLUMN ending text CHECK (ending IN ('told', 'failed')),
    ADD COLUMN ending_attempts integer NOT NULL DEFAULT 0 CHECK (ending_attempts >= 0),
    ADD COLUMN ending_after timestamptz,
    ADD CONSTRAINT base_queue_jobs_ending_state CHECK (ending IS NULL OR state IN ('cancelled', 'discarded'));
CREATE INDEX base_queue_jobs_endings ON base_queue_jobs (worker_id, id)
    WHERE state IN ('cancelled', 'discarded') AND ending IS NULL;
