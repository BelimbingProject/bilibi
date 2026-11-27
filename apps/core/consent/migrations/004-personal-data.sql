-- Consent's personal fields (base_personal_data_kinds, ADR 05) and what an
-- erasure leaves behind. An erased decision or check keeps its bare fact
-- (company, purpose, channel, outcome, times) with no address, evidence or
-- correlation key. An erased active suppression keeps stopping sends: its
-- address is replaced by a keyed hash (HMAC-SHA-256 under the tenant's key in
-- `core_consent_keys`, sealed with the installation's Settings key), which the
-- dispatch check computes for the address it is asked about.
ALTER TABLE core_consent_decisions ADD COLUMN erased_at timestamptz;
ALTER TABLE core_consent_decisions DROP CONSTRAINT core_consent_decisions_check;
ALTER TABLE core_consent_decisions ADD CONSTRAINT core_consent_decisions_subject_check
    CHECK (num_nonnulls(person_id, address) = 1 OR (erased_at IS NOT NULL AND person_id IS NULL AND address IS NULL));

ALTER TABLE core_consent_suppressions ADD COLUMN address_hash bytea CHECK (address_hash IS NULL OR octet_length(address_hash) = 32);
ALTER TABLE core_consent_suppressions ADD COLUMN erased_at timestamptz;
ALTER TABLE core_consent_suppressions DROP CONSTRAINT core_consent_suppressions_check;
ALTER TABLE core_consent_suppressions ADD CONSTRAINT core_consent_suppressions_subject_check
    CHECK (num_nonnulls(person_id, address, address_hash) = 1 OR (erased_at IS NOT NULL AND num_nonnulls(person_id, address, address_hash) = 0));
ALTER TABLE core_consent_suppressions ADD CONSTRAINT core_consent_suppressions_hash_check
    CHECK (address_hash IS NULL OR erased_at IS NOT NULL);
CREATE INDEX core_consent_suppressions_hash_idx ON core_consent_suppressions(tenant_id, company_id, channel, address_hash)
    WHERE address_hash IS NOT NULL AND lifted_at IS NULL;

ALTER TABLE core_consent_log ADD COLUMN erased_at timestamptz;
ALTER TABLE core_consent_log DROP CONSTRAINT core_consent_log_check;
ALTER TABLE core_consent_log ADD CONSTRAINT core_consent_log_subject_check
    CHECK (num_nonnulls(person_id, address) >= 1 OR erased_at IS NOT NULL);

-- One random 32-byte key per tenant, created by the first erasure that hashes
-- a suppressed address, sealed with `Settings::seal_owned`. Rows are never
-- replaced: a new key would stop matching every hashed suppression.
CREATE TABLE core_consent_keys (
 tenant_id bigint PRIMARY KEY REFERENCES tenants(id),
 sealed jsonb NOT NULL,
 created_at timestamptz NOT NULL DEFAULT now()
);

INSERT INTO base_personal_data_kinds(kind, module, fields, subject_identifier, person) VALUES
    ('core.consent.decision', 'core/consent', ARRAY['address', 'evidence'], false, false),
    ('core.consent.current', 'core/consent', ARRAY['address'], false, false),
    ('core.consent.suppression', 'core/consent', ARRAY['address', 'evidence', 'lift_reason'], false, false),
    ('core.consent.log', 'core/consent', ARRAY['address', 'key'], false, false);
