-- A deployment operator may prepare the first owner of a customer tenant.
-- The ordinary membership invitation remains the credential door; a system
-- invitation has no tenant account that invited it.
ALTER TABLE user_invitations ALTER COLUMN invited_by DROP NOT NULL;

CREATE TABLE user_tenant_owner_bootstraps (
 tenant_id bigint PRIMARY KEY REFERENCES tenants(id),
 user_id bigint NOT NULL,
 company_id bigint NOT NULL,
 invitation_id bigint NOT NULL REFERENCES user_invitations(id),
 created_at timestamptz NOT NULL DEFAULT now(),
 FOREIGN KEY(tenant_id,user_id) REFERENCES users(tenant_id,id),
 FOREIGN KEY(tenant_id,company_id) REFERENCES companies(tenant_id,id)
);
