-- A short-lived, two-person review before the installation owner receipt moves.
CREATE TABLE user_ownership_transfers (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 from_user_id bigint NOT NULL,
 to_user_id bigint NOT NULL,
 status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','accepted','expired')),
 created_at timestamptz NOT NULL DEFAULT now(),
 expires_at timestamptz NOT NULL,
 resolved_at timestamptz,
 CHECK (from_user_id <> to_user_id),
 CHECK ((status = 'pending') = (resolved_at IS NULL)),
 FOREIGN KEY (tenant_id,from_user_id) REFERENCES users(tenant_id,id),
 FOREIGN KEY (tenant_id,to_user_id) REFERENCES users(tenant_id,id)
);

CREATE UNIQUE INDEX user_ownership_transfers_one_pending
 ON user_ownership_transfers ((true)) WHERE status = 'pending';
CREATE INDEX user_ownership_transfers_recipient
 ON user_ownership_transfers (tenant_id,to_user_id,id DESC);
