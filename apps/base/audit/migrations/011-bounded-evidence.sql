-- Bounded audit evidence (#576, #578, #579).
--
-- 1. One hash chain per tenant and ledger instead of one for the whole
--    installation, linked in one pass per commit instead of once per row, so
--    tenants do not queue behind each other and a commit with many changes
--    holds its tenant's lock for one statement.
-- 2. Verification in bounded ranges, with a stored run so a queued job can
--    resume where it stopped.
-- 3. A guarded, ledgered erasure of old action rows' request facts and payload
--    values, the retention path for the action ledger.

-- The facts one link covers: ID, tenant, time, actor, event, record and field
-- names, exactly as migration 009 hashed them. A link is
-- sha256(previous link || facts).
CREATE FUNCTION base_audit_facts(
  p_id bigint, p_tenant bigint, p_at timestamptz, p_actor_type text, p_actor_id bigint,
  p_event text, p_type text, p_record text, p_fields text
) RETURNS bytea LANGUAGE sql STABLE AS $$
  SELECT convert_to(concat_ws(chr(31), p_id::text, coalesce(p_tenant::text, ''),
    to_char(p_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US'), p_actor_type, p_actor_id::text,
    p_event, coalesce(p_type, ''), coalesce(p_record, ''), p_fields), 'UTF8')
$$;

-- The advisory lock of one tenant's chain: pg_advisory_xact_lock(841332, key).
CREATE FUNCTION base_audit_chain_key(p_chain text, p_tenant bigint) RETURNS integer
LANGUAGE sql IMMUTABLE AS $$ SELECT hashtext(p_chain || ':' || p_tenant::text) $$;

DROP TRIGGER base_audit_mutations_chain ON base_audit_mutations;
DROP TRIGGER base_audit_actions_chain ON base_audit_actions;
DROP FUNCTION base_audit_chain_commit();
DROP FUNCTION base_audit_verify(text);
DROP FUNCTION base_audit_link(bytea, bigint, bigint, timestamptz, text, bigint, text, text, text, text);
DROP TABLE base_audit_chain_heads;
DROP INDEX base_audit_mutations_chain;
DROP INDEX base_audit_actions_chain;

-- The last link of each tenant's chain, so a removed tail is found. Rows with
-- no tenant form chain 0. `born` numbers a ledger's heads in the order their
-- chains began, without gaps, so a removed head (a tenant's whole chain) is
-- found too.
CREATE TABLE base_audit_chain_heads (
 chain text NOT NULL CHECK (chain IN ('mutations', 'actions')),
 tenant_id bigint NOT NULL CHECK (tenant_id >= 0),
 born bigint NOT NULL CHECK (born > 0),
 seq bigint NOT NULL CHECK (seq >= 0),
 hash bytea NOT NULL,
 sealed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY (chain, tenant_id),
 UNIQUE (chain, born)
);

-- Old action rows lose their request facts and payload values once their
-- retention period has passed; `erased_at` says when. The index is the rows
-- still to erase, in the ID order the retention walks; the event time is a
-- key so a pass over rows too new to erase never leaves the index.
ALTER TABLE base_audit_actions ADD COLUMN erased_at timestamptz;
CREATE INDEX base_audit_actions_unerased ON base_audit_actions(tenant_id, id, occurred_at) WHERE erased_at IS NULL;

-- One row per erasing batch: which sweep, up to when, how many.
CREATE TABLE base_audit_action_erasures (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL CHECK (tenant_id > 0),
 sweep_id bigint NOT NULL CHECK (sweep_id > 0),
 cutoff timestamptz NOT NULL,
 first_id bigint NOT NULL,
 last_id bigint NOT NULL,
 action_rows integer NOT NULL CHECK (action_rows > 0),
 -- Retained rows the batch passed over.
 kept_rows integer NOT NULL DEFAULT 0 CHECK (kept_rows >= 0),
 actor_type varchar(40) NOT NULL,
 actor_id bigint NOT NULL,
 trace_id text,
 occurred_at timestamptz,
 recorded_at timestamptz(6),
 CHECK (actor_type IN ('user','agent','guest','console','scheduler','queue','system','unknown'))
);
CREATE INDEX base_audit_action_erasures_sweep ON base_audit_action_erasures(tenant_id, sweep_id, id);

CREATE OR REPLACE FUNCTION base_audit_evidence_v1() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  -- Nested throughout: PL/pgSQL may evaluate every operand, and each table
  -- has only some of these columns.
  IF TG_OP = 'UPDATE' AND TG_TABLE_NAME IN ('base_audit_mutations', 'base_audit_actions') THEN
    -- The chain link is by far the commonest update, so it is decided first
    -- and from the columns before the call stack is read. The row that is
    -- stored is the old one with only its link set: nothing else can change.
    IF NEW.chain_seq IS NOT NULL AND NEW.chain_hash IS NOT NULL THEN
      IF OLD.chain_seq IS NULL AND base_audit_within('base_audit_chain_commit', TG_RELID) THEN
        OLD.chain_seq := NEW.chain_seq;
        OLD.chain_hash := NEW.chain_hash;
        RETURN OLD;
      END IF;
      -- Migration 011 links every row again, once, under its tenant.
      IF base_audit_within('base_audit_reseal', TG_RELID) THEN
        OLD.chain_seq := NEW.chain_seq;
        OLD.chain_hash := NEW.chain_hash;
        RETURN OLD;
      END IF;
    END IF;
  END IF;
  IF TG_OP = 'UPDATE' AND TG_TABLE_NAME = 'base_audit_mutations' THEN
    IF base_audit_within('base_audit_erase_values', TG_RELID)
       AND (NEW.id, NEW.company_id, NEW.tenant_id, NEW.actor_type, NEW.actor_id, NEW.actor_role,
            NEW.impersonator_id, NEW.system_principal, NEW.ip_address, NEW.url, NEW.user_agent,
            NEW.auditable_type, NEW.auditable_id, NEW.subject_name, NEW.subject_id, NEW.source,
            NEW.event, NEW.trace_id, NEW.occurred_at, NEW.recorded_at,
            NEW.agent_self_report, NEW.client_identity)
           IS NOT DISTINCT FROM
           (OLD.id, OLD.company_id, OLD.tenant_id, OLD.actor_type, OLD.actor_id, OLD.actor_role,
            OLD.impersonator_id, OLD.system_principal, OLD.ip_address, OLD.url, OLD.user_agent,
            OLD.auditable_type, OLD.auditable_id, OLD.subject_name, OLD.subject_id, OLD.source,
            OLD.event, OLD.trace_id, OLD.occurred_at, OLD.recorded_at,
            OLD.agent_self_report, OLD.client_identity)
       AND (NEW.chain_seq, NEW.chain_hash) IS NOT DISTINCT FROM (OLD.chain_seq, OLD.chain_hash) THEN
      RETURN NEW;
    END IF;
  END IF;
  IF TG_OP = 'UPDATE' AND TG_TABLE_NAME = 'base_audit_actions' THEN
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
  IF TG_TABLE_NAME = 'base_audit_chain_heads' THEN
    IF TG_OP = 'INSERT' THEN
      -- A chain begins empty, as its tenant's first evidence commits.
      IF NEW.seq = 0 AND NEW.hash = ''::bytea AND base_audit_within('base_audit_chain_commit', TG_RELID) THEN
        RETURN NEW;
      END IF;
      IF base_audit_within('base_audit_reseal', TG_RELID) THEN
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
  IF TG_TABLE_NAME = 'base_audit_erasures' THEN
    IF TG_OP <> 'INSERT' OR NOT base_audit_within('base_audit_erase_values', TG_RELID) THEN
      RAISE EXCEPTION 'the audit erasure ledger is written only by base_audit_erase_values'
        USING ERRCODE = '42501';
    END IF;
  END IF;
  IF TG_TABLE_NAME = 'base_audit_action_erasures' THEN
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
  IF TG_TABLE_NAME <> 'base_audit_capture_failures' THEN
    IF NEW.occurred_at IS NULL THEN
      NEW.occurred_at := date_trunc('second', NEW.recorded_at);
    END IF;
  END IF;
  RETURN NEW;
END;
$$;
CREATE TRIGGER base_audit_chain_heads_guard BEFORE INSERT OR UPDATE OR DELETE ON base_audit_chain_heads FOR EACH ROW EXECUTE FUNCTION base_audit_evidence_v1();
CREATE TRIGGER base_audit_chain_heads_truncate_guard BEFORE TRUNCATE ON base_audit_chain_heads FOR EACH STATEMENT EXECUTE FUNCTION base_audit_evidence_v1();
CREATE TRIGGER base_audit_action_erasures_evidence_guard BEFORE INSERT OR UPDATE OR DELETE ON base_audit_action_erasures FOR EACH ROW EXECUTE FUNCTION base_audit_evidence_v1();
CREATE TRIGGER base_audit_action_erasures_truncate_guard BEFORE TRUNCATE ON base_audit_action_erasures FOR EACH STATEMENT EXECUTE FUNCTION base_audit_evidence_v1();

-- Links every existing row again under its own tenant, oldest ID first; used
-- once, below, then dropped. Rows written after it are linked as they commit.
CREATE FUNCTION base_audit_reseal() RETURNS void LANGUAGE plpgsql AS $$
DECLARE
  t record;
  r record;
  next_seq bigint;
  link bytea;
  heads bigint := 0;
BEGIN
  FOR t IN SELECT DISTINCT coalesce(tenant_id, 0) AS tenant FROM base_audit_mutations ORDER BY 1 LOOP
    next_seq := 0;
    link := ''::bytea;
    FOR r IN SELECT id, tenant_id, occurred_at, actor_type, actor_id, event, auditable_type, auditable_id, old_values, new_values
               FROM base_audit_mutations WHERE coalesce(tenant_id, 0) = t.tenant ORDER BY id LOOP
      next_seq := next_seq + 1;
      link := sha256(link || base_audit_facts(r.id, r.tenant_id, r.occurred_at, r.actor_type, r.actor_id, r.event,
        r.auditable_type, r.auditable_id, base_audit_keys(r.old_values, r.new_values)));
      UPDATE base_audit_mutations SET chain_seq = next_seq, chain_hash = link WHERE id = r.id;
    END LOOP;
    heads := heads + 1;
    INSERT INTO base_audit_chain_heads(chain, tenant_id, born, seq, hash) VALUES ('mutations', t.tenant, heads, next_seq, link);
  END LOOP;
  heads := 0;
  FOR t IN SELECT DISTINCT coalesce(tenant_id, 0) AS tenant FROM base_audit_actions ORDER BY 1 LOOP
    next_seq := 0;
    link := ''::bytea;
    FOR r IN SELECT id, tenant_id, occurred_at, actor_type, actor_id, event, payload
               FROM base_audit_actions WHERE coalesce(tenant_id, 0) = t.tenant ORDER BY id LOOP
      next_seq := next_seq + 1;
      link := sha256(link || base_audit_facts(r.id, r.tenant_id, r.occurred_at, r.actor_type, r.actor_id, r.event,
        NULL, NULL, base_audit_keys(r.payload, NULL)));
      UPDATE base_audit_actions SET chain_seq = next_seq, chain_hash = link WHERE id = r.id;
    END LOOP;
    heads := heads + 1;
    INSERT INTO base_audit_chain_heads(chain, tenant_id, born, seq, hash) VALUES ('actions', t.tenant, heads, next_seq, link);
  END LOOP;
END;
$$;
SELECT base_audit_reseal();
DROP FUNCTION base_audit_reseal();
CREATE UNIQUE INDEX base_audit_mutations_chain ON base_audit_mutations((coalesce(tenant_id, 0)), chain_seq) WHERE chain_seq IS NOT NULL;
CREATE UNIQUE INDEX base_audit_actions_chain ON base_audit_actions((coalesce(tenant_id, 0)), chain_seq) WHERE chain_seq IS NOT NULL;

-- Links a transaction's new evidence rows as it commits: a deferred
-- constraint trigger, so no row is ever committed unlinked. A constraint
-- trigger fires once per row, so the first firing for a ledger links every
-- row the transaction wrote there, in one pass, and the others find their row
-- linked and return. The facts are computed before any lock is taken; then,
-- tenant by tenant, the tenant's lock makes committing writers take links one
-- at a time, in commit order, and one UPDATE stores them.
CREATE FUNCTION base_audit_chain_commit() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
  v_chain text := CASE WHEN TG_TABLE_NAME = 'base_audit_mutations' THEN 'mutations' ELSE 'actions' END;
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
    SELECT chain_seq IS NOT NULL INTO linked FROM base_audit_mutations WHERE id = NEW.id;
  ELSE
    SELECT chain_seq IS NOT NULL INTO linked FROM base_audit_actions WHERE id = NEW.id;
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

-- The calling transaction's unlinked rows of one ledger from `p_from` on,
-- with the facts their links cover.
CREATE FUNCTION base_audit_pending(p_chain text, p_from bigint)
RETURNS TABLE (tenant bigint, id bigint, facts bytea) LANGUAGE plpgsql STABLE AS $$
BEGIN
  IF p_chain = 'mutations' THEN
    RETURN QUERY
      SELECT coalesce(m.tenant_id, 0), m.id,
             base_audit_facts(m.id, m.tenant_id, m.occurred_at, m.actor_type, m.actor_id, m.event,
               m.auditable_type, m.auditable_id, base_audit_keys(m.old_values, m.new_values))
        FROM base_audit_mutations m
       WHERE m.chain_seq IS NULL AND m.id >= p_from AND base_audit_written_here(m.xmin);
  ELSE
    RETURN QUERY
      SELECT coalesce(a.tenant_id, 0), a.id,
             base_audit_facts(a.id, a.tenant_id, a.occurred_at, a.actor_type, a.actor_id, a.event,
               NULL, NULL, base_audit_keys(a.payload, NULL))
        FROM base_audit_actions a
       WHERE a.chain_seq IS NULL AND a.id >= p_from AND base_audit_written_here(a.xmin);
  END IF;
END;
$$;
CREATE CONSTRAINT TRIGGER base_audit_mutations_chain AFTER INSERT ON base_audit_mutations
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION base_audit_chain_commit();
CREATE CONSTRAINT TRIGGER base_audit_actions_chain AFTER INSERT ON base_audit_actions
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION base_audit_chain_commit();

-- Recomputes at most p_limit links of one tenant's chain after link p_after,
-- whose hash was p_prev (0 and '' for the first). `finished` when the chain's
-- end was reached or it broke: `reason` is then NULL when it is whole so far,
-- or `missing` (a link was removed), `changed` (a row's facts differ from its
-- link) or `tail` (the last links were removed), at break_seq. One statement
-- reads one snapshot, so the head it compares is the head of the rows it saw.
CREATE FUNCTION base_audit_verify_range(
  p_chain text, p_tenant bigint, p_after bigint, p_prev bytea, p_limit integer,
  OUT checked bigint, OUT last_seq bigint, OUT last_hash bytea, OUT finished boolean,
  OUT break_seq bigint, OUT break_id bigint, OUT reason text
) LANGUAGE plpgsql STABLE AS $$
DECLARE
  r record;
  link bytea;
  head record;
BEGIN
  IF p_tenant IS NULL OR p_tenant < 0 OR p_after IS NULL OR p_after < 0 OR p_prev IS NULL
     OR p_limit IS NULL OR p_limit NOT BETWEEN 1 AND 100000 THEN
    RAISE EXCEPTION 'invalid audit verification range' USING ERRCODE = '22023';
  END IF;
  checked := 0;
  last_seq := p_after;
  last_hash := p_prev;
  finished := true;
  IF p_chain = 'mutations' THEN
    FOR r IN SELECT m.id, m.chain_seq, m.chain_hash,
                    base_audit_facts(m.id, m.tenant_id, m.occurred_at, m.actor_type, m.actor_id, m.event,
                      m.auditable_type, m.auditable_id, base_audit_keys(m.old_values, m.new_values)) AS facts
               FROM base_audit_mutations m
              WHERE coalesce(m.tenant_id, 0) = p_tenant AND m.chain_seq > p_after
              ORDER BY m.chain_seq LIMIT p_limit LOOP
      IF r.chain_seq <> last_seq + 1 THEN
        break_seq := last_seq + 1; break_id := r.id; reason := 'missing'; RETURN;
      END IF;
      link := sha256(last_hash || r.facts);
      IF link IS DISTINCT FROM r.chain_hash THEN
        break_seq := r.chain_seq; break_id := r.id; reason := 'changed'; RETURN;
      END IF;
      last_seq := r.chain_seq;
      last_hash := link;
      checked := checked + 1;
    END LOOP;
  ELSIF p_chain = 'actions' THEN
    FOR r IN SELECT a.id, a.chain_seq, a.chain_hash,
                    base_audit_facts(a.id, a.tenant_id, a.occurred_at, a.actor_type, a.actor_id, a.event,
                      NULL, NULL, base_audit_keys(a.payload, NULL)) AS facts
               FROM base_audit_actions a
              WHERE coalesce(a.tenant_id, 0) = p_tenant AND a.chain_seq > p_after
              ORDER BY a.chain_seq LIMIT p_limit LOOP
      IF r.chain_seq <> last_seq + 1 THEN
        break_seq := last_seq + 1; break_id := r.id; reason := 'missing'; RETURN;
      END IF;
      link := sha256(last_hash || r.facts);
      IF link IS DISTINCT FROM r.chain_hash THEN
        break_seq := r.chain_seq; break_id := r.id; reason := 'changed'; RETURN;
      END IF;
      last_seq := r.chain_seq;
      last_hash := link;
      checked := checked + 1;
    END LOOP;
  ELSE
    RAISE EXCEPTION 'unknown audit chain %', p_chain USING ERRCODE = '22023';
  END IF;
  IF checked = p_limit THEN
    finished := false;
    RETURN;
  END IF;
  SELECT h.seq, h.hash INTO head FROM base_audit_chain_heads h WHERE h.chain = p_chain AND h.tenant_id = p_tenant;
  IF NOT FOUND OR head.seq IS DISTINCT FROM last_seq OR head.hash IS DISTINCT FROM last_hash THEN
    break_seq := last_seq + 1; reason := 'tail';
  END IF;
END;
$$;

-- One verification of both ledgers, walked by a queued job in bounded steps.
-- The cursor (`chain`, `chain_tenant`, `seq`, `hash`) is where the next step
-- starts, so a job that stops resumes there. Progress is working state, not
-- evidence: the result is also recorded as an `audit.chain.verified` action.
CREATE TABLE base_audit_verifications (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL CHECK (tenant_id > 0),
 requested_by bigint NOT NULL CHECK (requested_by > 0),
 state text NOT NULL DEFAULT 'running' CHECK (state IN ('running', 'whole', 'broken', 'abandoned')),
 chain text NOT NULL DEFAULT 'mutations' CHECK (chain IN ('mutations', 'actions')),
 -- The tenant whose chain is being walked; -1 before a ledger's first.
 chain_tenant bigint NOT NULL DEFAULT -1 CHECK (chain_tenant >= -1),
 -- Whether that tenant's chain has been walked to its end.
 tenant_done boolean NOT NULL DEFAULT true,
 seq bigint NOT NULL DEFAULT 0 CHECK (seq >= 0),
 hash bytea NOT NULL DEFAULT ''::bytea,
 -- Links recomputed and tenants finished in the ledger being walked.
 checked bigint NOT NULL DEFAULT 0 CHECK (checked >= 0),
 tenants bigint NOT NULL DEFAULT 0 CHECK (tenants >= 0),
 -- One report per finished ledger.
 reports jsonb NOT NULL DEFAULT '[]'::jsonb,
 requested_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 progressed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 finished_at timestamptz,
 CHECK ((state = 'running') = (finished_at IS NULL))
);
-- One verification runs at a time.
CREATE UNIQUE INDEX base_audit_verifications_running ON base_audit_verifications((true)) WHERE finished_at IS NULL;

-- Erases one batch of a tenant's action rows whose retention period has
-- passed: at most p_limit rows older than p_cutoff with an ID above p_after,
-- in ID order, not yet erased, that the company run covers (the row's
-- company, or for the primary company's run a row of no listed company).
-- A retained row (its bookmark, else the bit recorded with it) is passed over
-- and counted as kept. The guard clears each row (address, URL, browser and
-- payload values; never who, when, the event or the payload's keys, which are
-- the chain's facts), and one ledger row records the batch. Refused unless
-- the sweep is open in the tenant, as the module that owns sweeps says
-- (base_audit_erasure_causes). `last_id` is the highest ID examined when the
-- batch was full, else NULL: nothing is left.
CREATE FUNCTION base_audit_erase_actions(
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
     AND NOT coalesce((SELECT m.is_retained FROM base_audit_action_metadata m WHERE m.action_id = a.id), a.is_retained);
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
