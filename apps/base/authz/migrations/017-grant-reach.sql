-- Record reach (#683): how far a role's grant of a capability reaches into the
-- records it guards. `own`: records the account owns; `team`: also records
-- owned by someone who shares a Core team with it, or filed under one of its
-- teams; `all`: every record the capability opens. Only a capability whose
-- owner opted in (`CapabilityDefinition::with_reach`) may carry less than
-- `all`; Authz refuses the rest. Direct allows, grant-all and system
-- principals always reach all.
ALTER TABLE role_capabilities ADD COLUMN reach text NOT NULL DEFAULT 'all' CHECK (reach IN ('own','team','all'));
