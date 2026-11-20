ALTER TABLE base_rule_versions ADD COLUMN catalog_digest varchar(64);
ALTER TABLE base_rule_runs ADD COLUMN subject_version bigint;
ALTER TABLE base_rules ADD COLUMN needs_review boolean NOT NULL DEFAULT false;
