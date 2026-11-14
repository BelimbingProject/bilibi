-- Pre-production replacement, deliberately without copying existing evidence.
-- Monthly storage uses recording time; event time can be backdated.
DROP TABLE base_audit_action_metadata;
CREATE TABLE base_audit_mutations_new (
 LIKE base_audit_mutations INCLUDING DEFAULTS INCLUDING IDENTITY INCLUDING CONSTRAINTS,
 partition_month date NOT NULL DEFAULT date_trunc('month', statement_timestamp() AT TIME ZONE 'UTC')::date,
 is_retained boolean NOT NULL DEFAULT false,
 PRIMARY KEY (partition_month, id)
) PARTITION BY RANGE (partition_month);
CREATE TABLE base_audit_actions_new (
 LIKE base_audit_actions INCLUDING DEFAULTS INCLUDING IDENTITY INCLUDING CONSTRAINTS,
 partition_month date NOT NULL DEFAULT date_trunc('month', statement_timestamp() AT TIME ZONE 'UTC')::date,
 PRIMARY KEY (partition_month, id)
) PARTITION BY RANGE (partition_month);
DROP TABLE base_audit_mutations, base_audit_actions;
ALTER TABLE base_audit_mutations_new RENAME TO base_audit_mutations;
ALTER TABLE base_audit_actions_new RENAME TO base_audit_actions;
-- These heads described the discarded development evidence, not the new ledgers.
DROP TABLE base_audit_chain_heads;
CREATE TABLE base_audit_chain_heads (
 chain text NOT NULL CHECK (chain IN ('mutations', 'actions')),
 tenant_id bigint NOT NULL CHECK (tenant_id >= 0),
 born bigint NOT NULL CHECK (born > 0),
 seq bigint NOT NULL CHECK (seq >= 0), hash bytea NOT NULL,
 sealed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY (chain, tenant_id), UNIQUE (chain, born)
);
DELETE FROM base_audit_verifications;
CREATE TABLE base_audit_action_metadata (
 partition_month date NOT NULL,
 action_id bigint NOT NULL,
 is_retained boolean NOT NULL DEFAULT false,
 PRIMARY KEY (partition_month, action_id),
 FOREIGN KEY (partition_month, action_id) REFERENCES base_audit_actions(partition_month, id)
   ON UPDATE CASCADE ON DELETE RESTRICT
);
CREATE INDEX base_audit_mutations_tenant_timeline_index ON base_audit_mutations(tenant_id, occurred_at DESC, id DESC);
CREATE INDEX base_audit_mutations_auditable_occurred_index ON base_audit_mutations(auditable_type, auditable_id, occurred_at);
CREATE INDEX base_audit_mutations_actor_occurred_index ON base_audit_mutations(actor_type, actor_id, occurred_at);
CREATE INDEX base_audit_mutations_event_index ON base_audit_mutations(event);
CREATE INDEX base_audit_mutations_trace_index ON base_audit_mutations(trace_id);
CREATE INDEX base_audit_mutations_company_index ON base_audit_mutations(company_id);
CREATE INDEX base_audit_mutations_impersonator_index ON base_audit_mutations(impersonator_id) WHERE impersonator_id IS NOT NULL;
CREATE INDEX base_audit_mutations_system_principal_index ON base_audit_mutations(system_principal) WHERE system_principal IS NOT NULL;
CREATE INDEX base_audit_mutations_subject_idx ON base_audit_mutations(subject_name, subject_id, subject_identifier, occurred_at) WHERE subject_name IS NOT NULL;
CREATE INDEX base_audit_actions_tenant_timeline_index ON base_audit_actions(tenant_id, occurred_at DESC, id DESC);
CREATE INDEX base_audit_actions_event_occurred_index ON base_audit_actions(event, occurred_at);
CREATE INDEX base_audit_actions_actor_occurred_index ON base_audit_actions(actor_type, actor_id, occurred_at);
CREATE INDEX base_audit_actions_trace_index ON base_audit_actions(trace_id);
CREATE INDEX base_audit_actions_company_index ON base_audit_actions(company_id);
CREATE INDEX base_audit_actions_impersonator_index ON base_audit_actions(impersonator_id) WHERE impersonator_id IS NOT NULL;
CREATE INDEX base_audit_actions_system_principal_index ON base_audit_actions(system_principal) WHERE system_principal IS NOT NULL;
CREATE UNIQUE INDEX base_audit_mutations_chain ON base_audit_mutations((coalesce(tenant_id, 0)), chain_seq, partition_month) WHERE chain_seq IS NOT NULL;
CREATE INDEX base_audit_mutations_unsealed ON base_audit_mutations(id) WHERE chain_seq IS NULL;
CREATE INDEX base_audit_mutations_id ON base_audit_mutations(id);
ALTER TABLE base_audit_mutations ADD CONSTRAINT base_audit_mutations_link_shape CHECK
 ((chain_seq IS NULL AND chain_hash IS NULL) OR (chain_seq IS NOT NULL AND chain_seq > 0 AND chain_hash IS NOT NULL AND octet_length(chain_hash)=32));
