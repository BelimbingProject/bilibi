-- The scheduled cleanup deletes device receipts by age in bounded batches;
-- without this index every batch would read the whole table.
CREATE INDEX core_agent_devices_expires_idx ON core_agent_devices(expires_at);
