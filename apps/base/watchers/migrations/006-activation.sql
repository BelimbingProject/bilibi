-- A run is bound to the current activation, not merely the approved version.
ALTER TABLE base_watchers ADD COLUMN enabled_at timestamptz;
ALTER TABLE base_watcher_runs ADD COLUMN activation_at timestamptz;
