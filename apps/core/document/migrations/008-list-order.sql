-- The Documents list's default order (newest revision first). A page walks
-- documents in this order and checks the reader's grant on each, instead of
-- joining every grant the reader holds and sorting them all.
CREATE INDEX core_documents_last_revision_idx ON core_documents(tenant_id, last_revision_at DESC, id DESC) WHERE deleting_at IS NULL;
