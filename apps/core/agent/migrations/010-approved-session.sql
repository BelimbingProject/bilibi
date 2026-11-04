-- An authorization-code approval remembers the exact browser session that
-- explicitly approved it, so redemption creates the connection switched on
-- for that session. Session IDs are opaque holders and intentionally have no
-- foreign key after migration 007.
ALTER TABLE core_agent_authorization_codes
    ADD COLUMN enabled_session_id text;
