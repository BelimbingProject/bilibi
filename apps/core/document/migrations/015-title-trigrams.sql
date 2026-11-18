CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE INDEX core_documents_name_trgm
    ON core_documents USING gin (lower(name) gin_trgm_ops)
    WHERE deleting_at IS NULL;
