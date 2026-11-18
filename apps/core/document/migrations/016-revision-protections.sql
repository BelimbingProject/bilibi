-- A related record may pin one exact revision. Its number and document ID
-- remain durable: deletion and moving/renumbering a source both refuse.
CREATE TABLE core_document_revision_protections (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id bigint NOT NULL REFERENCES tenants(id),
    document_id bigint NOT NULL,
    revision integer NOT NULL CHECK (revision > 0),
    record_kind text NOT NULL CHECK (octet_length(record_kind) BETWEEN 1 AND 100),
    record_id text NOT NULL CHECK (octet_length(record_id) BETWEEN 1 AND 64),
    reason text NOT NULL CHECK (length(reason) BETWEEN 1 AND 255),
    protected_by bigint NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, id),
    UNIQUE (tenant_id, document_id, revision, record_kind, record_id),
    FOREIGN KEY (tenant_id, document_id, revision)
        REFERENCES core_document_revisions(tenant_id, document_id, revision),
    FOREIGN KEY (tenant_id, document_id, record_kind, record_id)
        REFERENCES core_document_links(tenant_id, document_id, record_kind, record_id),
    FOREIGN KEY (tenant_id, protected_by) REFERENCES users(tenant_id, id)
);
CREATE INDEX core_document_revision_protections_record_idx
    ON core_document_revision_protections(tenant_id, record_kind, record_id);

CREATE FUNCTION core_document_revision_protections_append_only() RETURNS trigger
LANGUAGE plpgsql AS 'BEGIN
    RAISE EXCEPTION ''core_document_revision_protections is append-only''
        USING ERRCODE = ''check_violation'', CONSTRAINT = ''core_document_revision_protections_append_only'';
END;';
CREATE TRIGGER core_document_revision_protections_append_only
    BEFORE UPDATE OR DELETE ON core_document_revision_protections
    FOR EACH ROW EXECUTE FUNCTION core_document_revision_protections_append_only();
