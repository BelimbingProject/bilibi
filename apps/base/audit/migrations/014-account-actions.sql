-- Privacy reads follow account identities across every recording month.
CREATE INDEX base_audit_actions_account ON base_audit_actions(tenant_id, actor_id, id)
 WHERE actor_type IN ('user', 'agent');
CREATE INDEX base_audit_actions_account_impersonator ON base_audit_actions(tenant_id, impersonator_id, id)
 WHERE impersonator_id IS NOT NULL;
