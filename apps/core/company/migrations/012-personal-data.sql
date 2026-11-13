-- The company kind's personal fields (base_personal_data_kinds, ADR 05, #595).
-- `*`: a company's audit values have tenant-defined keys (`cf_<key>` custom
-- fields) beside its own, so audit erasure of a company confirmed as a
-- person's clears every value and keeps which fields changed and when.
INSERT INTO base_personal_data_kinds(kind, module, fields, subject_identifier, person)
    VALUES ('company', 'core/company', ARRAY['*'], false, false);
