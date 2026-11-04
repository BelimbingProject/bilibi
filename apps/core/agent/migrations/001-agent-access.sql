-- E3: OAuth device codes, per-person agent connections, opaque access/refresh
-- tokens and PKCE authorization codes. Every secret column holds only the
-- SHA-256 digest of the value; the raw value exists in one response alone.

-- A grant made by one account in one tenant, never a second identity.
CREATE TABLE core_agent_connections (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 user_id bigint NOT NULL,
 -- K0 party facets of the granting actor, recorded when the sealed scope
 -- carries them (K9 binds them) and reproved through base/tenancy.parties on
 -- every issuance. Person storage lands with K7, so person_id has no FK yet.
 organisation_id bigint,
 person_id bigint,
 agent_id varchar(255) NOT NULL CHECK (length(agent_id) > 0),
 client_id varchar(255) NOT NULL DEFAULT 'bilibi-native',
 resource text,
 oauth_scope varchar(255),
 seen_agent_ids jsonb NOT NULL DEFAULT '{}'::jsonb,
 expires_at timestamptz NOT NULL,
 revoked_at timestamptz,
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (tenant_id, id),
 FOREIGN KEY (tenant_id, user_id) REFERENCES users(tenant_id, id) ON DELETE CASCADE,
 FOREIGN KEY (tenant_id, organisation_id) REFERENCES companies(tenant_id, id) ON DELETE RESTRICT
);
CREATE INDEX core_agent_connections_tenant_user_idx ON core_agent_connections(tenant_id, user_id);

-- Device authorization receipts exist before any tenant is known. They keep
-- their spent state after the connection is gone.
CREATE TABLE core_agent_devices (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 device_hash bytea NOT NULL UNIQUE CHECK (octet_length(device_hash) = 32),
 user_hash bytea NOT NULL UNIQUE CHECK (octet_length(user_hash) = 32),
 client_id varchar(255) NOT NULL,
 agent_id varchar(255) NOT NULL,
 expires_at timestamptz NOT NULL,
 next_poll_at timestamptz NOT NULL,
 interval integer NOT NULL CHECK (interval >= 5),
 status text NOT NULL CHECK (status IN ('pending', 'approved', 'denied', 'used')),
 connection_id bigint REFERENCES core_agent_connections(id) ON DELETE SET NULL,
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now()
);

-- Spent refresh digests stay, so a replay is recognised and ends the grant.
CREATE TABLE core_agent_tokens (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 connection_id bigint NOT NULL REFERENCES core_agent_connections(id) ON DELETE CASCADE,
 token_hash bytea NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
 kind text NOT NULL CHECK (kind IN ('access', 'refresh')),
 expires_at timestamptz NOT NULL,
 used_at timestamptz,
 created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX core_agent_tokens_connection_idx ON core_agent_tokens(connection_id);

CREATE TABLE core_agent_authorization_codes (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 connection_id bigint NOT NULL REFERENCES core_agent_connections(id) ON DELETE CASCADE,
 code_hash bytea NOT NULL UNIQUE CHECK (octet_length(code_hash) = 32),
 code_challenge varchar(255) NOT NULL,
 redirect_uri text NOT NULL,
 expires_at timestamptz NOT NULL,
 used_at timestamptz,
 created_at timestamptz NOT NULL DEFAULT now()
);
