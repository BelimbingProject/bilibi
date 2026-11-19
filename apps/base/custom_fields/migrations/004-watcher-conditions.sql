-- Watcher conditions (issue #730). A tenant administrator opts a field into Base
-- Rules conditions deliberately; until then a rule never reads it. Only the
-- kinds a condition compares as one typed value may opt in.
ALTER TABLE base_custom_fields ADD COLUMN watcher_condition boolean NOT NULL DEFAULT false;
ALTER TABLE base_custom_fields ADD CONSTRAINT base_custom_fields_watcher_condition CHECK (NOT watcher_condition OR kind IN ('text', 'number', 'date', 'choice', 'boolean'));
