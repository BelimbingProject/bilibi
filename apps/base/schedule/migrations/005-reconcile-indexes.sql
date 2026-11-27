-- #558: the reads each 15-second poll makes, indexed.
-- `reconcile_runs` scans the attempt rows still `running` and looks for a
-- later attempt of the same job.
CREATE INDEX base_schedule_runs_running ON base_schedule_runs (job_id) WHERE status = 'running';
CREATE INDEX base_schedule_runs_job_attempt ON base_schedule_runs (job_id, attempt) WHERE job_id IS NOT NULL;
-- A tenant administrator's Run now refuses while a manual run of the same key
-- is unfinished in that tenant.
CREATE INDEX base_schedule_tenant_runs_manual_unfinished
    ON base_schedule_tenant_runs (tenant_id, key) WHERE trigger = 'manual' AND finished_at IS NULL;
