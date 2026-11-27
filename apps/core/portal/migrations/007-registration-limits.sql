-- Public customer registration is bounded across every application node.
-- Subjects are one-way fingerprints, kept only as short-lived security state;
-- the raw sender address and recipient never enter this table.
CREATE TABLE customer_registration_limits (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 dimension text NOT NULL CHECK (dimension IN ('sender', 'recipient', 'portal')),
 subject_hash bytea NOT NULL CHECK (octet_length(subject_hash) = 32),
 minute_started_at timestamptz NOT NULL DEFAULT now(),
 minute_count integer NOT NULL DEFAULT 0 CHECK (minute_count >= 0),
 day_started_at timestamptz NOT NULL DEFAULT now(),
 day_count integer NOT NULL DEFAULT 0 CHECK (day_count >= 0),
 alerted_at timestamptz,
 updated_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (tenant_id, dimension, subject_hash)
);
CREATE INDEX customer_registration_limits_stale_idx
 ON customer_registration_limits(tenant_id, updated_at);
