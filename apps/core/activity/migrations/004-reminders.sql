-- Per-activity reminders (`core/activity`). `reminder_minutes` are the offsets
-- before the start or due time to remind at: NULL follows the creator's
-- default and then the tenant setting, an empty list is no reminder.
-- `reminder_email` says whether each reminder is also emailed (to the
-- assignee, and for a call or meeting to guests without an account): NULL
-- follows the tenant setting. `reminders_sent` holds the offsets the sweep
-- has already reminded at (-1 for the tenant-default reminder); it is cleared
-- with `reminded_at` when the time or the assignee changes.
ALTER TABLE core_activities
 ADD COLUMN reminder_minutes integer[] CHECK (reminder_minutes IS NULL OR (cardinality(reminder_minutes) <= 5 AND 0 <= ALL(reminder_minutes) AND 10080 >= ALL(reminder_minutes))),
 ADD COLUMN reminder_email boolean,
 ADD COLUMN reminders_sent integer[] NOT NULL DEFAULT '{}';

CREATE UNIQUE INDEX core_activity_guests_tenant_id_idx ON core_activity_guests(tenant_id, id);

-- One row per guest reminder email sent, so a retried job never emails a
-- guest twice. `at` is the start or due time the reminder was for, so a moved
-- activity reminds its guests again.
CREATE TABLE core_activity_guest_reminders (
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 guest_id bigint NOT NULL,
 activity_id bigint NOT NULL,
 at timestamptz NOT NULL,
 minutes integer NOT NULL,
 sent_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY (tenant_id, guest_id, at, minutes),
 FOREIGN KEY (tenant_id, guest_id) REFERENCES core_activity_guests(tenant_id, id) ON DELETE CASCADE,
 FOREIGN KEY (tenant_id, activity_id) REFERENCES core_activities(tenant_id, id)
);
CREATE INDEX core_activity_guest_reminders_activity_idx ON core_activity_guest_reminders(tenant_id, activity_id);
