-- An approval grants the watchers principal permission to run each exact
-- script declaration in that version. Runtime checks this row as well as the
-- principal's live script-execution capability.
CREATE TABLE base_watcher_script_permissions (
 tenant_id bigint NOT NULL,
 watcher_id bigint NOT NULL,
 version_id bigint NOT NULL,
 script_digest varchar(64) NOT NULL,
 principal varchar(120) NOT NULL CHECK (btrim(principal) <> ''),
 granted_by bigint NOT NULL,
 granted_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY (tenant_id, version_id, script_digest, principal),
 FOREIGN KEY (tenant_id, watcher_id) REFERENCES base_watchers (tenant_id, id),
 FOREIGN KEY (tenant_id, version_id) REFERENCES base_watcher_versions (tenant_id, id)
);
