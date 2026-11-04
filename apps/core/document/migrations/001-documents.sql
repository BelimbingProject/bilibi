-- A library of documents: `core/document`. Metadata lives here; every
-- revision's file is a private `base/artifacts` document, tracked from before
-- it is stored until it is purged by `core_document_files`. Links point at
-- records in any area through `base/records` references (`record_kind`,
-- `record_id`); no foreign key crosses to that record's module.
CREATE TABLE core_documents (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 company_id bigint NOT NULL,
 name text NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
 publish_date date NOT NULL,
 source text NOT NULL CHECK (source IN ('internal','external')),
 external_url text CHECK (external_url IS NULL OR (octet_length(external_url) BETWEEN 9 AND 2000 AND external_url LIKE 'https://%')),
 category text CHECK (category IS NULL OR length(category) BETWEEN 1 AND 64),
 sub_category text CHECK (sub_category IS NULL OR length(sub_category) BETWEEN 1 AND 64),
 document_type text CHECK (document_type IS NULL OR length(document_type) BETWEEN 1 AND 64),
 status text NOT NULL CHECK (length(status) BETWEEN 1 AND 64),
 revision integer NOT NULL CHECK (revision > 0),
 last_revision_at timestamptz NOT NULL,
 created_by bigint NOT NULL,
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now(),
 deleting_at timestamptz,
 version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
 UNIQUE (tenant_id, id),
 -- Revisions repeat the fixed source, so a revision of an internal
 -- document always has a file and one of an external document a link.
 UNIQUE (tenant_id, id, source),
 FOREIGN KEY (tenant_id, company_id) REFERENCES companies(tenant_id, id),
 FOREIGN KEY (tenant_id, created_by) REFERENCES users(tenant_id, id),
 CHECK ((source = 'external') = (external_url IS NOT NULL)),
 CHECK (sub_category IS NULL OR category IS NOT NULL)
);
CREATE INDEX core_documents_company_idx ON core_documents(tenant_id, company_id);
CREATE INDEX core_documents_name_idx ON core_documents(tenant_id, lower(name));

-- Access is granted on the document itself. Filing-company membership and
-- links to other records never imply access.
CREATE TABLE core_document_access (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 document_id bigint NOT NULL,
 principal_type text NOT NULL CHECK (principal_type IN ('user','company')),
 principal_id bigint NOT NULL,
 can_view boolean NOT NULL DEFAULT true,
 can_download boolean NOT NULL DEFAULT false,
 can_manage boolean NOT NULL DEFAULT false,
 can_share boolean NOT NULL DEFAULT false,
 granted_by bigint NOT NULL,
 granted_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (tenant_id, id),
 UNIQUE (tenant_id, document_id, principal_type, principal_id),
 FOREIGN KEY (tenant_id, document_id) REFERENCES core_documents(tenant_id, id),
 FOREIGN KEY (tenant_id, granted_by) REFERENCES users(tenant_id, id),
 CHECK (can_view OR NOT (can_download OR can_manage OR can_share))
);
CREATE INDEX core_document_access_user_idx ON core_document_access(tenant_id, principal_id, document_id) WHERE principal_type='user' AND can_view;
CREATE INDEX core_document_access_company_idx ON core_document_access(tenant_id, principal_id, document_id) WHERE principal_type='company' AND can_view;

