-- The sweep looks for tenants with open deliveries (pending or queued) every
-- 15 minutes; finished rows are pruned, so this index stays small.
CREATE INDEX base_report_deliveries_open_idx ON base_report_deliveries (tenant_id, id) WHERE state IN ('pending','queued');

CREATE INDEX base_report_deliveries_finished_idx ON base_report_deliveries (finished_at, tenant_id, id) WHERE state IN ('sent','skipped','cancelled');

ALTER TABLE base_report_deliveries ADD COLUMN mailed_at timestamptz;
