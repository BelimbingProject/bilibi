CREATE TABLE user_two_factor_reset_requests (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 user_id bigint NOT NULL,
 identity_id bigint NOT NULL REFERENCES user_identities(id) ON DELETE CASCADE,
 company_name text NOT NULL,
 recipient_kind text NOT NULL CHECK (recipient_kind IN ('tenant_administrators','installation_owner')),
 requested_at timestamptz NOT NULL DEFAULT now(),
 closed_at timestamptz,
 closed_by_user_id bigint,
 FOREIGN KEY (tenant_id,user_id) REFERENCES users(tenant_id,id) ON DELETE CASCADE
);

CREATE INDEX user_two_factor_reset_requests_open
 ON user_two_factor_reset_requests(tenant_id,user_id,requested_at DESC)
 WHERE closed_at IS NULL;

CREATE INDEX user_two_factor_reset_requests_identity_open
 ON user_two_factor_reset_requests(identity_id,requested_at DESC)
 WHERE closed_at IS NULL;
