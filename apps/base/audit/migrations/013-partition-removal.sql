-- A single chronological stream of live links and authorised missing ranges.
-- Checkpoints never claim to recompute removed values: they bridge hashes
-- checked by the guarded remover before the partition was dropped.
CREATE FUNCTION base_audit_chain_items(p_chain text, p_tenant bigint, p_after bigint, p_limit integer)
RETURNS TABLE(id bigint, first_seq bigint, last_seq bigint, hash bytea, previous_hash bytea, facts bytea)
LANGUAGE plpgsql STABLE AS $$
BEGIN
 IF p_chain = 'mutations' THEN
   RETURN QUERY SELECT q.* FROM (
     SELECT m.id, m.chain_seq, m.chain_seq, m.chain_hash, NULL::bytea,
       base_audit_facts(m.id,m.tenant_id,m.occurred_at,m.actor_type,m.actor_id,m.event,
         m.auditable_type,m.auditable_id,base_audit_keys(m.old_values,m.new_values))
       FROM (SELECT * FROM base_audit_mutations WHERE coalesce(tenant_id,0)=p_tenant AND chain_seq>p_after ORDER BY chain_seq LIMIT p_limit) m
     UNION ALL
     SELECT NULL::bigint,c.first_seq,c.last_seq,c.last_hash,c.previous_hash,NULL::bytea
       FROM (SELECT c0.* FROM base_audit_partition_checkpoints c0 JOIN base_audit_partition_removals d ON d.id=c0.removal_id AND d.chain=c0.chain WHERE c0.chain=p_chain AND c0.tenant_id=p_tenant AND c0.last_seq>p_after ORDER BY c0.first_seq LIMIT p_limit) c
   ) q ORDER BY 2 LIMIT p_limit;
 ELSIF p_chain = 'actions' THEN
   RETURN QUERY SELECT q.* FROM (
     SELECT a.id, a.chain_seq, a.chain_seq, a.chain_hash, NULL::bytea,
       base_audit_facts(a.id,a.tenant_id,a.occurred_at,a.actor_type,a.actor_id,a.event,
         NULL,NULL,base_audit_keys(a.payload,NULL))
       FROM (SELECT * FROM base_audit_actions WHERE coalesce(tenant_id,0)=p_tenant AND chain_seq>p_after ORDER BY chain_seq LIMIT p_limit) a
     UNION ALL
     SELECT NULL::bigint,c.first_seq,c.last_seq,c.last_hash,c.previous_hash,NULL::bytea
       FROM (SELECT c0.* FROM base_audit_partition_checkpoints c0 JOIN base_audit_partition_removals d ON d.id=c0.removal_id AND d.chain=c0.chain WHERE c0.chain=p_chain AND c0.tenant_id=p_tenant AND c0.last_seq>p_after ORDER BY c0.first_seq LIMIT p_limit) c
   ) q ORDER BY 2 LIMIT p_limit;
 ELSE
   RAISE EXCEPTION 'unknown audit chain' USING ERRCODE='22023';
 END IF;
END;
$$;

CREATE OR REPLACE FUNCTION base_audit_verify_range(
 p_chain text, p_tenant bigint, p_after bigint, p_prev bytea, p_limit integer,
 OUT checked bigint, OUT last_seq bigint, OUT last_hash bytea, OUT finished boolean,
 OUT break_seq bigint, OUT break_id bigint, OUT reason text
) LANGUAGE plpgsql STABLE AS $$
DECLARE
 r record;
 link bytea;
 head record;
 visited integer := 0;
BEGIN
 IF p_tenant IS NULL OR p_tenant<0 OR p_after IS NULL OR p_after<0 OR p_prev IS NULL
    OR p_limit IS NULL OR p_limit NOT BETWEEN 1 AND 100000 THEN
   RAISE EXCEPTION 'invalid audit verification range' USING ERRCODE='22023';
 END IF;
 checked := 0; last_seq := p_after; last_hash := p_prev; finished := true;
 FOR r IN SELECT * FROM base_audit_chain_items(p_chain,p_tenant,p_after,p_limit) LOOP
   IF r.first_seq <> last_seq + 1 THEN
     break_seq := last_seq + 1; break_id := r.id; reason := 'missing'; RETURN;
   END IF;
   IF r.id IS NULL THEN
     IF r.previous_hash IS DISTINCT FROM last_hash THEN
       break_seq := r.first_seq; reason := 'checkpoint'; RETURN;
     END IF;
     link := r.hash;
   ELSE
     link := sha256(last_hash || r.facts);
     IF link IS DISTINCT FROM r.hash THEN
       break_seq := r.first_seq; break_id := r.id; reason := 'changed'; RETURN;
     END IF;
     checked := checked + 1;
   END IF;
   last_seq := r.last_seq; last_hash := link; visited := visited + 1;
 END LOOP;
 IF visited = p_limit THEN finished := false; RETURN; END IF;
 SELECT h.seq,h.hash INTO head FROM base_audit_chain_heads h WHERE h.chain=p_chain AND h.tenant_id=p_tenant;
 IF NOT FOUND OR head.seq IS DISTINCT FROM last_seq OR head.hash IS DISTINCT FROM last_hash THEN
   break_seq := last_seq + 1; reason := 'tail';
 END IF;
