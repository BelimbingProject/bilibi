-- A person's tenant-defined custom field values (`base/custom_fields`,
-- ADR 21), keyed by each field's stable key. Erasure clears them.
ALTER TABLE persons
 ADD COLUMN custom_fields jsonb NOT NULL DEFAULT '{}'::jsonb,
 ADD CONSTRAINT persons_custom_fields_object CHECK (jsonb_typeof(custom_fields) = 'object');
CREATE INDEX persons_custom_fields_idx ON persons USING gin (custom_fields jsonb_path_ops);

-- Custom values are personal data: audit erasure clears them in a person's
-- history as it clears the contacts.
UPDATE base_personal_data_kinds SET fields = array_append(fields, 'custom') WHERE kind = 'person';
