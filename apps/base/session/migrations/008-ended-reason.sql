-- A session ended by a transaction that cannot record the account's
-- session-ended notice itself (an operator in another tenant, or an
-- impersonated write, which may not enqueue work) stays behind as a row that
-- no longer authenticates (`expires_at` is set to the ending time) and names
-- why it ended. The expiry sweep deletes it and records the notice in the
-- session's own tenant. A row without a reason that the sweep removes expired.
ALTER TABLE sessions ADD COLUMN ended_reason text
 CHECK (ended_reason IN ('signed_out', 'terminated'));
