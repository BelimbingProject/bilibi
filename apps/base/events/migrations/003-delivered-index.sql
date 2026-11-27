-- The Events page reads each subscriber's latest receipt with one
-- `ORDER BY event_id DESC LIMIT 1`. The pending index covers only undelivered
-- rows, so without this one that read sorted every delivered row.
CREATE INDEX base_events_delivered ON base_events_deliveries (subscriber, event_id DESC) WHERE delivered_at IS NOT NULL;
