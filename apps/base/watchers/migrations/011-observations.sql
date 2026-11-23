-- A run's AI observation (#863): asked once, before its conditions, and kept
-- so a retry reuses it. Pending while the provider works; never record text.
ALTER TABLE base_watcher_runs ADD COLUMN observation jsonb
    CHECK (observation IS NULL OR observation->>'state' IN ('pending','observed','unavailable'));

-- Observations a tenant has asked on `day` (UTC), against
-- watchers.ai_observations_per_day; the first spend of a new day restarts it.
CREATE TABLE base_watcher_ai_budget (
    tenant_id bigint PRIMARY KEY REFERENCES tenants(id),
    day date NOT NULL,
    used integer NOT NULL CHECK (used >= 0)
);
