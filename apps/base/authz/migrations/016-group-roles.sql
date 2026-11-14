-- Provider-managed groups (a Core team): members hold the group's roles while
-- they are members. Authz owns these rows; the group's owner writes them only
-- through `Grants` inside its own MutationTransaction, so Base never reads the
-- owner's tables. `source` is the owner's noun ("team"), `label` its name.
CREATE TABLE authz_groups (
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 source text NOT NULL CHECK (source ~ '^[a-z][a-z_]{0,39}$'),
 group_id bigint NOT NULL CHECK (group_id > 0),
 label text NOT NULL CHECK (length(label) BETWEEN 1 AND 200),
 company_id bigint CHECK (company_id IS NULL OR company_id > 0),
 PRIMARY KEY (tenant_id, source, group_id)
);
CREATE TABLE authz_group_roles (
 tenant_id bigint NOT NULL,
 source text NOT NULL,
 group_id bigint NOT NULL,
 role_id bigint NOT NULL,
 PRIMARY KEY (tenant_id, source, group_id, role_id),
 FOREIGN KEY (tenant_id, source, group_id) REFERENCES authz_groups(tenant_id, source, group_id) ON DELETE CASCADE,
 FOREIGN KEY (tenant_id, role_id) REFERENCES roles(tenant_id, id) ON DELETE CASCADE
);
CREATE TABLE authz_group_members (
 tenant_id bigint NOT NULL,
 source text NOT NULL,
 group_id bigint NOT NULL,
 user_id bigint NOT NULL CHECK (user_id > 0),
 PRIMARY KEY (tenant_id, source, group_id, user_id),
 FOREIGN KEY (tenant_id, source, group_id) REFERENCES authz_groups(tenant_id, source, group_id) ON DELETE CASCADE
);
CREATE INDEX authz_group_members_user ON authz_group_members(tenant_id, user_id);

-- `via` names where a role came from ("team Line 2 crew"); NULL for a manual
-- or sign-in provider membership. A group role is scoped to the group's
-- company (ADR 20) and is never a grant-all or engagement role, even if a
-- role changed after it was attached. Keep the applied 015 definition frozen.
CREATE OR REPLACE VIEW effective_principal_roles AS
 SELECT p.tenant_id,p.user_id,p.role_id,p.company_id,NULL::text AS via FROM principal_roles p
 JOIN roles r ON r.tenant_id=p.tenant_id AND r.id=p.role_id
 WHERE (NOT r.requires_engagement OR (p.company_id IS NOT NULL AND length(trim(p.purpose)) > 0))
   AND (valid_from IS NULL OR valid_from <= (clock_timestamp() AT TIME ZONE 'UTC')::date)
   AND (valid_until IS NULL OR valid_until >= (clock_timestamp() AT TIME ZONE 'UTC')::date)
 UNION SELECT p.tenant_id,p.user_id,p.role_id,NULL::bigint,NULL::text FROM authz_identity_roles p
 JOIN roles r ON r.tenant_id=p.tenant_id AND r.id=p.role_id WHERE NOT r.requires_engagement
 UNION SELECT m.tenant_id,m.user_id,g.role_id,o.company_id,o.source||' '||o.label FROM authz_group_members m
 JOIN authz_groups o ON o.tenant_id=m.tenant_id AND o.source=m.source AND o.group_id=m.group_id
 JOIN authz_group_roles g ON g.tenant_id=m.tenant_id AND g.source=m.source AND g.group_id=m.group_id
 JOIN roles r ON r.tenant_id=g.tenant_id AND r.id=g.role_id WHERE NOT r.requires_engagement AND NOT r.grant_all;

DROP FUNCTION authz_roles_for(bigint, bigint, bigint);
CREATE FUNCTION authz_roles_for(target_tenant bigint, target_user bigint, target_company bigint)
RETURNS TABLE(tenant_id bigint,user_id bigint,role_id bigint,company_id bigint,via text)
LANGUAGE sql VOLATILE AS $$
 SELECT DISTINCT p.tenant_id,p.user_id,p.role_id,p.company_id,p.via
 FROM effective_principal_roles p
 WHERE p.tenant_id=target_tenant
 AND (target_user IS NULL OR p.user_id=target_user)
 AND (p.company_id IS NULL OR p.company_id=target_company)
$$;

ALTER TABLE authz_decision_log ADD COLUMN via text CHECK (via IS NULL OR length(via) <= 300);
