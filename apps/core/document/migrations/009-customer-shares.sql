-- A document shared with a portal customer (issue 524): one Person, or one
-- Company for every business customer who signs in through it. The Person
-- and Company are plain ids (no foreign key crosses into Core Person or Core
-- Company, so a share never blocks a change there); the share was checked
-- against `base/records` when it was made. Revoking a share deletes its row;
-- `expires_at` ends it like an access grant's end.
CREATE TABLE core_document_customer_shares (
 id bigserial PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 document_id bigint NOT NULL,
 person_id bigint,
 company_id bigint,
 expires_at timestamptz,
 shared_by bigint NOT NULL,
 shared_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (tenant_id, id),
 FOREIGN KEY (tenant_id, document_id) REFERENCES core_documents(tenant_id, id),
 FOREIGN KEY (tenant_id, shared_by) REFERENCES users(tenant_id, id),
 CONSTRAINT core_document_customer_shares_one_recipient CHECK ((person_id IS NULL) <> (company_id IS NULL))
);
CREATE UNIQUE INDEX core_document_customer_shares_person_key ON core_document_customer_shares(tenant_id, document_id, person_id) WHERE person_id IS NOT NULL;
CREATE UNIQUE INDEX core_document_customer_shares_company_key ON core_document_customer_shares(tenant_id, document_id, company_id) WHERE company_id IS NOT NULL;
-- The portal list starts from the customer's own Person and Company.
CREATE INDEX core_document_customer_shares_person_idx ON core_document_customer_shares(tenant_id, person_id, document_id) WHERE person_id IS NOT NULL;
CREATE INDEX core_document_customer_shares_company_idx ON core_document_customer_shares(tenant_id, company_id, document_id) WHERE company_id IS NOT NULL;
