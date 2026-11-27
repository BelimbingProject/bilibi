-- Emailing a link to a document is a disclosure, so each request is one
-- audited row from the moment it is made until its outcome: queued, sent,
-- failed, suppressed (access ended before delivery) or rate_limited. Rows hold
-- ids only, never an address, a name or a link, and outlive the document as
-- evidence. `expiry_user_id` is who the expiry reminder task belongs to.
CREATE TABLE core_document_emails (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 document_id bigint NOT NULL,
 recipient_id bigint NOT NULL,
 requested_by bigint NOT NULL,
 state text NOT NULL CHECK (state IN ('queued','sent','failed','suppressed','rate_limited')),
 reason text CHECK (reason IS NULL OR length(reason) BETWEEN 1 AND 64),
 requested_at timestamptz NOT NULL DEFAULT now(),
 settled_at timestamptz,
 UNIQUE (tenant_id, id),
 FOREIGN KEY (tenant_id, recipient_id) REFERENCES users(tenant_id, id),
 FOREIGN KEY (tenant_id, requested_by) REFERENCES users(tenant_id, id)
);
CREATE INDEX core_document_emails_actor_idx ON core_document_emails(tenant_id, requested_by, requested_at);
CREATE INDEX core_document_emails_recipient_idx ON core_document_emails(tenant_id, document_id, recipient_id, requested_at);
CREATE INDEX core_document_emails_tenant_idx ON core_document_emails(tenant_id, requested_at);

ALTER TABLE core_documents ADD COLUMN expiry_user_id bigint;
