-- The sweep looks for tenants with open deliveries (pending or queued) every
-- 15 minutes; finished rows are pruned, so this index stays small.
CREATE INDEX base_report_deliveries_open_idx ON base_report_deliveries (tenant_id, id) WHERE state IN ('pending','queued');
