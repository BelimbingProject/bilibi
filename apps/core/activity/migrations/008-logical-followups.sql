ALTER TABLE core_activities DROP COLUMN followup_watcher_key;
ALTER TABLE core_activities ADD COLUMN followup_watcher_key varchar(64);
CREATE UNIQUE INDEX core_activity_watcher_followup_once ON core_activities(tenant_id, followup_watcher_key)
WHERE followup_watcher_key IS NOT NULL;
