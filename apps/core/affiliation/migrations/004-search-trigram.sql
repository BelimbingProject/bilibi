-- Contacts search matches any part of a current work email, phone or role
-- (#589). Trigram indexes serve that substring match (`LIKE '%term%'`, Grid's
-- text search), so a search reads the affiliations that match instead of
-- every one of the tenant. pg_trgm is decided in ADR 06.
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE INDEX person_affiliations_work_email_trgm ON person_affiliations USING gin (lower(work_email) gin_trgm_ops) WHERE work_email IS NOT NULL;
CREATE INDEX person_affiliations_work_phone_trgm ON person_affiliations USING gin (lower(work_phone) gin_trgm_ops) WHERE work_phone IS NOT NULL;
CREATE INDEX person_affiliations_title_trgm ON person_affiliations USING gin (lower(title) gin_trgm_ops) WHERE title IS NOT NULL;
