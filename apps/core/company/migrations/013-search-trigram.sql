-- Contacts search matches any part of a contact's company name (#589), and
-- Grid's text search matches company names the same way. A trigram index
-- serves that substring match (`LIKE '%term%'`), so a search reads the
-- companies that match instead of every one of the tenant. pg_trgm is
-- decided in ADR 29.
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE INDEX companies_name_trgm ON companies USING gin (lower(name) gin_trgm_ops) WHERE deleted_at IS NULL;
