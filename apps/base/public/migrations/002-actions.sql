-- Public actions (ADR 34, slice 6): what base/public proves before a POST
-- from the site's own pages reaches its owner. Each site signs its pages'
-- one-time tickets with its own key. A receipt keeps a ticket's digest and
-- its outcome, never a typed value; abuse counts are per subject and hour.
ALTER TABLE public_sites
 ADD COLUMN ticket_key bytea NOT NULL DEFAULT (uuid_send(gen_random_uuid()) || uuid_send(gen_random_uuid()));

CREATE TABLE public_receipts (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 action text NOT NULL CHECK (length(action) BETWEEN 1 AND 200),
 subject text NOT NULL CHECK (length(subject) BETWEEN 1 AND 100),
 ticket_hash bytea NOT NULL CHECK (octet_length(ticket_hash) = 32),
 receipt_hash bytea NOT NULL CHECK (octet_length(receipt_hash) = 32),
 outcome text NOT NULL CHECK (outcome IN ('pending', 'accepted', 'refused', 'spam')),
 reason text CHECK (reason IS NULL OR length(reason) <= 40),
 message text CHECK (message IS NULL OR length(message) <= 500),
 created_at timestamptz NOT NULL DEFAULT now(),
 settled_at timestamptz
);
CREATE UNIQUE INDEX public_receipts_ticket_idx ON public_receipts(tenant_id, action, ticket_hash);
CREATE INDEX public_receipts_receipt_idx ON public_receipts(tenant_id, action, receipt_hash);
CREATE INDEX public_receipts_places_idx ON public_receipts(tenant_id, action, subject, created_at) WHERE outcome <> 'spam';
CREATE INDEX public_receipts_age_idx ON public_receipts(tenant_id, created_at);

CREATE TABLE public_abuse_counts (
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 action text NOT NULL,
 subject text NOT NULL,
 hour timestamptz NOT NULL,
 trap integer NOT NULL DEFAULT 0 CHECK (trap >= 0),
 too_fast integer NOT NULL DEFAULT 0 CHECK (too_fast >= 0),
 busy integer NOT NULL DEFAULT 0 CHECK (busy >= 0),
 expired integer NOT NULL DEFAULT 0 CHECK (expired >= 0),
 PRIMARY KEY (tenant_id, action, subject, hour)
);
