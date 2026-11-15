-- Tenant label overrides (#686, ADR 28): plain text a tenant shows instead of a
-- module's own words for a menu item or a record type, for example
-- "Companies" as "Clients". `subject` is the stable ID the module declared (a
-- menu item ID, or a Grid table ID); an override never changes an operation
-- key, route or permission key. It is data only, never code. A row exists only
-- while an override does; clearing it returns the module's own word.
CREATE TABLE base_labels (
 id bigint GENERATED ALWAYS AS IDENTITY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 kind text NOT NULL CHECK (kind IN ('menu', 'record')),
 subject text NOT NULL CHECK (subject ~ '^[a-z0-9][a-z0-9._-]{0,127}$'),
 label text NOT NULL CHECK (length(btrim(label)) BETWEEN 1 AND 60),
 version bigint NOT NULL DEFAULT 1 CHECK (version >= 1),
 updated_at timestamptz NOT NULL DEFAULT now(),
 CONSTRAINT base_labels_pkey PRIMARY KEY (id),
 CONSTRAINT base_labels_subject_key UNIQUE (tenant_id, kind, subject)
);
