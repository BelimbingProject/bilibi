-- A review the agent replaced with a newer one ends as `replaced`.
ALTER TABLE base_agent_api_reviews DROP CONSTRAINT base_agent_api_reviews_status_check;
ALTER TABLE base_agent_api_reviews ADD CONSTRAINT base_agent_api_reviews_status_check CHECK (status IN ('open','saved','edited_and_saved','cancelled','refused','uncertain','replaced'));
