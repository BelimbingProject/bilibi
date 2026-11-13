-- A file stored but never attached is discarded by a delayed Queue job
-- (`files::DiscardJob`). The job first moves the row to `discarding`, which
-- no write can attach, then deletes its Artifact, then the row.
ALTER TABLE core_document_files DROP CONSTRAINT core_document_files_state_check;
ALTER TABLE core_document_files ADD CONSTRAINT core_document_files_state_check
    CHECK (state IN ('reserved', 'stored', 'attached', 'discarding'));
ALTER TABLE core_document_files DROP CONSTRAINT core_document_files_storage_check;
ALTER TABLE core_document_files ADD CONSTRAINT core_document_files_storage_check
    CHECK (
        (state = 'reserved' AND artifact_id IS NULL)
        OR (state IN ('stored', 'attached') AND (artifact_id IS NOT NULL OR media_public_id IS NOT NULL))
        OR state = 'discarding'
    );
