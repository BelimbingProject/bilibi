-- Retention reads one tenant's events oldest first and stops at the keep
-- time; no other index orders a tenant's events by when they were recorded.
CREATE INDEX base_events_recorded ON base_events (tenant_id, recorded_at, id);
-- One row per tenant and subscriber: the subscriber has a delivery for every
-- event it reads up to `event_id` (or the event was erased, or is gone). A
-- delivery moves it forward and it stops before the first missed event, so
-- the missed count starts here instead of at the tenant's first event.
CREATE TABLE base_events_watermarks (
 tenant_id bigint NOT NULL CHECK (tenant_id > 0),
 subscriber varchar(200) NOT NULL,
 event_id bigint NOT NULL CHECK (event_id >= 0),
 PRIMARY KEY (tenant_id, subscriber)
);
