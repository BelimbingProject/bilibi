ALTER TABLE base_custom_fields DROP CONSTRAINT base_custom_fields_kind_check;
ALTER TABLE base_custom_fields ADD CONSTRAINT base_custom_fields_kind_check CHECK (kind IN ('text', 'number', 'date', 'choice', 'boolean', 'long_text', 'link', 'phone', 'email'));
