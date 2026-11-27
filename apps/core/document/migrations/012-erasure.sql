-- A document a privacy erasure anonymised (#659): deleting, and finished by
-- the erasure's own after-erase step (its Media bytes first, while its
-- person link still says whose they are), never by the delete job.
ALTER TABLE core_documents
    ADD COLUMN erased_at timestamptz,
    ADD CONSTRAINT core_documents_erased_deleting CHECK (erased_at IS NULL OR deleting_at IS NOT NULL);
