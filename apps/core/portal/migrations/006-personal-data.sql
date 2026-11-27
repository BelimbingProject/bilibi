-- Customer account requests hold a name, a confirmed email address and an
-- optional company name. Declaring them lets an approved personal-data
-- erasure clear those fields from the append-only audit trail too.
INSERT INTO base_personal_data_kinds(kind, module, fields, subject_identifier, person)
VALUES ('portal.registration', 'core/portal', ARRAY['name', 'email', 'company_name'], false, false);
