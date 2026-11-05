-- When the invitation worker emailed a guest the invitation Send invites
-- last stamped in `invited_at`, so a retried job never emails them twice.
-- Account guests are told in the bell and keep it empty.
ALTER TABLE core_activity_guests ADD COLUMN invite_sent_at timestamptz;
