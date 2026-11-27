CREATE TABLE base_template_revisions (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id bigint NOT NULL REFERENCES tenants(id),
    company_id bigint CHECK (company_id > 0),
    template_key text NOT NULL,
    language text NOT NULL,
    slots jsonb CHECK (jsonb_typeof(slots) = 'object'),
    published_at timestamptz NOT NULL DEFAULT now()
);

-- NULL slots restore inheritance. The marker preserves the concurrency token
-- even when an override has been removed and published again.
CREATE INDEX base_template_revision_head ON base_template_revisions
    (tenant_id, template_key, language, company_id, id DESC);

CREATE FUNCTION base_template_revisions_keep() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'Template revisions are immutable';
END;
$$;

CREATE TRIGGER base_template_revisions_keep
    BEFORE UPDATE OR DELETE ON base_template_revisions
    FOR EACH ROW EXECUTE FUNCTION base_template_revisions_keep();
