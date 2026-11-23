-- Add manual contact channels to decisions, suppressions and dispatch checks.
ALTER TABLE core_consent_decisions DROP CONSTRAINT core_consent_decisions_channel_check,
    ADD CHECK (channel IN ('email','sms','messaging','voice','chat','postal','in_person','any')),
    DROP CONSTRAINT core_consent_decisions_captured_on_check,
    ADD CHECK (captured_on IN ('email','sms','messaging','voice','chat','postal','web','in_person'));
ALTER TABLE core_consent_current DROP CONSTRAINT core_consent_current_channel_check,
    ADD CHECK (channel IN ('email','sms','messaging','voice','chat','postal','in_person','any')),
    DROP CONSTRAINT core_consent_current_captured_on_check,
    ADD CHECK (captured_on IN ('email','sms','messaging','voice','chat','postal','web','in_person'));
ALTER TABLE core_consent_suppressions DROP CONSTRAINT core_consent_suppressions_channel_check,
    ADD CHECK (channel IN ('email','sms','messaging','voice','chat','postal','in_person'));
ALTER TABLE core_consent_log DROP CONSTRAINT core_consent_log_channel_check,
    ADD CHECK (channel IN ('email','sms','messaging','voice','chat','postal','in_person'));
