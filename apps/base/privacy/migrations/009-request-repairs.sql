-- A request's store rows are the stores mounted when it ran (#561). A row
-- whose store's module is no longer mounted is answered `unmounted` (finished,
-- nothing read or erased), so a deploy that removes a store never strands a
-- running or failed request.
ALTER TABLE base_privacy_request_stores DROP CONSTRAINT base_privacy_request_stores_state_check;
ALTER TABLE base_privacy_request_stores ADD CONSTRAINT base_privacy_request_stores_state_check
    CHECK (state IN ('pending','answered','deferred','unmounted'));

-- Every store erasure an owed cleanup commits, counted in the same
-- transaction: the person's erasure generation includes it, so an access
-- read that spans a cleanup is read again.
ALTER TABLE base_privacy_resumptions ADD COLUMN erasures integer NOT NULL DEFAULT 0 CHECK (erasures >= 0);

CREATE OR REPLACE FUNCTION base_privacy_resumptions_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'UPDATE' AND OLD.done_at IS NULL
     AND (NEW.id, NEW.tenant_id, NEW.request_id, NEW.person_id, NEW.created_at)
         IS NOT DISTINCT FROM (OLD.id, OLD.tenant_id, OLD.request_id, OLD.person_id, OLD.created_at)
     AND NEW.runs >= OLD.runs AND NEW.erasures >= OLD.erasures THEN
    RETURN NEW;
  END IF;
  RAISE EXCEPTION 'owed erasure cleanup only records its runs, and is never removed' USING ERRCODE = '42501';
END;
$$;
