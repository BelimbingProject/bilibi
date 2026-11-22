-- An interrupted trial has a start receipt and no completion or count.
ALTER TABLE base_watcher_trials ALTER COLUMN examined DROP NOT NULL;
ALTER TABLE base_watcher_trials ADD COLUMN completed_at timestamptz;
