-- One calendar feed key per person (`core/activity`): the unguessable link a
-- calendar app subscribes to. Only the SHA-256 of the key is stored; the key
-- itself is shown once, when it is made. Replacing a key overwrites the row,
-- revoking deletes it, so a stale link finds nothing at once.
CREATE TABLE core_activity_feed_keys (
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 user_id bigint NOT NULL,
 key_hash bytea NOT NULL UNIQUE CHECK (octet_length(key_hash) = 32),
 created_at timestamptz NOT NULL DEFAULT now(),
 last_used_at timestamptz,
 PRIMARY KEY (tenant_id, user_id),
 FOREIGN KEY (tenant_id, user_id) REFERENCES users(tenant_id, id) ON DELETE CASCADE
);
