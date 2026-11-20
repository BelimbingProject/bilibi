DROP TABLE base_rule_effects, base_rule_runs, base_rule_versions, base_rules;

-- Tenant watchers as data (ADR 28, #682). A watcher names a trigger (an event an
-- owner offers) and points at the one approved version that runs; versions
-- are never changed once written. Runs and effects are the receipts.
CREATE TABLE base_watchers (
 id bigserial PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 name varchar(120) NOT NULL CHECK (btrim(name) <> ''),
 trigger varchar(200) NOT NULL,
 -- The approved version that runs; NULL until one is approved.
 active_version_id bigint,
 -- Set while the watcher is switched off; an approved version stays approved.
 disabled_at timestamptz,
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now(),
 version integer NOT NULL DEFAULT 1,
 UNIQUE (tenant_id, id)
);
CREATE INDEX base_watchers_trigger ON base_watchers (tenant_id, trigger) WHERE active_version_id IS NOT NULL AND disabled_at IS NULL;

CREATE TABLE base_watcher_versions (
 id bigserial PRIMARY KEY,
 tenant_id bigint NOT NULL,
 watcher_id bigint NOT NULL,
 number integer NOT NULL CHECK (number > 0),
 -- {"if": {"match": "all"|"any", "conditions": [...]}, "do": [...]}.
 definition jsonb NOT NULL,
 examples jsonb NOT NULL,
 explanation varchar(2000),
 -- SHA-256 of the definition and examples as stored.
 digest varchar(64) NOT NULL,
 -- `person` or `agent`: how it was drafted, for the reviewer. The account
 -- IDs are evidence, not foreign keys: Base names no Core table.
 drafted_via varchar(16) NOT NULL CHECK (drafted_via IN ('person', 'agent')),
 drafted_by bigint NOT NULL,
 drafted_at timestamptz NOT NULL DEFAULT now(),
 approved_by bigint,
 approved_at timestamptz,
 CHECK ((approved_by IS NULL) = (approved_at IS NULL)),
 UNIQUE (tenant_id, watcher_id, number),
 UNIQUE (tenant_id, id),
 FOREIGN KEY (tenant_id, watcher_id) REFERENCES base_watchers (tenant_id, id)
);
ALTER TABLE base_watchers ADD FOREIGN KEY (tenant_id, active_version_id) REFERENCES base_watcher_versions (tenant_id, id);

-- One run per approved version and event: why it matched and what it did.
CREATE TABLE base_watcher_runs (
 id bigserial PRIMARY KEY,
 tenant_id bigint NOT NULL,
 watcher_id bigint NOT NULL,
 version_id bigint NOT NULL,
 event_id bigint NOT NULL,
 event_name varchar(200) NOT NULL,
 subject varchar(200) NOT NULL,
 -- The run whose action recorded the event, and how deep that chain is.
 cause_run_id bigint,
 depth integer NOT NULL CHECK (depth > 0),
 state varchar(16) NOT NULL CHECK (state IN ('queued', 'matched', 'no_match', 'done', 'partial', 'refused')),
 reason varchar(64),
 -- The permitted facts read and each condition's outcome.
 facts jsonb,
 outcomes jsonb,
 created_at timestamptz NOT NULL DEFAULT now(),
 started_at timestamptz,
 finished_at timestamptz,
 UNIQUE (tenant_id, version_id, event_id),
 UNIQUE (tenant_id, id),
 FOREIGN KEY (tenant_id, watcher_id) REFERENCES base_watchers (tenant_id, id),
 FOREIGN KEY (tenant_id, version_id) REFERENCES base_watcher_versions (tenant_id, id)
);
CREATE INDEX base_watcher_runs_watcher ON base_watcher_runs (tenant_id, watcher_id, id DESC);
CREATE INDEX base_watcher_runs_event ON base_watcher_runs (tenant_id, event_id) WHERE state IN ('queued', 'matched');
CREATE INDEX base_watcher_runs_started ON base_watcher_runs (tenant_id, started_at) WHERE started_at IS NOT NULL;

-- One row per action of a run: the duplicate key that makes each effect happen once.
CREATE TABLE base_watcher_effects (
 id bigserial PRIMARY KEY,
 tenant_id bigint NOT NULL,
 run_id bigint NOT NULL,
 version_id bigint NOT NULL,
 event_id bigint NOT NULL,
 position integer NOT NULL CHECK (position >= 0),
 operation varchar(200) NOT NULL,
 state varchar(16) NOT NULL CHECK (state IN ('done', 'refused')),
 reason varchar(64),
 result jsonb,
 created_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (tenant_id, version_id, event_id, position),
 FOREIGN KEY (tenant_id, run_id) REFERENCES base_watcher_runs (tenant_id, id)
);

ALTER TABLE base_watcher_versions ADD COLUMN catalog_digest varchar(64);
ALTER TABLE base_watcher_runs ADD COLUMN subject_version bigint;
ALTER TABLE base_watchers ADD COLUMN needs_review boolean NOT NULL DEFAULT false;
ALTER TABLE base_watcher_runs ADD COLUMN logical_event text;
