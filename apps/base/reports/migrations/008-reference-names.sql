ALTER TABLE base_report_exports
 DROP CONSTRAINT base_report_exports_access_check,
 ADD COLUMN reference_names jsonb NOT NULL DEFAULT '[]'::jsonb;
