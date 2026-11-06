-- Derived expression indexes follow every owner write and erasure without
-- another stored copy of personal information or a maintenance write path.
CREATE INDEX person_affiliations_email_key
    ON person_affiliations(tenant_id, lower(work_email), id) WHERE unlinked_at IS NULL;
CREATE INDEX person_affiliations_phone_key
    ON person_affiliations(tenant_id, core_person_phone_key(work_phone), id) WHERE unlinked_at IS NULL;
CREATE INDEX person_affiliations_uncomparable_phone ON person_affiliations(tenant_id)
    WHERE unlinked_at IS NULL AND work_phone IS NOT NULL AND core_person_phone_key(work_phone) IS NULL;
