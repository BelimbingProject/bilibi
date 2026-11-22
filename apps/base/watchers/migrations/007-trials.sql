-- Trial receipts record the request and aggregate outcome, never record facts.
CREATE TABLE base_watcher_trials (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 trigger varchar(200) NOT NULL,
 definition_digest char(64) NOT NULL,
 window_days integer NOT NULL,
 event_limit integer NOT NULL,
 examined integer NOT NULL,
 created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX base_watcher_trials_tenant ON base_watcher_trials (tenant_id, id DESC);
