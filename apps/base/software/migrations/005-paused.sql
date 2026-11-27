-- When the job began its pause (the service was stopped for it). The
-- installer writes it in the same transaction that starts the pausing step,
-- so a restart reads whether the pause began instead of guessing it from a
-- step's state.
ALTER TABLE base_software_jobs ADD COLUMN paused_at timestamptz;
