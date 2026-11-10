-- The Queue job each request step currently waits on, so an ending told late
-- for a job of an earlier run changes nothing. Set when the job is queued;
-- never read for anything else.
ALTER TABLE base_privacy_request_stores ADD COLUMN job_id bigint;
ALTER TABLE base_privacy_requests ADD COLUMN finish_job_id bigint;
