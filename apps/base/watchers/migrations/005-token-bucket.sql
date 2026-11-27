CREATE TABLE base_watcher_buckets (
    tenant_id bigint PRIMARY KEY REFERENCES tenants(id),
    tokens double precision NOT NULL CHECK (tokens >= 0 AND tokens <= 10000),
    refilled_at timestamptz NOT NULL
);
DROP INDEX base_watcher_runs_started;
