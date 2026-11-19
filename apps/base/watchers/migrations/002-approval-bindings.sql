ALTER TABLE base_watcher_versions ADD COLUMN catalog_digest varchar(64);
ALTER TABLE base_watcher_runs ADD COLUMN subject_version bigint;
ALTER TABLE base_watchers ADD COLUMN needs_review boolean NOT NULL DEFAULT false;
