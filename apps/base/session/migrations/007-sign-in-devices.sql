-- The device label (browser family and operating system) judged once at
-- sign-in, so a sign-in from a device the account's history has not seen can
-- be told apart. Bilibi has no production users: earlier history rows carry no
-- label and are dropped rather than converted.
DELETE FROM session_sign_ins;
ALTER TABLE session_sign_ins ADD COLUMN device text NOT NULL;
CREATE INDEX session_sign_ins_device_idx ON session_sign_ins(tenant_id, user_id, device);

-- One claim per account, device and UTC day for the new-device sign-in email:
-- the unique key is the once-a-day limit. The audit row records each email;
-- the expiry sweep removes claims older than yesterday.
CREATE TABLE session_sign_in_alerts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id bigint NOT NULL,
    user_id bigint NOT NULL,
    device text NOT NULL,
    alerted_on date NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, user_id, device, alerted_on)
);
CREATE INDEX session_sign_in_alerts_day_idx ON session_sign_in_alerts(alerted_on);
