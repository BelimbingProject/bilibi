-- Bounded historical reads by one tenant and event kind, newest first.
CREATE INDEX base_events_name_recorded ON base_events (tenant_id, name, recorded_at DESC, id DESC) WHERE erased_at IS NULL;
