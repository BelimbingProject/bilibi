-- Append-only record of each successful browser sign-in, so the previous
-- sign-in survives sign-out, expiry and "sign out everywhere else". Rows are
-- tenant- and user-scoped and are removed by the expiry sweep once old.
CREATE TABLE session_sign_ins (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id bigint NOT NULL,
    user_id bigint NOT NULL,
    signed_in_at timestamptz NOT NULL,
    ip_address text,
    user_agent text
);
CREATE INDEX session_sign_ins_account_idx
    ON session_sign_ins(tenant_id, user_id, signed_in_at DESC, id DESC);
CREATE INDEX session_sign_ins_age_idx ON session_sign_ins(signed_in_at);
