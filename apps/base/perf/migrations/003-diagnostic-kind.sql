DELETE FROM base_perf_diagnostics;
ALTER TABLE base_perf_diagnostics
    DROP CONSTRAINT base_perf_diagnostics_pkey,
    ADD COLUMN kind text NOT NULL,
    ADD PRIMARY KEY (severity, source, kind);
