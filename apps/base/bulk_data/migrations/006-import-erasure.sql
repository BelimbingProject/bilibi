-- An owner's erasure also reaches import runs (issue 571): the uploaded file
-- is the person's data as typed, and a row's source key and message can
-- quote a cell. `erased_for` is the ID-only cleanup authority, as on
-- exports; `cleaned_at` says an erasure has since removed everything the
-- run or export held, so a later erasure leaves it alone (issue 600).
ALTER TABLE base_bulk_data_runs
    ADD COLUMN erased_for bigint[] NOT NULL DEFAULT '{}',
    ADD COLUMN cleaned_at timestamptz;
ALTER TABLE base_bulk_data_exports ADD COLUMN cleaned_at timestamptz;
