-- Contacts search matches any part of a name, code, email or phone (#589).
-- Trigram indexes serve that substring match (`LIKE '%term%'`), so a search
-- reads the contacts that match instead of every contact of the tenant.
-- pg_trgm is decided in ADR 29; it is a trusted extension, so the database
-- owner installs it.
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE INDEX persons_name_trgm ON persons USING gin (lower(name) gin_trgm_ops);
CREATE INDEX persons_code_trgm ON persons USING gin (lower(code) gin_trgm_ops);
CREATE INDEX persons_email_trgm ON persons USING gin (lower(email) gin_trgm_ops) WHERE email IS NOT NULL;
CREATE INDEX persons_phone_trgm ON persons USING gin (lower(phone) gin_trgm_ops) WHERE phone IS NOT NULL;
