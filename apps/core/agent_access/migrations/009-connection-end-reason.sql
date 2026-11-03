ALTER TABLE core_agent_connections
 ADD COLUMN end_reason text,
 ADD CONSTRAINT core_agent_connections_end_reason_check CHECK (
  end_reason IS NULL OR end_reason IN (
   'password_reset',
   'password_changed',
   'signed_out',
   'turned_off',
   'replaced',
   'expired',
   'admin_action',
   'security_event'
  )
 );
