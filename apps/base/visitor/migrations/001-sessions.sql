-- A public visitor's session: a bearer proof bound to one tenant, one site of
-- a Domain's channel and the browser origin it was issued to. It lives apart
-- from `sessions`, so no staff or customer check can ever read it. Only a
-- digest of the token is kept.
CREATE TABLE visitor_sessions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id bigint NOT NULL REFERENCES tenants(id),
    channel text NOT NULL CHECK (channel ~ '^[a-z0-9][a-z0-9._/-]{0,63}$'),
    site text NOT NULL CHECK (char_length(site) BETWEEN 1 AND 128),
    subject text NOT NULL CHECK (char_length(subject) BETWEEN 1 AND 128),
    origin text NOT NULL CHECK (char_length(origin) BETWEEN 1 AND 255),
    token_digest bytea NOT NULL UNIQUE CHECK (octet_length(token_digest) = 32),
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    absolute_expires_at timestamptz NOT NULL,
    renewed_at timestamptz,
    revoked_at timestamptz,
    revoked_reason text CHECK (revoked_reason IN ('visitor', 'staff')),
    CHECK ((revoked_at IS NULL) = (revoked_reason IS NULL)),
    CHECK (expires_at <= absolute_expires_at)
);
CREATE INDEX visitor_sessions_subject_idx ON visitor_sessions (tenant_id, channel, subject);
CREATE INDEX visitor_sessions_expiry_idx ON visitor_sessions (absolute_expires_at);
