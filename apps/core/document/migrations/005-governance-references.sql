-- Tenant-scoped integration identity and document/revision lineage.
ALTER TABLE core_documents ADD COLUMN sync_id text CHECK (sync_id IS NULL OR length(sync_id) BETWEEN 1 AND 255);
ALTER TABLE core_documents ADD COLUMN related_document_id bigint;
ALTER TABLE core_documents ADD COLUMN relation text CHECK (relation IS NULL OR relation IN ('supersedes','replaces'));
ALTER TABLE core_documents ADD CONSTRAINT core_documents_relation_pair CHECK ((related_document_id IS NULL) = (relation IS NULL));
ALTER TABLE core_documents ADD CONSTRAINT core_documents_relation_not_self CHECK (related_document_id IS NULL OR related_document_id <> id);
ALTER TABLE core_documents ADD CONSTRAINT core_documents_related_document_fk FOREIGN KEY (tenant_id, related_document_id) REFERENCES core_documents(tenant_id, id);
CREATE UNIQUE INDEX core_documents_sync_id_unique ON core_documents(tenant_id, sync_id) WHERE sync_id IS NOT NULL;
CREATE INDEX core_documents_related_document_idx ON core_documents(tenant_id, related_document_id) WHERE related_document_id IS NOT NULL;

ALTER TABLE core_document_revisions ADD COLUMN label text CHECK (label IS NULL OR length(label) BETWEEN 1 AND 100);
ALTER TABLE core_document_revisions ADD COLUMN replaces_revision_id bigint;
ALTER TABLE core_document_revisions ADD CONSTRAINT core_document_revisions_replaces_not_self CHECK (replaces_revision_id IS NULL OR replaces_revision_id <> id);
ALTER TABLE core_document_revisions ADD CONSTRAINT core_document_revisions_replaces_fk FOREIGN KEY (tenant_id, replaces_revision_id) REFERENCES core_document_revisions(tenant_id, id);
