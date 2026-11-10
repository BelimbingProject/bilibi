-- The Queue job a `queued` email request waits on, so that when Queue ends the
-- job without settling the row (the sender lost the right to share first, or
-- the job ran out of attempts) its ending hook settles that request `failed`,
-- and an ending told late for an earlier job changes nothing. Set when the job
-- is queued; never read for anything else.
ALTER TABLE core_document_emails ADD COLUMN job_id bigint;
