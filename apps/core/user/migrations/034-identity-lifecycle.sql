-- Issue 787: installation identities outlive their last customer membership
-- until the installation owner's retention policy disposes of them.
ALTER TABLE user_identities
 ADD COLUMN orphaned_at timestamptz,
 ADD CONSTRAINT user_identities_orphaned_check CHECK (
  orphaned_at IS NULL OR (kind='staff' AND status='active' AND deleted_at IS NULL)
 );

CREATE INDEX user_identities_orphaned_at_idx
 ON user_identities(orphaned_at,id)
 WHERE orphaned_at IS NOT NULL AND status='active' AND deleted_at IS NULL;
