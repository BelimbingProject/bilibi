-- The system principal that recorded an event of a producer-bound type
-- (ADR 27), checked against the tenant's grant when it was recorded. Null for
-- every other event type. It is part of the idempotency comparison: the same
-- key with a different producer is a conflict.
ALTER TABLE base_events ADD COLUMN producer varchar(200) CHECK (producer <> '');