END;
$$;

-- Finds the incoming hash for a partition's non-adjacent row. Both live
-- links and earlier removal checkpoints can be its predecessor.
CREATE FUNCTION base_audit_previous_hash(p_chain text, p_tenant bigint, p_seq bigint) RETURNS bytea
LANGUAGE plpgsql STABLE AS $$
DECLARE
 result bytea;
 matches integer;
BEGIN
 IF p_seq=1 THEN RETURN ''::bytea; END IF;
 IF p_chain='mutations' THEN
   SELECT count(*), (array_agg(q.hash))[1] INTO matches,result FROM (
     SELECT chain_hash AS hash FROM base_audit_mutations WHERE coalesce(tenant_id,0)=p_tenant AND chain_seq=p_seq-1
     UNION ALL
     SELECT c.last_hash FROM base_audit_partition_checkpoints c JOIN base_audit_partition_removals d ON d.id=c.removal_id AND d.chain=c.chain WHERE c.chain=p_chain AND c.tenant_id=p_tenant AND c.last_seq=p_seq-1
   ) q;
 ELSE
   SELECT count(*), (array_agg(q.hash))[1] INTO matches,result FROM (
     SELECT chain_hash AS hash FROM base_audit_actions WHERE coalesce(tenant_id,0)=p_tenant AND chain_seq=p_seq-1
     UNION ALL
     SELECT c.last_hash FROM base_audit_partition_checkpoints c JOIN base_audit_partition_removals d ON d.id=c.removal_id AND d.chain=c.chain WHERE c.chain=p_chain AND c.tenant_id=p_tenant AND c.last_seq=p_seq-1
   ) q;
 END IF;
 IF matches<>1 THEN RAISE EXCEPTION 'audit partition has a missing or duplicated predecessor' USING ERRCODE='42501'; END IF;
 RETURN result;
END;
$$;

CREATE FUNCTION base_audit_remove_partition(
 p_chain text, p_month date, p_days integer, p_tenant bigint, p_actor bigint, p_trace text, p_reason text,
 OUT removal_id bigint, OUT removed bigint, OUT retained bigint
) LANGUAGE plpgsql AS $$
DECLARE
 parent text;
 leaf text;
 r record;
 previous bytea;
 last_tenant bigint := -1;
 last_seq bigint := 0;
 last_hash bytea;
 range_start bigint;
 range_previous bytea;
 range_end bigint;
 range_hash bytea;
 query text;
