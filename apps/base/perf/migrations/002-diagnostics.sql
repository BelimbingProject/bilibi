-- The source and message are selected from compiled-in, non-sensitive facts.
CREATE TABLE base_perf_diagnostics (
    severity text NOT NULL,
    source text NOT NULL,
    message text NOT NULL,
    count bigint NOT NULL,
    first_seen timestamptz NOT NULL,
    last_seen timestamptz NOT NULL,
    PRIMARY KEY (severity, source)
);
