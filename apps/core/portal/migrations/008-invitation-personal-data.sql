-- Register copied invitation values for the guarded audit-erasure path.
INSERT INTO base_personal_data_kinds(kind, module, fields, subject_identifier, person)
VALUES ('portal.invitation', 'core/portal', ARRAY['name', 'email'], false, false);

CREATE INDEX customer_invitations_person_idx ON customer_invitations(tenant_id, person_id, id);
