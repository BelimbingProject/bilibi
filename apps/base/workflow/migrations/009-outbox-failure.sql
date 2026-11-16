ALTER TABLE base_workflow_transition_outbox ADD COLUMN failed_at timestamptz;
ALTER TABLE base_workflow_transition_outbox ADD CHECK (failed_at IS NULL OR (delivered_at IS NULL AND lease_token IS NULL));
DROP INDEX base_workflow_outbox_due;
CREATE INDEX base_workflow_outbox_due ON base_workflow_transition_outbox(available_at,id) WHERE delivered_at IS NULL AND failed_at IS NULL;
