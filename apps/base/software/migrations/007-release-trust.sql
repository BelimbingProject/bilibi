-- Release trust changes are owner-reviewed jobs carried out by the separate
-- installer, which alone may replace the server-wide trust record.
ALTER TABLE base_software_jobs DROP CONSTRAINT base_software_jobs_kind_check;
ALTER TABLE base_software_jobs ADD CONSTRAINT base_software_jobs_kind_check
CHECK (kind IN ('install','undo','trust'));
