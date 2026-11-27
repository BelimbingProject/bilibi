-- One override per sibling group. An empty parent names the staff root.
CREATE TABLE base_menu_order (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id bigint NOT NULL REFERENCES tenants(id),
    parent text NOT NULL,
    item_ids text[] NOT NULL,
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    UNIQUE (tenant_id, parent)
);
