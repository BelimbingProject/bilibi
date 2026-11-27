-- The daily payload retention reads only the rows that still hold a saved
-- preview, in id order. Its predicate is this index's, so the job no longer
-- walks the whole log to find them.
CREATE INDEX base_integration_exchanges_payload_idx ON base_integration_exchanges (id)
 WHERE request_headers IS NOT NULL OR request_body IS NOT NULL OR response_headers IS NOT NULL OR response_body IS NOT NULL;
