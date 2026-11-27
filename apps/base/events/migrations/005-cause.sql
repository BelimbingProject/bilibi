-- The cause a Platform edge named for the transaction that recorded the event
-- (`Events::cause`), such as the rule run whose action recorded it. NULL when
-- the event was recorded by an ordinary write.
ALTER TABLE base_events ADD COLUMN cause varchar(200);
