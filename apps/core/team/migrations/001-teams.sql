CREATE TABLE core_teams (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id bigint NOT NULL REFERENCES tenants(id),
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    kind text NOT NULL CHECK (length(kind) BETWEEN 1 AND 64),
    parent_id bigint,
    company_id bigint,
    version bigint NOT NULL DEFAULT 1,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, id),
    CHECK (parent_id IS DISTINCT FROM id),
    FOREIGN KEY (tenant_id, parent_id) REFERENCES core_teams(tenant_id, id),
    FOREIGN KEY (tenant_id, company_id) REFERENCES companies(tenant_id, id)
);
-- Sibling teams have different names; the same name may appear under another parent.
CREATE UNIQUE INDEX core_teams_sibling_name ON core_teams(tenant_id, coalesce(parent_id, 0), lower(name));
CREATE INDEX core_teams_list ON core_teams(tenant_id, lower(name), id);
CREATE INDEX core_teams_parent ON core_teams(tenant_id, parent_id) WHERE parent_id IS NOT NULL;
CREATE TABLE core_team_members (
    tenant_id bigint NOT NULL,
    team_id bigint NOT NULL,
    user_id bigint NOT NULL,
    lead boolean NOT NULL DEFAULT false,
    added_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, team_id, user_id),
    FOREIGN KEY (tenant_id, team_id) REFERENCES core_teams(tenant_id, id),
    FOREIGN KEY (tenant_id, user_id) REFERENCES users(tenant_id, id)
);
-- "Which teams is this person in" for the port and the future visibility rule.
CREATE INDEX core_team_members_user ON core_team_members(tenant_id, user_id);
