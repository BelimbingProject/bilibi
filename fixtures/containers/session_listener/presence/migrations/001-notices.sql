-- The presence Domain's own record of each session-ended notice it received,
-- keyed by the event key, and what Session said when it handled it.
CREATE TABLE session_listener_notices(
  tenant_id bigint NOT NULL,
  account bigint NOT NULL,
  reason text NOT NULL,
  sessions bigint NOT NULL,
  still_signed_in boolean NOT NULL,
  signed_in_now boolean NOT NULL,
  event_key text NOT NULL UNIQUE,
  payload jsonb NOT NULL,
  received_at timestamptz NOT NULL DEFAULT now()
);
