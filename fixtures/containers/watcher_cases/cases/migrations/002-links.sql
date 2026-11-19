ALTER TABLE watcher_cases_cases ADD COLUMN parent_id bigint, ADD COLUMN owner_user_id bigint;
ALTER TABLE watcher_cases_cases ADD UNIQUE (tenant_id, id);
ALTER TABLE watcher_cases_cases ADD FOREIGN KEY (tenant_id, parent_id) REFERENCES watcher_cases_cases (tenant_id, id);
