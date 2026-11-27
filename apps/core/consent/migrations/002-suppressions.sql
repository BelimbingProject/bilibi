-- Addresses and people not to be contacted on a channel: a hard bounce, a
-- complaint or an unsubscribe. A person-level row covers every address of
-- that person on the channel. One active row per channel, address or
-- person, and reason; lifting keeps the row with who lifted it and why.
CREATE TABLE core_consent_suppressions (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 company_id bigint NOT NULL,
 person_id bigint CHECK (person_id IS NULL OR person_id > 0),
 address text CHECK (address IS NULL OR octet_length(address) BETWEEN 1 AND 254),
 channel text NOT NULL CHECK (channel IN ('email','sms','messaging','voice','chat')),
 reason text NOT NULL CHECK (reason IN ('hard_bounce','complaint','unsubscribe')),
 evidence text CHECK (evidence IS NULL OR length(evidence) BETWEEN 1 AND 2000),
 suppressed_at timestamptz NOT NULL DEFAULT now(),
 lifted_at timestamptz,
 lifted_by bigint,
 lifted_principal text CHECK (lifted_principal IS NULL OR octet_length(lifted_principal) BETWEEN 1 AND 200),
 lift_reason text CHECK (lift_reason IS NULL OR length(lift_reason) BETWEEN 1 AND 500),
 UNIQUE (tenant_id, id),
 FOREIGN KEY (tenant_id, company_id) REFERENCES companies(tenant_id, id),
 FOREIGN KEY (tenant_id, lifted_by) REFERENCES users(tenant_id, id),
 CHECK (num_nonnulls(person_id, address) = 1),
 CHECK ((lifted_at IS NULL) = (lift_reason IS NULL)),
 CHECK ((lifted_at IS NULL AND lifted_by IS NULL AND lifted_principal IS NULL) OR (lifted_at IS NOT NULL AND num_nonnulls(lifted_by, lifted_principal) = 1))
);
CREATE UNIQUE INDEX core_consent_suppressions_person_idx ON core_consent_suppressions(tenant_id, company_id, channel, person_id, reason) WHERE person_id IS NOT NULL AND lifted_at IS NULL;
CREATE UNIQUE INDEX core_consent_suppressions_address_idx ON core_consent_suppressions(tenant_id, company_id, channel, address, reason) WHERE address IS NOT NULL AND lifted_at IS NULL;
CREATE INDEX core_consent_suppressions_list_idx ON core_consent_suppressions(tenant_id, company_id, suppressed_at DESC, id DESC);
