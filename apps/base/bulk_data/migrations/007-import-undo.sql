-- A completed import can be undone for a bounded period. Only rows whose
-- owner reported a newly created record carry the version needed for the
-- owner's removal operation. Undo progress and refusals stay with the run.
ALTER TABLE base_bulk_data_runs DROP CONSTRAINT base_bulk_data_runs_state_check;
ALTER TABLE base_bulk_data_runs ADD CONSTRAINT base_bulk_data_runs_state_check
    CHECK (state IN ('draft', 'previewing', 'previewed', 'running', 'completed', 'cancelled', 'undoing', 'undone'));
ALTER TABLE base_bulk_data_runs DROP CONSTRAINT base_bulk_data_runs_check;
ALTER TABLE base_bulk_data_runs DROP CONSTRAINT base_bulk_data_runs_check1;
ALTER TABLE base_bulk_data_runs
    ADD COLUMN undo_next_row integer NOT NULL DEFAULT 0,
    ADD COLUMN undo_started_at timestamptz,
    ADD COLUMN undo_finished_at timestamptz;
ALTER TABLE base_bulk_data_runs ADD CONSTRAINT base_bulk_data_runs_next_row_check
    CHECK (next_row >= 0 AND next_row <= row_count);
ALTER TABLE base_bulk_data_runs ADD CONSTRAINT base_bulk_data_runs_undo_next_row_check
    CHECK (undo_next_row >= 0 AND undo_next_row <= row_count);
ALTER TABLE base_bulk_data_runs ADD CONSTRAINT base_bulk_data_runs_pass_state_check
    CHECK ((state IN ('previewing', 'previewed') AND pass = 'preview')
        OR (state IN ('running', 'completed', 'undoing', 'undone') AND pass = 'run')
        OR state IN ('draft', 'cancelled'));

ALTER TABLE base_bulk_data_rows
    ADD COLUMN record_version bigint CHECK (record_version IS NULL OR record_version >= 1),
    ADD COLUMN undo_outcome text CHECK (undo_outcome IN ('removed', 'edited', 'error')),
    ADD COLUMN undo_message text CHECK (undo_message IS NULL OR char_length(undo_message) <= 2000),
    ADD COLUMN undone_at timestamptz;
CREATE INDEX base_bulk_data_rows_undo_idx
    ON base_bulk_data_rows (tenant_id, run_id, row_number)
    WHERE pass = 'run' AND outcome = 'create';
