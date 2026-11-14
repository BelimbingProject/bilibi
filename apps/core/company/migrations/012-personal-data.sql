-- The company kind's personal fields (base_personal_data_kinds, ADR 05, #595):
-- the identity and contact fields a sole proprietorship's person owns, with
-- tenant-defined custom values audited under one nested `custom_fields` key.
-- Never `*`: a company's changes also carry amounts (revenue), its status and
-- Extension fields, whose history an erasure must keep.
INSERT INTO base_personal_data_kinds(kind, module, fields, subject_identifier, person)
    VALUES ('company', 'core/company', ARRAY['email', 'phone', 'tax_id', 'legal_name', 'registration_number', 'website', 'metadata', 'custom_fields', 'name', 'code'], false, false);
