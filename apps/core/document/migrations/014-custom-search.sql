-- Global search reaches the tenant's custom text fields (issue #685): a
-- stored generated vector of every custom value, GIN-indexed. Base Search
-- keeps a candidate only when one of the tenant's active text fields holds
-- the words, so archived fields, codes and ids never match.
ALTER TABLE core_documents ADD COLUMN custom_search_vector tsvector
 GENERATED ALWAYS AS (jsonb_to_tsvector('simple', custom_fields, '["string"]')) STORED;
CREATE INDEX core_documents_custom_search ON core_documents USING gin (custom_search_vector);
