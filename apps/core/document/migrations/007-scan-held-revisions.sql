-- A Media upload can be filed as an unreadable Document revision while its
-- scan is pending or failed. A clean verdict later copies its bytes into a
-- private Artifact; no placeholder Artifact id is ever stored.
ALTER TABLE core_document_files
    ADD COLUMN media_public_id text CHECK (media_public_id IS NULL OR octet_length(media_public_id) = 26),
    ADD COLUMN scan_status text NOT NULL DEFAULT 'not_scanned'
        CHECK (scan_status IN ('not_scanned', 'pending', 'clean', 'quarantined', 'failed', 'unscanned'));
UPDATE core_document_files SET scan_status='clean' WHERE artifact_id IS NOT NULL;
ALTER TABLE core_document_files
    ADD CONSTRAINT core_document_files_media_public_unique UNIQUE (tenant_id, media_public_id);
ALTER TABLE core_document_files DROP CONSTRAINT core_document_files_check;
ALTER TABLE core_document_files ADD CONSTRAINT core_document_files_storage_check
    CHECK (
        (state = 'reserved' AND artifact_id IS NULL)
        OR (state IN ('stored', 'attached') AND (artifact_id IS NOT NULL OR media_public_id IS NOT NULL))
    );

-- The ledger id, rather than the still-absent Artifact id, anchors a held
-- revision. Existing clean revisions backfill from their protected Artifact.
ALTER TABLE core_document_revisions
    ADD COLUMN file_id bigint,
    ADD COLUMN scan_job_id bigint;
UPDATE core_document_revisions AS r SET file_id=f.id
FROM core_document_files AS f
WHERE r.tenant_id=f.tenant_id AND r.artifact_id=f.artifact_id
  AND r.source='internal';
ALTER TABLE core_document_revisions
    ADD CONSTRAINT core_document_revisions_file_fk
    FOREIGN KEY (tenant_id, file_id) REFERENCES core_document_files(tenant_id, id);
ALTER TABLE core_document_revisions
    ADD CONSTRAINT core_document_revisions_file_unique UNIQUE (tenant_id, file_id);
ALTER TABLE core_document_revisions DROP CONSTRAINT core_document_revisions_scan_status_check;
ALTER TABLE core_document_revisions
    ADD CONSTRAINT core_document_revisions_scan_status_check
    CHECK (scan_status IN ('not_scanned', 'pending', 'clean', 'quarantined', 'failed', 'unscanned'));
ALTER TABLE core_document_revisions DROP CONSTRAINT core_document_revisions_check;
ALTER TABLE core_document_revisions ADD CONSTRAINT core_document_revisions_source_file_check
    CHECK (
        (source = 'internal' AND file_id IS NOT NULL AND file_name IS NOT NULL
            AND content_type IS NOT NULL AND byte_size IS NOT NULL AND sha256 IS NOT NULL
            AND external_url IS NULL
            AND (
                (artifact_id IS NOT NULL AND scan_status IN ('clean', 'quarantined'))
                OR (artifact_id IS NULL AND scan_status IN ('pending', 'quarantined', 'failed', 'unscanned'))
            ))
        OR (source = 'external' AND external_url IS NOT NULL AND file_id IS NULL
            AND artifact_id IS NULL AND file_name IS NULL AND content_type IS NULL
            AND byte_size IS NULL AND sha256 IS NULL AND scan_status = 'not_scanned')
    );
