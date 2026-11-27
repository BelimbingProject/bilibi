-- The binding stays with the existing private task, including after soft deletion.
ALTER TABLE core_activities ADD COLUMN followup_key varchar(64);
CREATE UNIQUE INDEX core_activity_followup_once ON core_activities(tenant_id, followup_key)
WHERE followup_key IS NOT NULL;
