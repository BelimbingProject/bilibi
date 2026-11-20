-- Bounded tenant/kind traversal for receipt history and freshness checks.
CREATE INDEX base_audit_mutations_tenant_kind_id_index
    ON base_audit_mutations (tenant_id, auditable_type, id DESC);
