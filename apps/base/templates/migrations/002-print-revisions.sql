CREATE TABLE base_print_revisions (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id bigint NOT NULL REFERENCES tenants(id),
    company_id bigint CHECK (company_id > 0),
    template_key text NOT NULL,
    language text NOT NULL,
    contract_version text NOT NULL,
    layout text CHECK (octet_length(layout) BETWEEN 1 AND 49152),
    published_at timestamptz NOT NULL DEFAULT now()
);

-- A NULL layout restores inheritance while preserving the editor's revision.
CREATE INDEX base_print_revision_head ON base_print_revisions
    (tenant_id, template_key, language, company_id, id DESC);

CREATE TRIGGER base_print_revisions_keep
    BEFORE UPDATE OR DELETE ON base_print_revisions
    FOR EACH ROW EXECUTE FUNCTION base_template_revisions_keep();
