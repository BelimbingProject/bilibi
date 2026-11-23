-- The partner address a case is shared with, and the outbox of what was sent
-- there: the reference outbound action records each send it would deliver.
ALTER TABLE watcher_cases_cases ADD COLUMN partner_address text;
CREATE TABLE watcher_cases_notices(
  id bigserial PRIMARY KEY,
  tenant_id bigint NOT NULL,
  case_id bigint NOT NULL,
  destination text NOT NULL,
  payload jsonb NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  FOREIGN KEY (tenant_id, case_id) REFERENCES watcher_cases_cases (tenant_id, id)
);
