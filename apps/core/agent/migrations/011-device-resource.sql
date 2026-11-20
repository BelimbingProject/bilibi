-- A device request may name the resource its tokens are for (RFC 8707), so a
-- cloud app with no browser callback can reach `/mcp`. NULL keeps the
-- `/api/v1` audience. Approval copies it to the connection.
ALTER TABLE core_agent_devices
    ADD COLUMN resource text;
