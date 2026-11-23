-- At-most-once customer-hosted scorer delivery receipts. The body and signing
-- secret never enter this table; Integration's ordinary exchange row owns the
-- HTTP measurement and this row owns deduplication and the typed decision.
CREATE TABLE base_integration_scorer_deliveries (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL,
 scorer varchar(200) NOT NULL CHECK (btrim(scorer) <> ''),
 event_digest bytea NOT NULL CHECK (octet_length(event_digest) = 32),
 delivery_id varchar(40) NOT NULL CHECK (btrim(delivery_id) <> ''),
 state varchar(16) NOT NULL CHECK (state IN ('sending','done','failed','refused','revoked')),
 exchange_id text,
 response_status smallint CHECK (response_status BETWEEN 100 AND 599),
 result varchar(120),
 reason varchar(64),
 created_at timestamptz NOT NULL DEFAULT now(),
 finished_at timestamptz,
 UNIQUE (tenant_id, scorer, event_digest),
 UNIQUE (tenant_id, delivery_id),
 CHECK ((state = 'sending') = (finished_at IS NULL))
);
CREATE INDEX base_integration_scorer_deliveries_timeline
 ON base_integration_scorer_deliveries (tenant_id, created_at DESC, id DESC);
