-- One row per call a module made: metadata only. No instruction, text or
-- reply has a column here, ever; the calling module keeps any text in its
-- own erasable store. A row is a machine measurement, like an Integration
-- exchange: no foreign keys, so evidence outlives accounts, and the daily
-- retention job deletes old rows.
CREATE TABLE base_ai_interactions (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 public_id text NOT NULL UNIQUE CHECK (length(public_id) BETWEEN 1 AND 40),
 tenant_id bigint NOT NULL,
 user_id bigint,
 principal text CHECK (length(principal) BETWEEN 1 AND 255),
 connection_id bigint,
 impersonator_id bigint,
 company_id bigint,
 module text NOT NULL CHECK (length(module) BETWEEN 1 AND 255),
 provider text CHECK (length(provider) BETWEEN 1 AND 64),
 model text CHECK (length(model) BETWEEN 1 AND 255),
 input_characters bigint NOT NULL CHECK (input_characters >= 0),
 output_characters bigint CHECK (output_characters >= 0),
 outcome text NOT NULL CHECK (outcome IN ('generated', 'unavailable')),
 reason text CHECK (reason IN ('disabled', 'not_configured', 'timeout', 'provider_error')),
 duration_ms bigint NOT NULL CHECK (duration_ms >= 0),
 exchange_id text CHECK (length(exchange_id) BETWEEN 1 AND 40),
 created_at timestamptz NOT NULL DEFAULT now(),
 CHECK ((user_id IS NULL) <> (principal IS NULL)),
 CHECK ((outcome = 'generated') = (reason IS NULL)),
 CHECK ((outcome = 'generated') = (output_characters IS NOT NULL)),
 CHECK (outcome = 'unavailable' OR (provider IS NOT NULL AND model IS NOT NULL))
);
CREATE INDEX base_ai_interactions_timeline_idx ON base_ai_interactions (tenant_id, created_at DESC, id DESC);
CREATE INDEX base_ai_interactions_created_idx ON base_ai_interactions (created_at);
