-- Structured custom values (issue #685). Two more types: several choices
-- from one standalone pick-list, kept on the record as a JSON array of codes,
-- and a reference to one record of a Base Records kind (`target`), kept as
-- that record's id text. A field's target, like its type and its pick-list,
-- never changes. Values stay in each owner's `custom_fields` JSON column.
ALTER TABLE base_custom_fields ADD COLUMN target text;
ALTER TABLE base_custom_fields DROP CONSTRAINT base_custom_fields_kind_check;
ALTER TABLE base_custom_fields ADD CONSTRAINT base_custom_fields_kind_check CHECK (kind IN ('text', 'number', 'date', 'choice', 'boolean', 'long_text', 'link', 'phone', 'email', 'choices', 'reference'));
ALTER TABLE base_custom_fields DROP CONSTRAINT base_custom_fields_picklist;
ALTER TABLE base_custom_fields ADD CONSTRAINT base_custom_fields_picklist CHECK ((kind IN ('choice', 'choices')) = (picklist IS NOT NULL));
ALTER TABLE base_custom_fields ADD CONSTRAINT base_custom_fields_target CHECK ((kind = 'reference') = (target IS NOT NULL));
