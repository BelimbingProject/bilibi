-- Owner-projected business outcomes, beside the owner mutation.
ALTER TABLE base_bulk_data_rows DROP CONSTRAINT base_bulk_data_rows_outcome_check;
ALTER TABLE base_bulk_data_rows ADD CONSTRAINT base_bulk_data_rows_outcome_check
    CHECK (outcome IN ('create', 'update', 'skip', 'action', 'error'));
ALTER TABLE base_bulk_data_rows ADD COLUMN links jsonb NOT NULL DEFAULT '[]'
    CHECK (jsonb_typeof(links) = 'array' AND jsonb_array_length(links) <= 4);
