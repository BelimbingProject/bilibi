-- The last time the installer that claimed a running job said it was alive.
-- It writes this every half minute while it works on the job, so a job whose
-- installer is gone for good (the service was removed or renamed, the machine
-- is lost) can be ended instead of blocking every later install: the app
-- ends a running job that never began its pause once the installer has been
-- silent for `software.installer_silence_minutes`.
ALTER TABLE base_software_jobs ADD COLUMN heartbeat_at timestamptz;
