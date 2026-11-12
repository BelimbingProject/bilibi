-- System Info's queue line counts waiting, discarded and unrecorded jobs on
-- every page view. The waiting counts read `base_queue_claim`; these two give
-- the other counts an index as small as what they count, so the line never
-- reads the finished jobs.
CREATE INDEX base_queue_jobs_discarded ON base_queue_jobs (id) WHERE state = 'discarded';
CREATE INDEX base_queue_jobs_ending_failed ON base_queue_jobs (id) WHERE ending = 'failed';