-- Every private file this module stores, from before `base/artifacts`
-- reserves it until it is purged: `reserved` (no artifact yet), `stored`
-- (not yet a revision), and `attached` (a revision's file). The
-- artifacts owner answers from this row, and the cleanup job finds files
-- never attached here.
CREATE TABLE core_document_files (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 company_id bigint NOT NULL,
 subject uuid NOT NULL,
 artifact_id text CHECK (artifact_id IS NULL OR octet_length(artifact_id) BETWEEN 1 AND 64),
 file_name text NOT NULL CHECK (length(file_name) BETWEEN 1 AND 255),
 content_type text NOT NULL CHECK (octet_length(content_type) BETWEEN 1 AND 255),
 byte_size bigint NOT NULL CHECK (byte_size > 0),
 sha256 text NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
 state text NOT NULL CHECK (state IN ('reserved','stored','attached')),
 stored_by bigint NOT NULL,
 stored_at timestamptz NOT NULL DEFAULT now(),
 attached_at timestamptz,
 UNIQUE (tenant_id, id),
 UNIQUE (tenant_id, subject),
 UNIQUE (tenant_id, artifact_id),
 FOREIGN KEY (tenant_id, company_id) REFERENCES companies(tenant_id, id),
 FOREIGN KEY (tenant_id, stored_by) REFERENCES users(tenant_id, id),
 CHECK ((artifact_id IS NULL) = (state = 'reserved')),
 CHECK (state <> 'attached' OR attached_at IS NOT NULL)
);
CREATE INDEX core_document_files_state_idx ON core_document_files(tenant_id, state, stored_at) WHERE state <> 'attached';

-- One row per revision. Numbers are unique per document, rise and keep their
-- gaps; each internal revision has its own file.
CREATE TABLE core_document_revisions (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 document_id bigint NOT NULL,
 source text NOT NULL CHECK (source IN ('internal','external')),
 revision integer NOT NULL CHECK (revision > 0),
 name text NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
 artifact_id text,
 file_name text CHECK (file_name IS NULL OR length(file_name) BETWEEN 1 AND 255),
 content_type text CHECK (content_type IS NULL OR octet_length(content_type) BETWEEN 1 AND 255),
 byte_size bigint CHECK (byte_size IS NULL OR byte_size > 0),
 sha256 text CHECK (sha256 IS NULL OR sha256 ~ '^[0-9a-f]{64}$'),
 external_url text CHECK (external_url IS NULL OR (octet_length(external_url) BETWEEN 9 AND 2000 AND external_url LIKE 'https://%')),
 change_log text NOT NULL CHECK (length(change_log) BETWEEN 1 AND 2000),
 scan_status text NOT NULL DEFAULT 'not_scanned' CHECK (scan_status IN ('not_scanned','clean','quarantined')),
 created_by bigint NOT NULL,
 created_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (tenant_id, id),
 UNIQUE (tenant_id, document_id, revision),
 UNIQUE (tenant_id, artifact_id),
 FOREIGN KEY (tenant_id, document_id, source) REFERENCES core_documents(tenant_id, id, source),
 FOREIGN KEY (tenant_id, artifact_id) REFERENCES core_document_files(tenant_id, artifact_id),
 FOREIGN KEY (tenant_id, created_by) REFERENCES users(tenant_id, id),
 CHECK (
  (source = 'internal' AND artifact_id IS NOT NULL AND file_name IS NOT NULL AND content_type IS NOT NULL AND byte_size IS NOT NULL AND sha256 IS NOT NULL AND external_url IS NULL)
  OR (source = 'external' AND external_url IS NOT NULL AND artifact_id IS NULL AND file_name IS NULL AND content_type IS NULL AND byte_size IS NULL AND sha256 IS NULL)
 )
);
CREATE INDEX core_document_revisions_sha_idx ON core_document_revisions(tenant_id, sha256) WHERE sha256 IS NOT NULL;

-- The records a document relates to. A document may relate to many records.
CREATE TABLE core_document_links (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 document_id bigint NOT NULL,
 record_kind text NOT NULL CHECK (octet_length(record_kind) BETWEEN 1 AND 100),
 record_id text NOT NULL CHECK (octet_length(record_id) BETWEEN 1 AND 64),
 linked_by bigint NOT NULL,
 linked_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (tenant_id, document_id, record_kind, record_id),
 FOREIGN KEY (tenant_id, document_id) REFERENCES core_documents(tenant_id, id),
 FOREIGN KEY (tenant_id, linked_by) REFERENCES users(tenant_id, id)
);
CREATE INDEX core_document_links_record_idx ON core_document_links(tenant_id, record_kind, record_id);