CREATE TABLE base_audit_mutations_retained PARTITION OF base_audit_mutations FOR VALUES FROM ('infinity') TO (MAXVALUE);
CREATE UNIQUE INDEX base_audit_actions_chain ON base_audit_actions((coalesce(tenant_id, 0)), chain_seq, partition_month) WHERE chain_seq IS NOT NULL;
CREATE INDEX base_audit_actions_unsealed ON base_audit_actions(id) WHERE chain_seq IS NULL;
CREATE INDEX base_audit_actions_id ON base_audit_actions(id);
ALTER TABLE base_audit_actions ADD CONSTRAINT base_audit_actions_link_shape CHECK
 ((chain_seq IS NULL AND chain_hash IS NULL) OR (chain_seq IS NOT NULL AND chain_seq > 0 AND chain_hash IS NOT NULL AND octet_length(chain_hash)=32));
CREATE TABLE base_audit_actions_retained PARTITION OF base_audit_actions FOR VALUES FROM ('infinity') TO (MAXVALUE);
CREATE INDEX base_audit_actions_unerased ON base_audit_actions(tenant_id, id, occurred_at) WHERE erased_at IS NULL;

-- Immutable authority and bridge evidence for each completed removal.
CREATE TABLE base_audit_partition_removals (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 chain text NOT NULL CHECK (chain IN ('mutations','actions')),
 partition_month date NOT NULL CHECK (isfinite(partition_month)),
 retention_days integer NOT NULL CHECK (retention_days > 0),
 removed_rows bigint NOT NULL CHECK (removed_rows >= 0),
 retained_rows bigint NOT NULL CHECK (retained_rows >= 0),
 tenant_id bigint NOT NULL, actor_id bigint NOT NULL, trace_id text NOT NULL,
 reason text NOT NULL CHECK (length(btrim(reason)) BETWEEN 1 AND 2000),
 recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 UNIQUE (chain, partition_month)
);
CREATE TABLE base_audit_partition_checkpoints (
 removal_id bigint NOT NULL REFERENCES base_audit_partition_removals(id),
 chain text NOT NULL CHECK (chain IN ('mutations','actions')),
 tenant_id bigint NOT NULL CHECK (tenant_id >= 0),
 first_seq bigint NOT NULL CHECK (first_seq > 0),
 last_seq bigint NOT NULL CHECK (last_seq >= first_seq),
 previous_hash bytea NOT NULL CHECK (octet_length(previous_hash) IN (0,32)),
 last_hash bytea NOT NULL CHECK (octet_length(last_hash)=32),
 PRIMARY KEY (chain, tenant_id, first_seq),
 UNIQUE (chain, tenant_id, last_seq)
);
CREATE FUNCTION base_audit_partition_ledger_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP = 'INSERT' AND base_audit_within('base_audit_remove_partition', TG_RELID) THEN
   RETURN NEW;
 END IF;
 RAISE EXCEPTION 'audit partition evidence is written only by guarded removal' USING ERRCODE='42501';
