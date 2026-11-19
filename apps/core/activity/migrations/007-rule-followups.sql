ALTER TABLE core_activities DROP COLUMN followup_key;
ALTER TABLE core_activities ADD COLUMN followup_rule_key varchar(64);
CREATE UNIQUE INDEX core_activity_rule_followup_once ON core_activities(tenant_id, followup_rule_key)
WHERE followup_rule_key IS NOT NULL;
