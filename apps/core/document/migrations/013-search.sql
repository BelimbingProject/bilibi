-- Only protected fields enter the shared search vector. Explicit grants
-- remain in the Grid source; matching a name never grants discovery.
ALTER TABLE core_documents ADD COLUMN search_vector tsvector GENERATED ALWAYS AS (
    setweight(to_tsvector('simple', name), 'A') ||
    setweight(to_tsvector('simple', coalesce(sync_id, '')), 'B')
) STORED;
CREATE INDEX core_documents_search_idx ON core_documents USING gin (search_vector);
