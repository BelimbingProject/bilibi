-- Consent decisions and the current state per key: `core/consent`. A decision
-- is about a Core person (no foreign key crosses to Core Person) or an
-- address alone, for one purpose (a `core.consent.purposes` code) and one
-- channel. Decisions are append-only: a change is the next version.
CREATE TABLE core_consent_decisions (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 company_id bigint NOT NULL,
 person_id bigint CHECK (person_id IS NULL OR person_id > 0),
 address text CHECK (address IS NULL OR octet_length(address) BETWEEN 1 AND 254),
 purpose text NOT NULL CHECK (octet_length(purpose) BETWEEN 1 AND 64),
 channel text NOT NULL CHECK (channel IN ('email','sms','messaging','voice','chat','any')),
 granted boolean NOT NULL,
 source text NOT NULL CHECK (source IN ('portal','preference_link','agent','import','inbound_keyword','api')),
 captured_on text NOT NULL CHECK (captured_on IN ('email','sms','messaging','voice','chat','web','in_person')),
 evidence text CHECK (evidence IS NULL OR length(evidence) BETWEEN 1 AND 2000),
 version integer NOT NULL CHECK (version > 0),
 recorded_at timestamptz NOT NULL DEFAULT now(),
 recorded_by bigint,
 recorded_principal text CHECK (recorded_principal IS NULL OR octet_length(recorded_principal) BETWEEN 1 AND 200),
 UNIQUE (tenant_id, id),
 FOREIGN KEY (tenant_id, company_id) REFERENCES companies(tenant_id, id),
 FOREIGN KEY (tenant_id, recorded_by) REFERENCES users(tenant_id, id),
 CHECK (num_nonnulls(person_id, address) = 1),
 CHECK (num_nonnulls(recorded_by, recorded_principal) = 1)
);
CREATE UNIQUE INDEX core_consent_decisions_person_idx ON core_consent_decisions(tenant_id, company_id, person_id, purpose, channel, version) WHERE person_id IS NOT NULL;
CREATE UNIQUE INDEX core_consent_decisions_address_idx ON core_consent_decisions(tenant_id, company_id, address, purpose, channel, version) WHERE address IS NOT NULL;

-- The latest version per key, written in the decision's transaction, so a
-- check reads one row.
CREATE TABLE core_consent_current (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 company_id bigint NOT NULL,
 person_id bigint CHECK (person_id IS NULL OR person_id > 0),
 address text CHECK (address IS NULL OR octet_length(address) BETWEEN 1 AND 254),
 purpose text NOT NULL CHECK (octet_length(purpose) BETWEEN 1 AND 64),
 channel text NOT NULL CHECK (channel IN ('email','sms','messaging','voice','chat','any')),
 granted boolean NOT NULL,
 source text NOT NULL CHECK (source IN ('portal','preference_link','agent','import','inbound_keyword','api')),
 captured_on text NOT NULL CHECK (captured_on IN ('email','sms','messaging','voice','chat','web','in_person')),
 version integer NOT NULL CHECK (version > 0),
 recorded_at timestamptz NOT NULL,
 UNIQUE (tenant_id, id),
 FOREIGN KEY (tenant_id, company_id) REFERENCES companies(tenant_id, id),
 CHECK (num_nonnulls(person_id, address) = 1)
);
CREATE UNIQUE INDEX core_consent_current_person_idx ON core_consent_current(tenant_id, company_id, person_id, purpose, channel) WHERE person_id IS NOT NULL;
CREATE UNIQUE INDEX core_consent_current_address_idx ON core_consent_current(tenant_id, company_id, address, purpose, channel) WHERE address IS NOT NULL;
