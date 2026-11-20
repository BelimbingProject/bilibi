ALTER TABLE watcher_cases_cases
  ADD COLUMN custom_fields jsonb NOT NULL DEFAULT '{}'::jsonb,
  ADD CONSTRAINT watcher_cases_cases_custom_fields_object CHECK (jsonb_typeof(custom_fields) = 'object');
