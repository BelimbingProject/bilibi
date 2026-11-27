-- Named list views one person saves and shares (#686). A view is the page's
-- own arrangement (`view`: columns, lenses, row height, change-since date) and
-- the page's query (`params`: its sort, search and filters), kept as plain
-- JSON. It stores no rows and no access: every reader opens it through the
-- page, which applies their own grants, field restrictions and reach.
-- `shared_with_everyone` shares it with the tenant; the role rows limit it to
-- the holders of those roles instead; neither keeps it to its owner.
CREATE TABLE base_grid_shared_views (
 id bigint GENERATED ALWAYS AS IDENTITY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 owner_id bigint NOT NULL CHECK (owner_id > 0),
 page text NOT NULL CHECK (page ~ '^[a-z][a-z0-9_.-]{0,89}$'),
 name text NOT NULL CHECK (length(btrim(name)) BETWEEN 1 AND 80),
 view jsonb NOT NULL CHECK (jsonb_typeof(view) = 'object' AND octet_length(view::text) <= 16384),
 shared_with_everyone boolean NOT NULL DEFAULT false,
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now(),
 CONSTRAINT base_grid_shared_views_pkey PRIMARY KEY (id),
 CONSTRAINT base_grid_shared_views_tenant_id_key UNIQUE (tenant_id, id)
);
-- One name per page, whatever its case, so a list of views is unambiguous.
CREATE UNIQUE INDEX base_grid_shared_views_name_idx ON base_grid_shared_views (tenant_id, page, lower(name));
CREATE INDEX base_grid_shared_views_owner_idx ON base_grid_shared_views (tenant_id, owner_id);

-- The roles a view is shared with. A role removed from the tenant drops its
-- shares with it.
CREATE TABLE base_grid_shared_view_roles (
 tenant_id bigint NOT NULL,
 view_id bigint NOT NULL,
 role_id bigint NOT NULL,
 CONSTRAINT base_grid_shared_view_roles_pkey PRIMARY KEY (tenant_id, view_id, role_id),
 CONSTRAINT base_grid_shared_view_roles_view_fkey FOREIGN KEY (tenant_id, view_id) REFERENCES base_grid_shared_views (tenant_id, id) ON DELETE CASCADE,
 CONSTRAINT base_grid_shared_view_roles_role_fkey FOREIGN KEY (tenant_id, role_id) REFERENCES roles (tenant_id, id) ON DELETE CASCADE
);
CREATE INDEX base_grid_shared_view_roles_role_idx ON base_grid_shared_view_roles (tenant_id, role_id);
