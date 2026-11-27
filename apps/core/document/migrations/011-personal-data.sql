-- The personal fields of documents, revisions and file ledger rows
-- (base_personal_data_kinds, ADR 05): what an erasure clears from their audit
-- evidence. Documents related to a person are erased through `privacy.rs`.
INSERT INTO base_personal_data_kinds(kind, module, fields, subject_identifier, person) VALUES
    ('core.document.document', 'core/document', ARRAY['name', 'external_url', 'custom_fields'], false, false),
    ('core.document.revision', 'core/document', ARRAY['name', 'file_name', 'external_url', 'change_log', 'label'], false, false),
    ('core.document.file', 'core/document', ARRAY['file_name'], false, false);
