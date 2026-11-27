-- Every fresh answer `check` gives at dispatch time. The caller's key
-- correlates checks with its delivery record; delivery idempotency remains
-- the sender's responsibility.
CREATE TABLE core_consent_log (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 company_id bigint NOT NULL,
 person_id bigint CHECK (person_id IS NULL OR person_id > 0),
 address text CHECK (address IS NULL OR octet_length(address) BETWEEN 1 AND 254),
 channel text NOT NULL CHECK (channel IN ('email','sms','messaging','voice','chat')),
 purpose text NOT NULL CHECK (octet_length(purpose) BETWEEN 1 AND 64),
 key text NOT NULL CHECK (octet_length(key) BETWEEN 1 AND 200),
 at timestamptz NOT NULL,
 outcome text NOT NULL CHECK (outcome IN ('allowed','no_consent','withdrawn','suppressed')),
 caller text NOT NULL CHECK (octet_length(caller) BETWEEN 1 AND 100),
 logged_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (tenant_id, id),
 FOREIGN KEY (tenant_id, company_id) REFERENCES companies(tenant_id, id),
 CHECK (num_nonnulls(person_id, address) >= 1)
);
CREATE INDEX core_consent_log_person_idx ON core_consent_log(tenant_id, company_id, person_id, at) WHERE person_id IS NOT NULL;
CREATE INDEX core_consent_log_address_idx ON core_consent_log(tenant_id, company_id, address, at) WHERE address IS NOT NULL;
CREATE INDEX core_consent_log_key_idx ON core_consent_log(tenant_id, key, at DESC);
