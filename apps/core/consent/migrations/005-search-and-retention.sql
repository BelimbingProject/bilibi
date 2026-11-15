-- Indexes for the reads that scanned whole tables (#604), and the dispatch
-- log's retention sweep. `pg_trgm` is allowed by decision record 29; IF NOT
-- EXISTS lets another module's migration enable it too.
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- The privacy locate matches an address-only row by an email exactly, or by a
-- phone's digits anywhere in the address's digits (`privacy.rs`, `MATCH`).
-- These indexes are written to that predicate, so it reads them unchanged.
CREATE INDEX core_consent_decisions_identifier_idx ON core_consent_decisions(tenant_id, address)
    WHERE person_id IS NULL AND address IS NOT NULL;
CREATE INDEX core_consent_decisions_digits_idx ON core_consent_decisions
    USING gin (regexp_replace(address, '[^0-9]', '', 'g') gin_trgm_ops) WHERE person_id IS NULL AND address IS NOT NULL;
CREATE INDEX core_consent_current_identifier_idx ON core_consent_current(tenant_id, address)
    WHERE person_id IS NULL AND address IS NOT NULL;
CREATE INDEX core_consent_current_digits_idx ON core_consent_current
    USING gin (regexp_replace(address, '[^0-9]', '', 'g') gin_trgm_ops) WHERE person_id IS NULL AND address IS NOT NULL;
CREATE INDEX core_consent_suppressions_identifier_idx ON core_consent_suppressions(tenant_id, address)
    WHERE person_id IS NULL AND address IS NOT NULL;
CREATE INDEX core_consent_suppressions_digits_idx ON core_consent_suppressions
    USING gin (regexp_replace(address, '[^0-9]', '', 'g') gin_trgm_ops) WHERE person_id IS NULL AND address IS NOT NULL;
CREATE INDEX core_consent_log_identifier_idx ON core_consent_log(tenant_id, address)
    WHERE person_id IS NULL AND address IS NOT NULL;
CREATE INDEX core_consent_log_digits_idx ON core_consent_log
    USING gin (regexp_replace(address, '[^0-9]', '', 'g') gin_trgm_ops) WHERE person_id IS NULL AND address IS NOT NULL;

-- The Suppressions list searches part of an address, or a person's id.
CREATE INDEX core_consent_suppressions_search_idx ON core_consent_suppressions USING gin (lower(address) gin_trgm_ops);
CREATE INDEX core_consent_suppressions_person_search_idx ON core_consent_suppressions(tenant_id, company_id, person_id)
    WHERE person_id IS NOT NULL;

-- The retention sweep reads the checks not yet anonymised that are older than
-- the period; an anonymised one leaves this index.
CREATE INDEX core_consent_log_unswept_idx ON core_consent_log(tenant_id, at) WHERE erased_at IS NULL;