BEGIN
 IF p_chain IS NULL OR p_chain NOT IN ('mutations','actions') OR p_month IS NULL OR NOT isfinite(p_month)
    OR p_month <> date_trunc('month', p_month)::date OR p_days IS NULL OR p_days NOT BETWEEN 1 AND 365000
    OR p_tenant IS NULL OR p_tenant<=0 OR p_actor IS NULL OR p_actor<=0
    OR p_trace IS NULL OR p_trace='' OR p_reason IS NULL OR length(btrim(p_reason)) NOT BETWEEN 1 AND 2000 THEN
   RAISE EXCEPTION 'invalid audit partition removal' USING ERRCODE='22023';
 END IF;
 IF (p_month + interval '1 month') AT TIME ZONE 'UTC' > clock_timestamp() - make_interval(days=>p_days)
    OR p_month >= date_trunc('month', clock_timestamp() AT TIME ZONE 'UTC')::date THEN
   RAISE EXCEPTION 'audit partition has not expired' USING ERRCODE='42501';
 END IF;
 parent := 'base_audit_' || p_chain;
 leaf := parent || '_' || to_char(p_month,'YYYYMM');
 IF current_user <> (SELECT pg_get_userbyid(relowner) FROM pg_class WHERE oid=parent::regclass) THEN
   RAISE EXCEPTION 'audit partition removal requires the evidence owner' USING ERRCODE='42501';
 END IF;
 -- DDL is serialized before taking relation locks. Verification starts and
 -- steps lock this table, so none can race the check or keep a stale cursor.
 PERFORM pg_advisory_xact_lock(841333,0);
 LOCK TABLE base_audit_verifications IN EXCLUSIVE MODE;
 IF EXISTS (SELECT 1 FROM base_audit_verifications WHERE state='running') THEN
   RAISE EXCEPTION 'finish the running audit verification before removing a partition' USING ERRCODE='55000';
 END IF;
 IF EXISTS (SELECT 1 FROM base_audit_partition_removals WHERE chain=p_chain AND partition_month=p_month) THEN
   RAISE EXCEPTION 'audit partition was already removed' USING ERRCODE='55000';
 END IF;
 IF NOT EXISTS (SELECT 1 FROM pg_inherits WHERE inhparent=parent::regclass AND inhrelid=to_regclass(leaf)) THEN
   RAISE EXCEPTION 'audit partition does not exist' USING ERRCODE='22023';
 END IF;
 -- Locks the parent and its leaves, waiting for in-flight commits and
 -- bookmarks. This is explicit maintenance; timeouts roll back all effects.
 EXECUTE format('LOCK TABLE %I IN ACCESS EXCLUSIVE MODE',parent);
 IF p_chain='actions' THEN LOCK TABLE base_audit_action_metadata IN EXCLUSIVE MODE; END IF;
 IF p_chain='mutations' THEN
   query := 'SELECT m.*,coalesce(m.tenant_id,0) AS tenant,m.is_retained AS keep,
     base_audit_facts(m.id,m.tenant_id,m.occurred_at,m.actor_type,m.actor_id,m.event,
       m.auditable_type,m.auditable_id,base_audit_keys(m.old_values,m.new_values)) AS facts
     FROM base_audit_mutations m WHERE m.partition_month=$1 ORDER BY coalesce(m.tenant_id,0),m.chain_seq';
 ELSE
   query := 'SELECT a.*,coalesce(a.tenant_id,0) AS tenant,coalesce(b.is_retained,a.is_retained) AS keep,
     base_audit_facts(a.id,a.tenant_id,a.occurred_at,a.actor_type,a.actor_id,a.event,
       NULL,NULL,base_audit_keys(a.payload,NULL)) AS facts
     FROM base_audit_actions a LEFT JOIN base_audit_action_metadata b ON b.action_id=a.id AND b.partition_month=a.partition_month
     WHERE a.partition_month=$1 ORDER BY coalesce(a.tenant_id,0),a.chain_seq';
 END IF;
 -- The final ledger counts are known before the append-only row is written.
 EXECUTE 'SELECT count(*) FILTER (WHERE NOT keep),count(*) FILTER (WHERE keep) FROM (' || query || ') q'
   INTO removed,retained USING p_month;
 INSERT INTO base_audit_partition_removals(chain,partition_month,retention_days,removed_rows,retained_rows,tenant_id,actor_id,trace_id,reason)
   VALUES(p_chain,p_month,p_days,removed,retained,p_tenant,p_actor,p_trace,btrim(p_reason)) RETURNING id INTO removal_id;
 FOR r IN EXECUTE query USING p_month LOOP
   IF r.chain_seq IS NULL OR r.chain_hash IS NULL THEN
     RAISE EXCEPTION 'unsealed audit evidence cannot be removed' USING ERRCODE='42501';
   END IF;
   IF r.tenant=last_tenant AND r.chain_seq=last_seq+1 THEN previous := last_hash;
   ELSE previous := base_audit_previous_hash(p_chain,r.tenant,r.chain_seq);
   END IF;
   IF sha256(previous || r.facts) IS DISTINCT FROM r.chain_hash THEN
     RAISE EXCEPTION 'broken audit evidence cannot be removed' USING ERRCODE='42501';
   END IF;
   IF range_start IS NOT NULL AND (r.keep OR r.tenant<>last_tenant OR r.chain_seq<>range_end+1) THEN
     INSERT INTO base_audit_partition_checkpoints VALUES(removal_id,p_chain,last_tenant,range_start,range_end,range_previous,range_hash);
     range_start := NULL;
   END IF;
   IF NOT r.keep THEN
     IF range_start IS NULL THEN range_start:=r.chain_seq; range_previous:=previous; END IF;
     range_end:=r.chain_seq; range_hash:=r.chain_hash;
   END IF;
   last_tenant:=r.tenant; last_seq:=r.chain_seq; last_hash:=r.chain_hash;
 END LOOP;
 IF range_start IS NOT NULL THEN
   INSERT INTO base_audit_partition_checkpoints VALUES(removal_id,p_chain,last_tenant,range_start,range_end,range_previous,range_hash);
 END IF;
 IF p_chain='actions' THEN
   UPDATE base_audit_actions a SET partition_month='infinity'
     WHERE a.partition_month=p_month AND coalesce((SELECT b.is_retained FROM base_audit_action_metadata b
       WHERE b.partition_month=a.partition_month AND b.action_id=a.id),a.is_retained);
   DELETE FROM base_audit_action_metadata WHERE partition_month=p_month;
 ELSE
   UPDATE base_audit_mutations SET partition_month='infinity' WHERE partition_month=p_month AND is_retained;
 END IF;
 -- Moved rows fire deferred INSERT chain triggers. Settle them before DDL;
 -- they find their unchanged links in the retained partition and do nothing.
 IF p_chain='actions' THEN SET CONSTRAINTS base_audit_actions_chain IMMEDIATE;
 ELSE SET CONSTRAINTS base_audit_mutations_chain IMMEDIATE;
 END IF;
 EXECUTE format('ALTER TABLE %I DETACH PARTITION %I',parent,leaf);
 EXECUTE format('DROP TABLE %I',leaf);
END;
$$;
REVOKE EXECUTE ON FUNCTION base_audit_remove_partition(text,date,integer,bigint,bigint,text,text) FROM PUBLIC;
