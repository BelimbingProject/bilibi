-- The searches the approval reads and the maintenance sweep make most often.
CREATE INDEX base_workflow_approval_requests_run ON base_workflow_approval_requests(tenant_id,run_id);
CREATE INDEX base_workflow_approval_events_request ON base_workflow_approval_events(tenant_id,request_id,id);
CREATE INDEX base_workflow_approval_delegations_delegate ON base_workflow_approval_delegations(tenant_id,company_id,case_key,delegate);
-- The process sweep visits a bounded number of running runs per pass and
-- continues after the last run of the previous pass. One row for the
-- installation: runs of every tenant share one id order.
CREATE INDEX base_workflow_process_runs_running ON base_workflow_process_runs(id) WHERE status='running';
CREATE TABLE base_workflow_process_maintenance (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    last_run bigint NOT NULL DEFAULT 0 CHECK (last_run >= 0)
);
INSERT INTO base_workflow_process_maintenance(singleton) VALUES(true);
