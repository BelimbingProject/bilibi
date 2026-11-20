ALTER TABLE base_watchers ADD COLUMN bucket_tokens double precision;
ALTER TABLE base_watchers ADD COLUMN bucket_at timestamptz;
ALTER TABLE base_watchers ADD CONSTRAINT base_watchers_bucket_pair CHECK ((bucket_tokens IS NULL) = (bucket_at IS NULL));
DROP INDEX base_watcher_runs_started;
