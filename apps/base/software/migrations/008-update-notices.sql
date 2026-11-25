CREATE TABLE base_software_update_notices (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id bigint NOT NULL REFERENCES tenants(id),
    recipient bigint NOT NULL,
    repository_id bigint NOT NULL,
    tag text NOT NULL,
    UNIQUE (tenant_id, recipient, repository_id, tag)
);
