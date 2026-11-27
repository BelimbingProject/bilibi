-- A declared system principal may store private attachments (issue 434): a
-- module's mailbox import or connector job keeps the files it receives. Its
-- uploads name the principal, never an account, so an attachment row records
-- exactly one of `uploaded_by_id` (a user) and `uploaded_by_principal` (a
-- system principal), and the asset row the same of its creator. The length
-- bound is Audit's `system_principal` column width.
ALTER TABLE base_media_attachments
    DROP CONSTRAINT base_media_attachments_uploaded_by_type_check,
    ALTER COLUMN uploaded_by_id DROP NOT NULL,
    ADD COLUMN uploaded_by_principal text CHECK (length(uploaded_by_principal) BETWEEN 1 AND 100),
    ADD CONSTRAINT base_media_attachments_uploader CHECK (
        (uploaded_by_type = 'user' AND uploaded_by_id IS NOT NULL AND uploaded_by_principal IS NULL)
        OR (uploaded_by_type = 'system' AND uploaded_by_id IS NULL AND uploaded_by_principal IS NOT NULL));
ALTER TABLE base_media_assets
    ADD COLUMN created_by_principal text CHECK (length(created_by_principal) BETWEEN 1 AND 100),
    ADD CONSTRAINT base_media_assets_creator CHECK (created_by_id IS NULL OR created_by_principal IS NULL),
    -- Only a private original: a principal never stores a public file or a derivative.
    ADD CONSTRAINT base_media_assets_principal_private CHECK (created_by_principal IS NULL OR (private AND parent_id IS NULL));

-- Replaces 004's guard, unchanged but for the new uploader column, which is
-- as immutable as the account it stands beside.
CREATE OR REPLACE FUNCTION base_media_attachments_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP = 'DELETE' THEN
  IF OLD.state <> 'draft' THEN
   RAISE EXCEPTION 'submitted attachments are immutable' USING ERRCODE = 'restrict_violation';
  END IF;
  RETURN OLD;
 END IF;
 IF NEW.public_id IS DISTINCT FROM OLD.public_id
  OR NEW.tenant_id IS DISTINCT FROM OLD.tenant_id
  OR NEW.subject_type IS DISTINCT FROM OLD.subject_type
  OR NEW.subject_id IS DISTINCT FROM OLD.subject_id
  OR NEW.uploaded_by_type IS DISTINCT FROM OLD.uploaded_by_type
  OR NEW.uploaded_by_id IS DISTINCT FROM OLD.uploaded_by_id
  OR NEW.uploaded_by_principal IS DISTINCT FROM OLD.uploaded_by_principal
  OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
  RAISE EXCEPTION 'attachment is immutable' USING ERRCODE = 'restrict_violation';
 END IF;
 IF OLD.state = 'erased' THEN
  IF OLD.bytes_removed_at IS NULL AND NEW.bytes_removed_at IS NOT NULL
     AND (NEW.state, NEW.media_asset_id, NEW.submitted_at, NEW.retained_at, NEW.erased_at, NEW.erased_by, NEW.erased_disk, NEW.erased_key)
         IS NOT DISTINCT FROM
         (OLD.state, OLD.media_asset_id, OLD.submitted_at, OLD.retained_at, OLD.erased_at, OLD.erased_by, OLD.erased_disk, OLD.erased_key) THEN
   RETURN NEW;
  END IF;
  RAISE EXCEPTION 'erased attachment is immutable' USING ERRCODE = 'restrict_violation';
 END IF;
 IF NEW.state = 'erased'
    AND base_media_erasing(TG_RELID)
    AND NEW.submitted_at IS NOT DISTINCT FROM OLD.submitted_at
    AND NEW.retained_at IS NOT DISTINCT FROM OLD.retained_at
    AND NEW.bytes_removed_at IS NULL THEN
  RETURN NEW;
 END IF;
 IF OLD.state = 'submitted' THEN
  RAISE EXCEPTION 'submitted attachments are immutable' USING ERRCODE = 'restrict_violation';
 END IF;
 IF NEW.media_asset_id IS DISTINCT FROM OLD.media_asset_id
  OR NEW.state = 'erased'
  OR (OLD.retained_at IS NOT NULL AND NEW.retained_at IS DISTINCT FROM OLD.retained_at) THEN
  RAISE EXCEPTION 'attachment is immutable' USING ERRCODE = 'restrict_violation';
 END IF;
 RETURN NEW;
END $$;
