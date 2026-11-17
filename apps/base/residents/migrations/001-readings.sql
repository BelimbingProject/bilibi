-- One row per (tenant, reader, key) a module asked the Platform to keep
-- reading. Residents owns all lease state: `running` means one server process
-- holds the lease (`lease_token`, renewed by its heartbeat); `waiting` means
-- none does and any process may claim it from `available_at`. Releasing a key
-- deletes its row. No Core FK and no actor: a reading runs as the reader's
-- declared system principal, proven again before every start.
CREATE TABLE base_resident_readings (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id bigint NOT NULL CHECK (tenant_id > 0),
    reader_id varchar(128) NOT NULL,
    reader_module text NOT NULL,
    key varchar(128) NOT NULL CHECK (key ~ '^[a-z0-9][a-z0-9._:/-]{0,127}$'),
    state text NOT NULL DEFAULT 'waiting' CHECK (state IN ('waiting','running')),
    available_at timestamptz NOT NULL DEFAULT now(),
    -- The process that holds, or last held, the lease: host name and process ID.
    owner varchar(200),
    lease_token text,
    lease_expires_at timestamptz,
    started_at timestamptz,
    heartbeat_at timestamptz,
    starts bigint NOT NULL DEFAULT 0 CHECK (starts >= 0),
    failures integer NOT NULL DEFAULT 0 CHECK (failures >= 0),
    last_error varchar(64) CHECK (last_error IS NULL OR last_error ~ '^[a-z][a-z0-9_]{0,63}$'),
    last_error_at timestamptz,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, reader_id, key),
    CHECK ((state = 'running') = (lease_token IS NOT NULL AND lease_expires_at IS NOT NULL)),
    CHECK (state <> 'running' OR owner IS NOT NULL),
    CHECK ((last_error IS NULL) = (last_error_at IS NULL))
);
-- A poll reads only what it can act on: the due readings and the expired leases.
CREATE INDEX base_resident_readings_due ON base_resident_readings (available_at, id) WHERE state = 'waiting';
CREATE INDEX base_resident_readings_leases ON base_resident_readings (lease_expires_at, id) WHERE state = 'running';
