-- An owner may keep an ordinary document until it deletes it (`keep_in`): a
-- library file its record lists. Such a row has neither an expiry nor a
-- retain-until; a records-grade row still never has both.
ALTER TABLE base_artifacts DROP CONSTRAINT base_artifacts_retention_exclusive;
ALTER TABLE base_artifacts
    ADD CONSTRAINT base_artifacts_retention_exclusive CHECK (retain_until IS NULL OR expires_at IS NULL);
