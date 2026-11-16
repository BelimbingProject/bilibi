-- One tenant-wide legal minimum for audit evidence (#601). No ledger row ever
-- named a company: the owning company is a property of the record, which the
-- mutation boundary does not know, and the session's company is not it. So
-- the per-company branch never applied; every row already followed the
-- strictest minimum, and action retention the primary company. The column,
-- its indexes and the per-company parameters go; behaviour is unchanged.
DROP FUNCTION base_audit_erase_deferred(bigint, bigint, bigint[], bigint[], integer[], integer, text, bigint, text);
DROP FUNCTION base_audit_erase_values(bigint, bigint, bigint, bigint, text, text, text[], boolean, bigint[], integer[], integer, text, bigint, text);
DROP FUNCTION base_audit_erase_actions(bigint, bigint, timestamptz, bigint, integer, bigint, boolean, bigint[], text, bigint, text);

-- The evidence guard of migration 012, without company_id.
CREATE OR REPLACE FUNCTION base_audit_evidence_v1() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
  ledger text := coalesce(TG_ARGV[0], TG_TABLE_NAME);
  moving boolean := false;
BEGIN
  IF ledger IN ('base_audit_mutations', 'base_audit_actions') THEN
    IF TG_OP = 'UPDATE' THEN moving := NEW.partition_month IS DISTINCT FROM OLD.partition_month;
    ELSIF TG_OP = 'DELETE' THEN moving := true;
    ELSIF TG_OP = 'INSERT' THEN moving := NEW.chain_seq IS NOT NULL;
    END IF;
    IF moving THEN
    -- Row movement is PostgreSQL's UPDATE, DELETE, INSERT sequence. Only
    -- the fixed removal function can open this path; it moves retained rows
    -- to infinity, never changing their evidence or chain links.
    IF base_audit_within('base_audit_remove_partition', TG_RELID) THEN
      IF TG_OP = 'UPDATE' THEN
        IF NEW.partition_month = 'infinity'::date AND isfinite(OLD.partition_month) THEN
          OLD.partition_month := NEW.partition_month;
          RETURN OLD;
        END IF;
      ELSIF TG_OP = 'DELETE' THEN
        RETURN OLD;
      ELSIF TG_OP = 'INSERT' AND NEW.partition_month = 'infinity'::date AND NEW.chain_seq IS NOT NULL THEN
        RETURN NEW;
      END IF;
    END IF;
    -- Releasing a preserved bookmark relocates, but never removes, the
    -- evidence. Its values and original recording time remain unchanged;
    -- the current month gives it a fresh minimum storage period.
    IF base_audit_within('base_audit_toggle_retained', TG_RELID) THEN
      IF TG_OP = 'UPDATE' THEN
        IF OLD.partition_month = 'infinity'::date
           AND NEW.partition_month = date_trunc('month', clock_timestamp() AT TIME ZONE 'UTC')::date THEN
          OLD.partition_month := NEW.partition_month;
          RETURN OLD;
        END IF;
      ELSIF TG_OP = 'DELETE' THEN
        RETURN OLD;
      ELSIF TG_OP = 'INSERT' AND NEW.chain_seq IS NOT NULL THEN
        RETURN NEW;
      END IF;
    END IF;
    END IF;
    IF TG_OP = 'INSERT' THEN
      IF NEW.partition_month <> date_trunc('month', clock_timestamp() AT TIME ZONE 'UTC')::date
         OR NEW.chain_seq IS NOT NULL OR NEW.chain_hash IS NOT NULL THEN
        RAISE EXCEPTION 'new audit evidence belongs to the current recording month and starts unlinked' USING ERRCODE='42501';
      END IF;
    END IF;
  END IF;
  -- Nested throughout: PL/pgSQL may evaluate every operand, and each table
  -- has only some of these columns.
  IF TG_OP = 'UPDATE' AND ledger IN ('base_audit_mutations', 'base_audit_actions') THEN
    -- The chain link is by far the commonest update, so it is decided first
    -- and from the columns before the call stack is read. The row that is
    -- stored is the old one with only its link set: nothing else can change.
    IF NEW.chain_seq IS NOT NULL AND NEW.chain_hash IS NOT NULL THEN
      IF OLD.chain_seq IS NULL AND base_audit_within('base_audit_chain_commit', TG_RELID) THEN
        OLD.chain_seq := NEW.chain_seq;
        OLD.chain_hash := NEW.chain_hash;
        RETURN OLD;
      END IF;
    END IF;
  END IF;
  IF TG_OP = 'UPDATE' AND ledger = 'base_audit_mutations' THEN
    IF base_audit_within('base_audit_erase_values', TG_RELID)
       AND (NEW.id, NEW.tenant_id, NEW.actor_type, NEW.actor_id, NEW.actor_role,
            NEW.impersonator_id, NEW.system_principal, NEW.ip_address, NEW.url, NEW.user_agent,
            NEW.auditable_type, NEW.auditable_id, NEW.subject_name, NEW.subject_id, NEW.source,
            NEW.event, NEW.trace_id, NEW.occurred_at, NEW.recorded_at,
            NEW.agent_self_report, NEW.client_identity, NEW.partition_month, NEW.is_retained)
           IS NOT DISTINCT FROM
           (OLD.id, OLD.tenant_id, OLD.actor_type, OLD.actor_id, OLD.actor_role,
            OLD.impersonator_id, OLD.system_principal, OLD.ip_address, OLD.url, OLD.user_agent,
            OLD.auditable_type, OLD.auditable_id, OLD.subject_name, OLD.subject_id, OLD.source,
            OLD.event, OLD.trace_id, OLD.occurred_at, OLD.recorded_at,
            OLD.agent_self_report, OLD.client_identity, OLD.partition_month, OLD.is_retained)
       AND (NEW.chain_seq, NEW.chain_hash) IS NOT DISTINCT FROM (OLD.chain_seq, OLD.chain_hash) THEN
      RETURN NEW;
    END IF;
  END IF;
  IF TG_OP = 'UPDATE' AND ledger = 'base_audit_actions' THEN
    -- Retention: the guard itself clears the row, so the erasing function can
    -- remove nothing else. Who, when, which event and which payload keys stay
    -- (they are the chain's facts); the address, URL, browser and payload
    -- values go.
    IF OLD.erased_at IS NULL AND NEW.erased_at IS NOT NULL THEN
      IF base_audit_within('base_audit_erase_actions', TG_RELID) THEN
        OLD.ip_address := NULL;
        OLD.url := NULL;
        OLD.user_agent := NULL;
        OLD.payload := base_audit_erase_json(OLD.payload, ARRAY['*']);
        OLD.erased_at := NEW.erased_at;
        RETURN OLD;
      END IF;
    END IF;
  END IF;
  IF ledger = 'base_audit_chain_heads' THEN
    IF TG_OP = 'INSERT' THEN
      -- A chain begins empty, as its tenant's first evidence commits.
      IF NEW.seq = 0 AND NEW.hash = ''::bytea AND base_audit_within('base_audit_chain_commit', TG_RELID) THEN
        RETURN NEW;
      END IF;
    ELSIF TG_OP = 'UPDATE' THEN
      IF (NEW.chain, NEW.tenant_id, NEW.born) IS NOT DISTINCT FROM (OLD.chain, OLD.tenant_id, OLD.born)
         AND NEW.seq > OLD.seq AND base_audit_within('base_audit_chain_commit', TG_RELID) THEN
        RETURN NEW;
      END IF;
    END IF;
    RAISE EXCEPTION 'audit chain heads move only as evidence commits' USING ERRCODE = '42501';
  END IF;
  IF ledger = 'base_audit_erasures' THEN
    IF TG_OP <> 'INSERT' OR NOT base_audit_within('base_audit_erase_values', TG_RELID) THEN
      RAISE EXCEPTION 'the audit erasure ledger is written only by base_audit_erase_values'
        USING ERRCODE = '42501';
    END IF;
  END IF;
  IF ledger = 'base_audit_action_erasures' THEN
    IF TG_OP <> 'INSERT' OR NOT base_audit_within('base_audit_erase_actions', TG_RELID) THEN
      RAISE EXCEPTION 'the action erasure ledger is written only by base_audit_erase_actions'
        USING ERRCODE = '42501';
    END IF;
  END IF;
  IF TG_OP <> 'INSERT' THEN
    RAISE EXCEPTION 'audit evidence is append-only' USING ERRCODE = '42501';
  END IF;
  NEW.recorded_at := clock_timestamp();
  -- Nested: PL/pgSQL may evaluate both operands, and diagnostics have no event time.
  IF ledger <> 'base_audit_capture_failures' THEN
    IF NEW.occurred_at IS NULL THEN
      NEW.occurred_at := date_trunc('second', NEW.recorded_at);
    END IF;
  END IF;
  RETURN NEW;
END;
$$;

ALTER TABLE base_audit_mutations DROP COLUMN company_id;
ALTER TABLE base_audit_actions DROP COLUMN company_id;
ALTER TABLE base_audit_capture_failures DROP COLUMN company_id;

-- Erases one record's evidence for one cause. Refused unless the record
-- kind's row in base_personal_data_kinds declares every field passed (and the
-- subject identifier, when asked), a record that is a person is p_person
-- itself, the cause's registered checker confirms it in the tenant for
-- p_person (a request names its own person), and the target is bound to the
-- cause: the calling transaction itself wrote the record's
-- change (the store's erasure, captured in the same transaction), or the
-- retention sweep is finishing that record's own due deferral. A row stays
-- while newer than p_keep_days, the tenant's legal minimum (none when NULL).
-- Kept rows get one open deferral naming p_person, which the
-- sweep erases once due. The erasure's own rows (its trace) are never kept,
-- since no erasure trail may hold the values it erased. Returns the rows
-- erased, the rows kept and until when.
CREATE FUNCTION base_audit_erase_values(
  p_tenant bigint, p_request bigint, p_sweep bigint, p_person bigint, p_type text, p_id text, p_fields text[],
  p_subject_identifier boolean, p_keep_days integer,
  p_actor_type text, p_actor_id bigint, p_trace text,
  OUT erased integer, OUT kept integer, OUT kept_until timestamptz
) LANGUAGE plpgsql AS $$
DECLARE
  declared record;
  checker regprocedure;
  confirmed boolean;
  kept_ids bigint[];
BEGIN
  IF p_tenant IS NULL OR p_tenant <= 0 OR (p_request IS NULL) = (p_sweep IS NULL)
     OR (p_person IS NOT NULL AND p_person <= 0)
     OR p_type IS NULL OR p_id IS NULL OR p_fields IS NULL OR cardinality(p_fields) > 64
     OR p_subject_identifier IS NULL
     OR (p_keep_days IS NOT NULL AND p_keep_days NOT BETWEEN 1 AND 36500) THEN
    RAISE EXCEPTION 'invalid audit erasure' USING ERRCODE = '22023';
  END IF;
  SELECT fields, subject_identifier, person INTO declared FROM base_personal_data_kinds WHERE kind = p_type;
  IF NOT FOUND OR NOT (p_fields <@ declared.fields)
     OR (p_subject_identifier AND NOT declared.subject_identifier) THEN
    RAISE EXCEPTION 'audit erasure of fields its record kind does not declare' USING ERRCODE = '42501';
  END IF;
  -- A record that is a person is only ever its own person's: a request for
  -- one person never clears another person's record, even one the same
  -- transaction wrote.
  IF declared.person AND p_person IS NOT NULL AND p_id <> p_person::text THEN
    RAISE EXCEPTION 'audit erasure of another person' USING ERRCODE = '42501';
  END IF;
  SELECT c.checker INTO checker FROM base_audit_erasure_causes c
   WHERE c.cause = CASE WHEN p_request IS NOT NULL THEN 'request' ELSE 'sweep' END;
  IF checker IS NULL THEN
    RAISE EXCEPTION 'audit erasure cause has no checker' USING ERRCODE = '42501';
  END IF;
  IF p_request IS NOT NULL AND p_person IS NULL THEN
    RAISE EXCEPTION 'a request erasure names its person' USING ERRCODE = '22023';
  END IF;
  EXECUTE format('SELECT %s($1, $2, $3)', checker::regproc) INTO confirmed
    USING p_tenant, coalesce(p_request, p_sweep), p_person;
  IF confirmed IS NOT TRUE THEN
    RAISE EXCEPTION 'audit erasure cause is not open in this tenant for this person' USING ERRCODE = '42501';
  END IF;
  IF NOT base_audit_within('base_audit_erase_deferred', 'base_audit_erasure_deferrals'::regclass)
     AND NOT EXISTS (SELECT 1 FROM base_audit_mutations
                      WHERE tenant_id = p_tenant AND auditable_type = p_type AND auditable_id = p_id
                        AND base_audit_written_here(xmin)) THEN
    RAISE EXCEPTION 'audit erasure of a record this transaction did not change' USING ERRCODE = '42501';
  END IF;
  -- Each row's legal end: its time plus the tenant's minimum, if any.
  SELECT coalesce(array_agg(m.id), '{}'), count(*), max(m.occurred_at + make_interval(days => p_keep_days))
    INTO kept_ids, kept, kept_until
    FROM base_audit_mutations m
   WHERE m.tenant_id = p_tenant AND m.auditable_type = p_type AND m.auditable_id = p_id
     AND p_keep_days IS NOT NULL AND m.trace_id IS DISTINCT FROM p_trace
     AND m.occurred_at + make_interval(days => p_keep_days) > clock_timestamp();
  UPDATE base_audit_mutations m
     SET old_values = base_audit_erase_json(old_values, p_fields),
         new_values = base_audit_erase_json(new_values, p_fields),
         subject_identifier = CASE WHEN p_subject_identifier AND subject_identifier IS NOT NULL
                                   THEN '[erased]' ELSE subject_identifier END
   WHERE m.tenant_id = p_tenant AND m.auditable_type = p_type AND m.auditable_id = p_id
     AND NOT (m.id = ANY(kept_ids));
  GET DIAGNOSTICS erased = ROW_COUNT;
  IF kept > 0 THEN
    INSERT INTO base_audit_erasure_deferrals(tenant_id, person_id, auditable_type, auditable_id, fields, subject_identifier, due_at)
      VALUES (p_tenant, p_person, p_type, p_id, p_fields, p_subject_identifier, kept_until)
      ON CONFLICT (tenant_id, auditable_type, auditable_id) WHERE done_at IS NULL
      DO UPDATE SET due_at = greatest(base_audit_erasure_deferrals.due_at, EXCLUDED.due_at),
                    fields = ARRAY(SELECT DISTINCT unnest(base_audit_erasure_deferrals.fields || EXCLUDED.fields) ORDER BY 1),
                    subject_identifier = base_audit_erasure_deferrals.subject_identifier OR EXCLUDED.subject_identifier;
  ELSE
    -- Nothing left to wait for: an open deferral for this record is done.
    UPDATE base_audit_erasure_deferrals SET done_at = clock_timestamp()
     WHERE tenant_id = p_tenant AND auditable_type = p_type AND auditable_id = p_id AND done_at IS NULL;
  END IF;
  INSERT INTO base_audit_erasures(tenant_id, request_id, sweep_id, auditable_type, auditable_id, fields,
      mutation_rows, kept_rows, kept_until, actor_type, actor_id, trace_id)
    VALUES (p_tenant, p_request, p_sweep, p_type, p_id, p_fields, erased, kept, kept_until,
      p_actor_type, p_actor_id, p_trace);
END;
$$;

-- The retention sweep's half: erases the open, due deferrals named in p_ids
-- (the caller has already left out held records and persons, under the hold
-- guard), with the current legal minimum, so rows written since stay and
-- reopen a deferral. Returns how many deferrals it closed.
CREATE FUNCTION base_audit_erase_deferred(
  p_tenant bigint, p_sweep bigint, p_ids bigint[],
  p_keep_days integer, p_actor_type text, p_actor_id bigint, p_trace text
) RETURNS integer LANGUAGE plpgsql AS $$
DECLARE
  due record;
  closed integer := 0;
BEGIN
  IF p_tenant IS NULL OR p_tenant <= 0 OR p_sweep IS NULL OR p_sweep <= 0
     OR p_ids IS NULL OR cardinality(p_ids) > 1000 THEN
    RAISE EXCEPTION 'invalid audit erasure' USING ERRCODE = '22023';
  END IF;
  FOR due IN SELECT id, person_id, auditable_type, auditable_id, fields, subject_identifier FROM base_audit_erasure_deferrals
              WHERE tenant_id = p_tenant AND id = ANY(p_ids) AND done_at IS NULL AND due_at <= clock_timestamp()
              ORDER BY due_at, id FOR UPDATE SKIP LOCKED LOOP
    UPDATE base_audit_erasure_deferrals SET done_at = clock_timestamp() WHERE id = due.id AND done_at IS NULL;
    PERFORM base_audit_erase_values(p_tenant, NULL, p_sweep, due.person_id, due.auditable_type, due.auditable_id,
      due.fields, due.subject_identifier, p_keep_days,
      p_actor_type, p_actor_id, p_trace);
    closed := closed + 1;
  END LOOP;
  RETURN closed;
END;
$$;

REVOKE EXECUTE ON FUNCTION base_audit_erase_values(bigint, bigint, bigint, bigint, text, text, text[], boolean, integer, text, bigint, text) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION base_audit_erase_deferred(bigint, bigint, bigint[], integer, text, bigint, text) FROM PUBLIC;

-- Erases one batch of a tenant's action rows whose retention period has
-- passed: at most p_limit rows older than p_cutoff with an ID above p_after,
-- in ID order, not yet erased. A retained row (its bookmark, else the bit
-- recorded with it) is passed over and counted as kept. The guard clears each
-- row (address, URL, browser and payload values; never who, when, the event
-- or the payload's keys, which are the chain's facts), and one ledger row
-- records the batch. Refused unless the sweep is open in the tenant, as the
-- module that owns sweeps says (base_audit_erasure_causes). `last_id` is the
-- highest ID examined when the batch was full, else NULL: nothing is left.
CREATE FUNCTION base_audit_erase_actions(
  p_tenant bigint, p_sweep bigint, p_cutoff timestamptz, p_after bigint, p_limit integer,
  p_actor_type text, p_actor_id bigint, p_trace text,
  OUT erased integer, OUT kept integer, OUT last_id bigint
) LANGUAGE plpgsql AS $$
DECLARE
  checker regprocedure;
  confirmed boolean;
  ids bigint[];
  first_id bigint;
  examined integer;
BEGIN
  IF p_tenant IS NULL OR p_tenant <= 0 OR p_sweep IS NULL OR p_sweep <= 0
     OR p_cutoff IS NULL OR p_cutoff > clock_timestamp() OR p_after IS NULL OR p_after < 0
     OR p_limit IS NULL OR p_limit NOT BETWEEN 1 AND 1000 THEN
    RAISE EXCEPTION 'invalid audit erasure' USING ERRCODE = '22023';
  END IF;
  SELECT c.checker INTO checker FROM base_audit_erasure_causes c WHERE c.cause = 'sweep';
  IF checker IS NULL THEN
    RAISE EXCEPTION 'audit erasure cause has no checker' USING ERRCODE = '42501';
  END IF;
  EXECUTE format('SELECT %s($1, $2, $3)', checker::regproc) INTO confirmed USING p_tenant, p_sweep, NULL::bigint;
  IF confirmed IS NOT TRUE THEN
    RAISE EXCEPTION 'audit erasure cause is not open in this tenant for this person' USING ERRCODE = '42501';
  END IF;
  -- Locked first, so a bookmark being placed waits and the next statement
  -- reads it.
  SELECT coalesce(array_agg(c.id), '{}'), count(*), min(c.id), max(c.id) INTO ids, examined, first_id, last_id
    FROM (SELECT a.id FROM base_audit_actions a
           WHERE a.tenant_id = p_tenant AND a.id > p_after AND a.erased_at IS NULL AND a.occurred_at < p_cutoff
           ORDER BY a.id LIMIT p_limit FOR UPDATE) c;
  UPDATE base_audit_actions a SET erased_at = clock_timestamp()
   WHERE a.id = ANY(ids)
     AND NOT coalesce((SELECT m.is_retained FROM base_audit_action_metadata m WHERE m.action_id = a.id AND m.partition_month = a.partition_month), a.is_retained);
  GET DIAGNOSTICS erased = ROW_COUNT;
  kept := examined - erased;
  IF erased > 0 THEN
    INSERT INTO base_audit_action_erasures(tenant_id, sweep_id, cutoff, first_id, last_id, action_rows, kept_rows,
        actor_type, actor_id, trace_id)
      VALUES (p_tenant, p_sweep, p_cutoff, first_id, last_id, erased, kept, p_actor_type, p_actor_id, p_trace);
  END IF;
  IF examined < p_limit THEN
    last_id := NULL;
  END IF;
END;
$$;
REVOKE EXECUTE ON FUNCTION base_audit_erase_actions(bigint, bigint, timestamptz, bigint, integer, text, bigint, text) FROM PUBLIC;
