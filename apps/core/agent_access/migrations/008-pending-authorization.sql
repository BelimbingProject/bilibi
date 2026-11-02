-- Browser consent creates a short-lived authorization receipt. The durable
-- connection is created only when the client redeems that receipt, so an
-- abandoned or expired callback never appears as an AI connection.
DELETE FROM core_agent_authorization_codes;
ALTER TABLE core_agent_authorization_codes
 ALTER COLUMN connection_id DROP NOT NULL,
 ADD COLUMN tenant_id bigint NOT NULL REFERENCES tenants(id),
 ADD COLUMN user_id bigint NOT NULL,
 ADD COLUMN organisation_id bigint,
 ADD COLUMN person_id bigint,
 ADD COLUMN agent_id varchar(255) NOT NULL CHECK (length(agent_id) > 0),
 ADD COLUMN client_id text NOT NULL,
 ADD COLUMN resource text NOT NULL,
 ADD COLUMN oauth_scope varchar(255) NOT NULL,
 ADD COLUMN approved_client varchar(512) NOT NULL,
 ADD COLUMN replace_connection_ids bigint[] NOT NULL DEFAULT '{}',
 ADD CONSTRAINT core_agent_authorization_codes_user_fkey
  FOREIGN KEY (tenant_id,user_id) REFERENCES users(tenant_id,id) ON DELETE CASCADE,
 ADD CONSTRAINT core_agent_authorization_codes_organisation_fkey
  FOREIGN KEY (tenant_id,organisation_id) REFERENCES companies(tenant_id,id) ON DELETE RESTRICT;
CREATE INDEX core_agent_authorization_codes_request_idx
 ON core_agent_authorization_codes(tenant_id,user_id,client_id,redirect_uri,resource,code_challenge)
 WHERE connection_id IS NULL AND used_at IS NULL;
