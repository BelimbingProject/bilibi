-- Field labels (#686, ADR 28): a tenant may also rename a field of a record
-- type. `subject` is `<table>.<field>`, the stable Grid table and field IDs.
ALTER TABLE base_labels DROP CONSTRAINT base_labels_kind_check;
ALTER TABLE base_labels ADD CONSTRAINT base_labels_kind_check CHECK (kind IN ('menu', 'record', 'field'));
-- A field subject joins two IDs of up to 64 characters.
ALTER TABLE base_labels DROP CONSTRAINT base_labels_subject_check;
ALTER TABLE base_labels ADD CONSTRAINT base_labels_subject_check CHECK (subject ~ '^[a-z0-9][a-z0-9._-]{0,199}$');