END;
$$;
CREATE TRIGGER base_audit_partition_removals_guard BEFORE INSERT OR UPDATE OR DELETE ON base_audit_partition_removals
 FOR EACH ROW EXECUTE FUNCTION base_audit_partition_ledger_guard();
CREATE TRIGGER base_audit_partition_removals_truncate BEFORE TRUNCATE ON base_audit_partition_removals
 FOR EACH STATEMENT EXECUTE FUNCTION base_audit_partition_ledger_guard();
CREATE TRIGGER base_audit_partition_checkpoints_guard BEFORE INSERT OR UPDATE OR DELETE ON base_audit_partition_checkpoints
 FOR EACH ROW EXECUTE FUNCTION base_audit_partition_ledger_guard();
CREATE TRIGGER base_audit_partition_checkpoints_truncate BEFORE TRUNCATE ON base_audit_partition_checkpoints
 FOR EACH STATEMENT EXECUTE FUNCTION base_audit_partition_ledger_guard();
CREATE OR REPLACE FUNCTION base_audit_evidence_v1() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
  ledger text := coalesce(TG_ARGV[0], TG_TABLE_NAME);
BEGIN
  IF ledger IN ('base_audit_mutations', 'base_audit_actions') THEN
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
       AND (NEW.id, NEW.company_id, NEW.tenant_id, NEW.actor_type, NEW.actor_id, NEW.actor_role,
            NEW.impersonator_id, NEW.system_principal, NEW.ip_address, NEW.url, NEW.user_agent,
            NEW.auditable_type, NEW.auditable_id, NEW.subject_name, NEW.subject_id, NEW.source,
            NEW.event, NEW.trace_id, NEW.occurred_at, NEW.recorded_at,
            NEW.agent_self_report, NEW.client_identity, NEW.partition_month, NEW.is_retained)
           IS NOT DISTINCT FROM
           (OLD.id, OLD.company_id, OLD.tenant_id, OLD.actor_type, OLD.actor_id, OLD.actor_role,
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
CREATE TRIGGER base_audit_mutations_evidence_guard BEFORE INSERT OR UPDATE OR DELETE ON base_audit_mutations
 FOR EACH ROW EXECUTE FUNCTION base_audit_evidence_v1('base_audit_mutations');
CREATE TRIGGER base_audit_mutations_truncate_guard BEFORE TRUNCATE ON base_audit_mutations
 FOR EACH STATEMENT EXECUTE FUNCTION base_audit_evidence_v1('base_audit_mutations');
CREATE TRIGGER base_audit_mutations_retained_truncate_guard BEFORE TRUNCATE ON base_audit_mutations_retained
 FOR EACH STATEMENT EXECUTE FUNCTION base_audit_evidence_v1('base_audit_mutations');
CREATE TRIGGER base_audit_actions_evidence_guard BEFORE INSERT OR UPDATE OR DELETE ON base_audit_actions
 FOR EACH ROW EXECUTE FUNCTION base_audit_evidence_v1('base_audit_actions');
CREATE TRIGGER base_audit_actions_truncate_guard BEFORE TRUNCATE ON base_audit_actions
 FOR EACH STATEMENT EXECUTE FUNCTION base_audit_evidence_v1('base_audit_actions');
CREATE TRIGGER base_audit_actions_retained_truncate_guard BEFORE TRUNCATE ON base_audit_actions_retained
 FOR EACH STATEMENT EXECUTE FUNCTION base_audit_evidence_v1('base_audit_actions');
CREATE TRIGGER base_audit_chain_heads_guard BEFORE INSERT OR UPDATE OR DELETE ON base_audit_chain_heads FOR EACH ROW EXECUTE FUNCTION base_audit_evidence_v1();
CREATE TRIGGER base_audit_chain_heads_truncate_guard BEFORE TRUNCATE ON base_audit_chain_heads FOR EACH STATEMENT EXECUTE FUNCTION base_audit_evidence_v1();

-- Lifecycle DDL, called before INSERT, never from a row trigger. Identifiers
-- come only from the closed ledger choice and a validated date, not SQL input.
CREATE FUNCTION base_audit_create_partition(p_chain text, p_month date) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
 parent text;
 leaf text;
BEGIN
 IF p_chain NOT IN ('mutations','actions') OR p_chain IS NULL OR p_month IS NULL
    OR NOT isfinite(p_month) OR p_month <> date_trunc('month', p_month)::date THEN
   RAISE EXCEPTION 'invalid audit partition' USING ERRCODE='22023';
 END IF;
 parent := 'base_audit_' || p_chain;
 leaf := parent || '_' || to_char(p_month, 'YYYYMM');
 IF current_user <> (SELECT pg_get_userbyid(relowner) FROM pg_class WHERE oid=parent::regclass) THEN
   RAISE EXCEPTION 'audit partition maintenance requires the evidence owner' USING ERRCODE='42501';
 END IF;
 IF to_regclass(leaf) IS NOT NULL THEN RETURN; END IF;
 PERFORM pg_advisory_xact_lock(841333, 0);
 IF EXISTS (SELECT 1 FROM base_audit_partition_removals WHERE chain=p_chain AND partition_month=p_month) THEN
   RAISE EXCEPTION 'removed audit months cannot be reopened' USING ERRCODE='42501';
 END IF;
 IF to_regclass(leaf) IS NULL THEN
   EXECUTE format('CREATE TABLE %I PARTITION OF %I FOR VALUES FROM (%L) TO (%L)', leaf, parent, p_month, (p_month + interval '1 month')::date);
   EXECUTE format('CREATE TRIGGER truncate_guard BEFORE TRUNCATE ON %I FOR EACH STATEMENT EXECUTE FUNCTION base_audit_evidence_v1(%L)', leaf, parent);
 END IF;
END;
$$;
REVOKE EXECUTE ON FUNCTION base_audit_create_partition(text,date) FROM PUBLIC;
CREATE FUNCTION base_audit_prepare_partitions() RETURNS void LANGUAGE plpgsql AS $$
DECLARE
 month date := date_trunc('month', statement_timestamp() AT TIME ZONE 'UTC')::date;
 chain text;
BEGIN
 FOREACH chain IN ARRAY ARRAY['mutations','actions'] LOOP
   PERFORM base_audit_create_partition(chain, month);
   PERFORM base_audit_create_partition(chain, (month + interval '1 month')::date);
 END LOOP;
END;
$$;
REVOKE EXECUTE ON FUNCTION base_audit_prepare_partitions() FROM PUBLIC;
SELECT base_audit_prepare_partitions();
CREATE OR REPLACE FUNCTION base_audit_chain_commit() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
  v_chain text := TG_ARGV[0];
  linked boolean;
  r record;
  tenant bigint;
  next_seq bigint;
  link bytea;
  ids bigint[] := '{}';
  seqs bigint[] := '{}';
  links bytea[] := '{}';
  stored integer;
BEGIN
  IF v_chain = 'mutations' THEN
    SELECT chain_seq IS NOT NULL INTO linked FROM base_audit_mutations WHERE id = NEW.id AND partition_month = NEW.partition_month;
  ELSE
    SELECT chain_seq IS NOT NULL INTO linked FROM base_audit_actions WHERE id = NEW.id AND partition_month = NEW.partition_month;
  END IF;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'audit evidence vanished before its commit' USING ERRCODE = '42501';
  END IF;
  IF linked THEN
    RETURN NULL;
  END IF;
  -- The sort reads every row, so every row's facts exist before the first
  -- lock. Another transaction's unlinked rows are not visible here; a
  -- committed row with no link is left for verification to report.
  FOR r IN SELECT * FROM base_audit_pending(v_chain, NEW.id) p ORDER BY p.tenant, p.id LOOP
    IF tenant IS DISTINCT FROM r.tenant THEN
      IF tenant IS NOT NULL THEN
        UPDATE base_audit_chain_heads SET seq = next_seq, hash = link, sealed_at = clock_timestamp()
         WHERE chain = v_chain AND tenant_id = tenant;
      END IF;
      tenant := r.tenant;
      PERFORM pg_advisory_xact_lock(841332, base_audit_chain_key(v_chain, tenant));
      SELECT h.seq, h.hash INTO next_seq, link FROM base_audit_chain_heads h
       WHERE h.chain = v_chain AND h.tenant_id = tenant FOR UPDATE;
      IF NOT FOUND THEN
        -- The tenant's first evidence: its chain begins, numbered after the
        -- ledger's newest, one beginning at a time.
        PERFORM pg_advisory_xact_lock(841332, base_audit_chain_key(v_chain || '.born', 0));
        INSERT INTO base_audit_chain_heads(chain, tenant_id, born, seq, hash)
          SELECT v_chain, tenant, coalesce(max(h.born), 0) + 1, 0, ''::bytea
            FROM base_audit_chain_heads h WHERE h.chain = v_chain;
        next_seq := 0;
        link := ''::bytea;
      END IF;
    END IF;
    next_seq := next_seq + 1;
    link := sha256(link || r.facts);
    ids := array_append(ids, r.id);
    seqs := array_append(seqs, next_seq);
    links := array_append(links, link);
  END LOOP;
  IF tenant IS NULL THEN
    RAISE EXCEPTION 'audit evidence vanished before its commit' USING ERRCODE = '42501';
  END IF;
  UPDATE base_audit_chain_heads SET seq = next_seq, hash = link, sealed_at = clock_timestamp()
   WHERE chain = v_chain AND tenant_id = tenant;
  IF v_chain = 'mutations' THEN
    UPDATE base_audit_mutations m SET chain_seq = u.seq, chain_hash = u.link
      FROM unnest(ids, seqs, links) AS u(id, seq, link) WHERE m.id = u.id;
  ELSE
    UPDATE base_audit_actions a SET chain_seq = u.seq, chain_hash = u.link
      FROM unnest(ids, seqs, links) AS u(id, seq, link) WHERE a.id = u.id;
  END IF;
  GET DIAGNOSTICS stored = ROW_COUNT;
  IF stored <> cardinality(ids) THEN
    RAISE EXCEPTION 'audit evidence vanished before its commit' USING ERRCODE = '42501';
  END IF;
  RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER base_audit_mutations_chain AFTER INSERT ON base_audit_mutations
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION base_audit_chain_commit('mutations');
CREATE CONSTRAINT TRIGGER base_audit_actions_chain AFTER INSERT ON base_audit_actions
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION base_audit_chain_commit('actions');

-- Bookmark lookup follows the partitioned foreign key.
CREATE OR REPLACE FUNCTION base_audit_erase_actions(
  p_tenant bigint, p_sweep bigint, p_cutoff timestamptz, p_after bigint, p_limit integer,
  p_company bigint, p_primary boolean, p_companies bigint[],
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
     OR p_limit IS NULL OR p_limit NOT BETWEEN 1 AND 1000
     OR p_company IS NULL OR p_company <= 0 OR p_primary IS NULL OR p_companies IS NULL THEN
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
             AND (a.company_id = p_company
                  OR (p_primary AND (a.company_id IS NULL OR NOT a.company_id = ANY(p_companies))))
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
REVOKE EXECUTE ON FUNCTION base_audit_erase_actions(bigint, bigint, timestamptz, bigint, integer, bigint, boolean, bigint[], text, bigint, text) FROM PUBLIC;
