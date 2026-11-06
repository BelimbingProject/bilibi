-- The active-session list and account security summary need the successful
-- admission time. It has the same lifetime as the session row itself.
ALTER TABLE sessions ADD COLUMN signed_in_at timestamptz NOT NULL DEFAULT now();
CREATE INDEX sessions_account_signed_in_idx
    ON sessions(tenant_id, user_id, signed_in_at DESC, id);
