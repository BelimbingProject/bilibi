-- A document's expiry date, the reminder task Core Activity keeps for it
-- (a plain id: no foreign key crosses a module), the tenant's custom field
-- values, and an optional end for an explicit access grant.
ALTER TABLE core_documents
 ADD COLUMN expires_on date,
 ADD COLUMN expiry_activity_id bigint,
 ADD COLUMN custom_fields jsonb NOT NULL DEFAULT '{}'::jsonb,
 ADD CONSTRAINT core_documents_custom_fields_object CHECK (jsonb_typeof(custom_fields) = 'object');
CREATE INDEX core_documents_expires_idx ON core_documents(tenant_id, expires_on) WHERE expires_on IS NOT NULL;
CREATE INDEX core_documents_custom_fields_idx ON core_documents USING gin (custom_fields jsonb_path_ops);

ALTER TABLE core_document_access ADD COLUMN expires_at timestamptz;
