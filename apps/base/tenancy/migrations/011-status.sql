-- A tenant is active or suspended, nothing else. A suspension always says when
-- and why (the reason is also in the audit trail); an active tenant carries
-- neither. Creation is always active; suspend and reactivate are the only moves
-- (`TenantStatus::after`).
ALTER TABLE tenants
 ADD COLUMN suspended_at timestamptz,
 ADD COLUMN suspension_reason text,
 ADD CONSTRAINT tenants_status_check CHECK (status IN ('active', 'suspended')),
 ADD CONSTRAINT tenants_suspension_check CHECK (
  (status = 'suspended') = (suspended_at IS NOT NULL AND suspension_reason IS NOT NULL)
  AND (status = 'suspended' OR (suspended_at IS NULL AND suspension_reason IS NULL))
  AND (suspension_reason IS NULL OR (btrim(suspension_reason) <> '' AND char_length(suspension_reason) <= 500))
 );
