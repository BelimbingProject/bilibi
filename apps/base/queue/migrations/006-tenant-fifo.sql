CREATE INDEX base_queue_tenant_fifo ON base_queue_jobs (tenant_id, worker_id, id)
WHERE state IN ('available','scheduled','retryable','executing');
