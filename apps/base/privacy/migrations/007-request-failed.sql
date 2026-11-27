-- A running request whose background job ended without finishing (its
-- approver lost the right to run it, or a store or the finish step ran out of
-- attempts) moves to `failed`, recorded by the `privacy-monitor` principal
-- through Queue's ending hook, instead of staying `running` for ever. A
-- person who may approve runs it again (`failed -> running`).
ALTER TABLE base_privacy_requests
    DROP CONSTRAINT base_privacy_requests_status_check,
    ADD CONSTRAINT base_privacy_requests_status_check
        CHECK (status IN ('received','verified','approved','running','failed','completed','refused')),
    -- When it failed and why, as a short code: never an error message.
    ADD COLUMN failed_at timestamptz,
    ADD COLUMN failure text CHECK (failure ~ '^[a-z][a-z0-9_]{0,63}$'),
    -- How many times the request has started running. A person's erasure
    -- generation is the sum over their erasures, so running a failed erasure
    -- again invalidates exports read before it, as its first run did.
    ADD COLUMN runs integer NOT NULL DEFAULT 0 CHECK (runs >= 0),
    ADD CHECK ((status = 'failed') = (failed_at IS NOT NULL AND failure IS NOT NULL)),
    ADD CHECK (status <> 'failed' OR approved_by IS NOT NULL);
