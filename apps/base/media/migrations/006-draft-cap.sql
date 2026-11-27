-- Each upload counts its uploader's unretained drafts in the tenant (issue
-- 570: at most 200 per uploader), and the uploader's own abandoned drafts are
-- found by the same columns.
CREATE INDEX base_media_attachments_uploader_drafts_idx ON base_media_attachments (tenant_id, uploaded_by_id, uploaded_by_principal, id)
    WHERE state = 'draft' AND retained_at IS NULL;
