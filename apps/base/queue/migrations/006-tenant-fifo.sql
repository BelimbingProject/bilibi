ALTER TABLE base_queue_jobs ADD COLUMN fifo_key bigint;

CREATE INDEX base_queue_claim_ordinary ON base_queue_jobs (available_at, id)
WHERE fifo_key IS NULL AND state IN ('available','scheduled','retryable');

CREATE INDEX base_queue_tenant_fifo ON base_queue_jobs (tenant_id, worker_id, fifo_key, id)
WHERE fifo_key IS NOT NULL AND state IN ('available','scheduled','retryable','executing');
