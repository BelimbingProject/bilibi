-- A retained draft may be released (issue 602): `Media::release_drafts` clears
-- `retained_at` once its owner no longer needs the draft (a held document
-- upload whose scan settled), so abandoned-draft cleanup removes it as usual.
-- Replaces 005's guard, unchanged but for that one transition: a draft's
-- retention may be cleared, never moved, and submitted and erased rows stay
-- immutable.
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
  OR (OLD.retained_at IS NOT NULL AND NEW.retained_at IS NOT NULL AND NEW.retained_at IS DISTINCT FROM OLD.retained_at) THEN
  RAISE EXCEPTION 'attachment is immutable' USING ERRCODE = 'restrict_violation';
 END IF;
 RETURN NEW;
END $$;
