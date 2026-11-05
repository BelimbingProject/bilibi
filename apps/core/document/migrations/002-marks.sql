-- Each person's favourites and follows. Written by the library actions
-- (slice 3); a document's delete removes its marks.
CREATE TABLE core_document_marks (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 user_id bigint NOT NULL,
 document_id bigint NOT NULL,
 favourite boolean NOT NULL DEFAULT false,
 following boolean NOT NULL DEFAULT false,
 updated_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (tenant_id, user_id, document_id),
 FOREIGN KEY (tenant_id, user_id) REFERENCES users(tenant_id, id),
 FOREIGN KEY (tenant_id, document_id) REFERENCES core_documents(tenant_id, id)
);
CREATE INDEX core_document_marks_document_idx ON core_document_marks(tenant_id, document_id);
