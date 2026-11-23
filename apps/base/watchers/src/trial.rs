//! Bounded historical trials: the live condition evaluator and owner previews,
//! with no admission, execution, or saved record facts.
use crate::definition::Definition;
use crate::review::Draft;
use crate::run::{evaluate, event_subject, logical_event, record_revision, set_action_identity};
use crate::script::{ScriptBudget, MAX_SCRIPT_TIME};
use crate::store::Watchers;
use bilibi_agent_api::Result;
use bilibi_database::{Change, Error};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// What a trial of an observing draft asked, before its scan: the scan's end
/// (so both read the same window) and each event's observation.
struct Observed {
    end: String,
    /// The audited start receipt, written before any provider call.
    trial_id: i64,
    observations: HashMap<i64, (i64, Value)>,
}

/// One page of the window's retained, unerased events, in recorded order.
const EVENTS: &str = "SELECT id,version,payload,recorded_at::text FROM base_events
    WHERE tenant_id=$1 AND name=$2 AND erased_at IS NULL
      AND recorded_at >= $3::text::timestamptz - make_interval(days => $4::int)
      AND recorded_at <= $3::text::timestamptz
      AND ($5::text IS NULL OR (recorded_at,id) > ($5::text::timestamptz,$6))
    ORDER BY recorded_at,id LIMIT $7";

// A hard work ceiling independent of how many events the reader can see.
const MAX_CANDIDATES: usize = 5_000;
const CANDIDATE_PAGE: usize = 128;

impl Watchers {
    pub(crate) async fn trial(
        &self,
        scope: &Scope,
        draft: Draft,
        problems: crate::definition::Problems,
        days: i64,
        limit: i64,
    ) -> Result<Value> {
        // The AI observations come first, holding no connection while the
        // provider works and outside the script budget.
        let observed = self
            .trial_observations(scope, &draft, problems.clone(), days, limit)
            .await?;
        let started = tokio::time::Instant::now();
        let scripted = draft
            .definition
            .as_ref()
            .is_some_and(|definition| definition.scripts().next().is_some());
        let prelude = ScriptBudget::new(scripted.then_some(started + MAX_SCRIPT_TIME));
        let timeout =
            || bilibi_agent_api::invalid("draft", "The scripted trial reached its time limit.");
        let mut client = prelude
            .within(self.db.get())
            .await
            .map_err(|_| timeout())?
            .map_err(Error::from)?;
        let result = match self
            .script_deadline(&**client, scope, scripted, started)
            .await
        {
            Ok(budget) => budget
                .within(self.trial_on(
                    &mut client,
                    scope,
                    (draft, problems),
                    (days, limit),
                    &budget,
                    observed.as_ref(),
                ))
                .await
                .unwrap_or_else(|error| Err(error.into())),
            Err(error) => Err(error.into()),
        };
        if let Err(bilibi_agent_api::Error::Backend(error)) = &result {
            if crate::script::timed_out(error) {
                crate::script::cancel_sql(&client).await;
                drop(bilibi_database::Connection::take(client));
                return Err(timeout());
            }
        }
        if matches!(
            result,
            Err(bilibi_agent_api::Error::Backend(
                Error::UnrecoveredTransaction(_)
            ))
        ) {
            drop(bilibi_database::Connection::take(client));
        }
        result
    }

    /// The bounded window and event count, from the request and the
    /// tenant's limits; an observing draft also stays within
    /// `watchers.trial_observations`.
    async fn trial_window(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
        (days, limit): (i64, i64),
        observing: bool,
    ) -> Result<(i64, i64)> {
        let max_days = self.limit(client, scope, crate::TRIAL_DAYS).await?;
        let mut max_events = self.limit(client, scope, crate::TRIAL_EVENTS).await?;
        if observing {
            max_events =
                max_events.min(self.limit(client, scope, crate::TRIAL_OBSERVATIONS).await?);
        }
        let days = if days == 0 { 30.min(max_days) } else { days };
        let limit = if limit == 0 {
            100.min(max_events)
        } else {
            limit
        };
        if days < 1 || days > max_days {
            return Err(bilibi_agent_api::invalid(
                "days",
                format!("must be between 1 and {max_days}"),
            ));
        }
        if limit < 1 || limit > max_events {
            return Err(bilibi_agent_api::invalid(
                "limit",
                format!("must be between 1 and {max_events}"),
            ));
        }
        Ok((days, limit))
    }

    /// For a draft with `observe`: the events the trial will show (the same
    /// window, order and reader checks), each asked once as the `watchers`
    /// principal within the daily budget, which the reader spends. Writes
    /// no run and keeps no record text.
    async fn trial_observations(
        &self,
        scope: &Scope,
        draft: &Draft,
        problems: crate::definition::Problems,
        days: i64,
        limit: i64,
    ) -> Result<Option<Observed>> {
        let Some(definition) = draft.definition.as_ref() else {
            return Ok(None);
        };
        let Some(observe) = definition.observe.as_ref() else {
            return Ok(None);
        };
        let mut asks = Vec::new();
        let (end, days, trial_id) = {
            let mut client = self.db.get().await.map_err(Error::from)?;
            let review = self.review_draft(&**client, scope, draft, problems).await?;
            if !review.ok() {
                return Err(super::agent::refused(&review));
            }
            bilibi_authz::authorize_key(&**client, scope, crate::VIEW).await?;
            let (days, limit) = self
                .trial_window(&**client, scope, (days, limit), true)
                .await?;
            let trigger = self.trigger(&draft.trigger).ok_or(Error::Invalid)?;
            // The audited start comes first, so any provider call or budget
            // spend below is tied to this person, trigger and draft.
            let trial_id = self
                .start_trial(&mut client, scope, draft, definition, (days, limit))
                .await?;
            let end: String = client
                .query_one("SELECT now()::text", &[])
                .await
                .map_err(Error::from)?
                .get(0);
            let reader_fields = definition.fact_fields(trigger.definition.version, true);
            let (mut cursor_at, mut cursor_id, mut candidates, mut shown) =
                (None::<String>, 0i64, 0usize, 0i64);
            'scan: while candidates < MAX_CANDIDATES && shown < limit {
                let page_size = (MAX_CANDIDATES - candidates).min(CANDIDATE_PAGE) as i64;
                let rows = client
                    .query(
                        EVENTS,
                        &[
                            &scope.tenant(),
                            &draft.trigger,
                            &end,
                            &(days as i32),
                            &cursor_at,
                            &cursor_id,
                            &page_size,
                        ],
                    )
                    .await
                    .map_err(Error::from)?;
                if rows.is_empty() {
                    break;
                }
                for row in rows {
                    candidates += 1;
                    let id: i64 = row.get(0);
                    cursor_id = id;
                    cursor_at = Some(row.get(3));
                    let version: i32 = row.get(1);
                    let payload: Value = row.get(2);
                    if !u16::try_from(version)
                        .is_ok_and(|v| trigger.definition.on.versions.contains(&v))
                    {
                        continue;
                    }
                    let Some(subject) = event_subject(trigger.definition, &payload) else {
                        continue;
                    };
                    if logical_event(trigger.definition, &draft.trigger, id, &payload).is_err()
                        || self
                            .read_facts(
                                &**client,
                                scope,
                                trigger.definition.table,
                                &subject,
                                &reader_fields,
                            )
                            .await?
                            .is_err()
                    {
                        continue;
                    }
                    shown += 1;
                    asks.push(id);
                    if shown == limit || candidates == MAX_CANDIDATES {
                        break 'scan;
                    }
                }
            }
            (end, days, trial_id)
        };
        let trigger = self.trigger(&draft.trigger).ok_or(Error::Invalid)?;
        let version = trigger.definition.version;
        let table = trigger.definition.table;
        let reader_fields = definition.fact_fields(version, true);
        // Automation must read every field the trial evaluates, not only
        // the ones sent; only the declared fields go into the question.
        let mut observation_fields = observe.read_fields(version);
        for field in definition.fact_fields(version, true) {
            if !observation_fields.contains(&field) {
                observation_fields.push(field);
            }
        }
        let tenant = scope.tenant();
        let mut observations = HashMap::new();
        for id in asks {
            let mut client = self.db.get().await.map_err(Error::from)?;
            let mut tx = self
                .mutations
                .begin(
                    &mut client,
                    scope.mutation_context("watchers.trial.observing"),
                )
                .await?;
            let prepared: Result<_> = async {
                bilibi_authz::authorize_key(tx.sql(), scope, crate::VIEW).await?;
                bilibi_authz::authorize_key(tx.sql(), scope, crate::MANAGE).await?;
                if !bilibi_authz::decide(tx.sql(), scope, crate::APPROVE).await?.allowed {
                    return Ok(None);
                }
                let event = tx.sql().query_opt(
                    "SELECT version,payload FROM base_events
                     WHERE tenant_id=$1 AND id=$2 AND name=$3 AND erased_at IS NULL
                       AND recorded_at BETWEEN $4::text::timestamptz - make_interval(days => $5::int)
                                           AND $4::text::timestamptz FOR SHARE",
                    &[&tenant, &id, &draft.trigger, &end, &(days as i32)],
                ).await.map_err(Error::from)?;
                let Some(event) = event else {
                    return Ok(None);
                };
                if !u16::try_from(event.get::<_, i32>(0))
                    .is_ok_and(|version| trigger.definition.on.versions.contains(&version)) {
                    return Ok(None);
                }
                let payload: Value = event.get(1);
                let Some(subject) = event_subject(trigger.definition, &payload) else {
                    return Ok(None);
                };
                if logical_event(trigger.definition, &draft.trigger, id, &payload).is_err() {
                    return Ok(None);
                }
                let Ok((reader, _)) = self.read_facts(tx.sql(), scope, table, &subject, &reader_fields).await? else {
                    return Ok(None);
                };
                let revision = record_revision(reader.get(version))?;
                let principal = match self.principal(tx.sql(), tenant).await {
                    Ok(principal) => principal,
                    Err(Error::Forbidden | Error::Unauthorized) => {
                        return Ok(Some((revision, Err("automation_unavailable"))));
                    }
                    Err(error) => return Err(error.into()),
                };
                let (facts, _) = match self.read_facts(tx.sql(), &principal, table, &subject, &observation_fields).await? {
                    Ok(read) => read,
                    Err(code) => return Ok(Some((revision, Err(code)))),
                };
                if record_revision(facts.get(version))? != revision {
                    return Ok(Some((revision, Err("record_changed"))));
                }
                if let Some(reason) = self.observation_available(tx.sql(), &principal).await? {
                    return Ok(Some((revision, Err(reason))));
                }
                Ok(Some((revision, Ok((principal, facts)))))
            }.await;
            let Some((revision, prepared)) = prepared? else {
                tx.rollback().await?;
                continue;
            };
            let observation = match prepared {
                Ok((principal, facts)) if self.spend_observation(&mut tx, scope).await? => {
                    tx.commit().await?;
                    drop(client);
                    self.ask(&principal, observe, &facts).await?
                }
                other => {
                    tx.rollback().await?;
                    crate::observe::unavailable(other.err().unwrap_or("budget_exhausted"))
                }
            };
            observations.insert(id, (revision, observation));
        }
        Ok(Some(Observed {
            end,
            trial_id,
            observations,
        }))
    }

    /// The audited start receipt of a trial: who, which trigger, the draft's
    /// digest, the window and the limit. Kept even if the trial is
    /// interrupted, without its facts or previews.
    async fn start_trial(
        &self,
        client: &mut tokio_postgres::Client,
        scope: &Scope,
        draft: &Draft,
        definition: &Definition,
        (days, limit): (i64, i64),
    ) -> Result<i64> {
        let digest: String =
            Sha256::digest(serde_json::to_vec(&definition.to_json()).map_err(|_| Error::Internal)?)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
        let tenant = scope.tenant();
        let trigger_name = draft.trigger.clone();
        let mut tx = self
            .mutations
            .begin(client, scope.mutation_context("watchers.trial.started"))
            .await?;
        bilibi_authz::authorize_key(tx.sql(), scope, crate::MANAGE).await?;
        let trial_id = tx.mutate(move |sql| Box::pin(async move {
            let id: i64 = sql.query_one("INSERT INTO base_watcher_trials (tenant_id,trigger,definition_digest,window_days,event_limit) VALUES ($1,$2,$3,$4,$5) RETURNING id", &[&tenant,&trigger_name,&digest,&(days as i32),&(limit as i32)]).await?.get(0);
            Ok((id, vec![Change { kind:"base/watchers.trial".into(), id, old:Value::Null, new:json!({"trigger":trigger_name,"definition_digest":digest,"window_days":days,"event_limit":limit}) }]))
        })).await?;
        tx.commit().await?;
        Ok(trial_id)
    }

    async fn trial_on(
        &self,
        client: &mut tokio_postgres::Client,
        scope: &Scope,
        (draft, problems): (Draft, crate::definition::Problems),
        (days, limit): (i64, i64),
        budget: &ScriptBudget,
        observed: Option<&Observed>,
    ) -> Result<Value> {
        let review = self.review_draft(client, scope, &draft, problems).await?;
        if !review.ok() {
            return Err(super::agent::refused(&review));
        }
        let (days, limit) = self
            .trial_window(client, scope, (days, limit), observed.is_some())
            .await?;
        let trigger = self.trigger(&draft.trigger).ok_or(Error::Invalid)?;
        let definition: &Definition = draft.definition.as_ref().ok_or(Error::Invalid)?;
        let fields = definition.fact_fields(trigger.definition.version, false);
        // An observation tells about the fields it sends: the reader sees an
        // event only when they may read those too.
        let reader_fields = if observed.is_some() {
            definition.fact_fields(trigger.definition.version, true)
        } else {
            fields.clone()
        };
        let tenant = scope.tenant();
        // Log the start before reading events, so a failed or interrupted trial
        // remains visible without storing its facts or action preview. An
        // observing trial logged it before its first provider call.
        let trial_id = match observed {
            Some(observed) => observed.trial_id,
            None => {
                self.start_trial(client, scope, &draft, definition, (days, limit))
                    .await?
            }
        };
        budget.check()?;
        let end: String = match observed {
            Some(observed) => observed.end.clone(),
            None => client
                .query_one("SELECT now()::text", &[])
                .await
                .map_err(Error::from)?
                .get(0),
        };
        let mut cursor_at: Option<String> = None;
        let mut cursor_id = 0i64;
        let reservations = Default::default();
        let mut results = Vec::new();
        let mut bytes = 0usize;
        let mut truncated = false;
        let mut stopped_early = false;
        let mut candidates = 0usize;
        'scan: loop {
            budget.check()?;
            if candidates == MAX_CANDIDATES {
                stopped_early = true;
                break;
            }
            let page_size = (MAX_CANDIDATES - candidates).min(CANDIDATE_PAGE) as i64;
            let rows = client
                .query(
                    EVENTS,
                    &[
                        &tenant,
                        &draft.trigger,
                        &end,
                        &(days as i32),
                        &cursor_at,
                        &cursor_id,
                        &page_size,
                    ],
                )
                .await
                .map_err(Error::from)?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                budget.check()?;
                candidates += 1;
                let id: i64 = row.get(0);
                let version: i32 = row.get(1);
                let at: String = row.get(3);
                cursor_id = id;
                cursor_at = Some(at.clone());
                if !u16::try_from(version)
                    .is_ok_and(|v| trigger.definition.on.versions.contains(&v))
                {
                    continue;
                }
                let payload: Value = row.get(2);
                let Some(subject) = event_subject(trigger.definition, &payload) else {
                    continue;
                };
                let Ok(logical) = logical_event(trigger.definition, &draft.trigger, id, &payload)
                else {
                    continue;
                };
                let tx = client
                    .build_transaction()
                    .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
                    .start()
                    .await
                    .map_err(Error::from)?;
                let evidence: Result<Option<Value>> = async {
                    if tx.query_opt(
                        "SELECT id FROM base_events WHERE tenant_id=$1 AND id=$2 AND erased_at IS NULL",
                        &[&tenant, &id],
                    ).await.map_err(Error::from)?.is_none() {
                        return Ok(None);
                    }
                    let reader = self.read_facts(&tx, scope, trigger.definition.table, &subject, &reader_fields).await?;
                    let Ok((reader, _)) = reader else {
                        return Ok(None);
                    };
                    bilibi_authz::authorize_key(&tx, scope, crate::MANAGE).await?;
                    bilibi_authz::authorize_key(&tx, scope, crate::VIEW).await?;
                    if results.len() == limit as usize {
                        truncated = true;
                        return Ok(None);
                    }
                    let observation = match observed {
                        Some(_) if !bilibi_authz::decide(&tx, scope, crate::APPROVE).await?.allowed => {
                            Some(crate::observe::unavailable("not_permitted"))
                        }
                        Some(observed) => {
                            let revision = record_revision(reader.get(trigger.definition.version))?;
                            Some(match observed.observations.get(&id) {
                                Some((classified, observation)) if *classified == revision => observation.clone(),
                                Some(_) => crate::observe::unavailable("record_changed"),
                                None => crate::observe::unavailable("not_observed"),
                            })
                        }
                        None => None,
                    };
                    let base = |state: &str| {
                        let mut row = json!({"event_id":id,"recorded_at":at,"subject":subject,"state":state});
                        if let Some(observation) = &observation {
                            row["observation"] = observation.clone();
                        }
                        row
                    };
                    if observation.as_ref().is_some_and(|answer| answer["reason"] == "record_changed") {
                        return Ok(Some(base("record_changed")));
                    }
                    let principal = match self.principal(&tx, tenant).await {
                        Ok(principal) => principal,
                        Err(Error::Forbidden | Error::Unauthorized) => return Ok(Some(base("automation_unavailable"))),
                        Err(error) => return Err(error.into()),
                    };
                    let automated = self.read_facts(&tx, &principal, trigger.definition.table, &subject, &fields).await?;
                    let Ok((facts, types)) = automated else {
                        return Ok(Some(base("automation_read_refused")));
                    };
                    let mut seen = facts.clone();
                    if let Some(observation) = &observation {
                        let (field, value) = crate::observe::fact(observation);
                        seen.insert(field, value);
                    }
                    let (matched, outcomes) = evaluate(definition, &seen, &types);
                    let mut effects = Vec::new();
                    if matched {
                        let revision = record_revision(facts.get(trigger.definition.version))?;
                        let authority = bilibi_authz::effective_grants(&tx, scope).await?;
                        let unrestricted = scope.impersonator().is_none()
                            && authority.grant_all
                            && authority.direct_denied.is_empty()
                            && bilibi_authz::fields::restricted(&tx, scope).await?.is_empty()
                            && bilibi_authz::fields::masked(&tx, scope).await?.is_empty();
                        if budget.deadline.is_some() {
                            let fields = definition.script_fact_fields(trigger.definition.version);
                            if self.read_facts(&tx, scope, trigger.definition.table, &subject, &fields).await?.is_err() {
                                return Ok(Some(base("script_facts_restricted")));
                            }
                        }
                        let calls = match self.plan_steps(&tx, &principal, definition, (trigger, &subject, revision), budget.deadline).await? {
                            Ok(calls) => calls,
                            Err(reason) => return Ok(Some(base(reason))),
                        };
                        for (position, call) in calls.iter().enumerate() {
                            let action = self.action(&call.operation).ok_or(Error::Invalid)?;
                            let visible = unrestricted && std::iter::once(action.capability)
                                .chain(action.definition.approval_capabilities.iter().copied())
                                .all(|key| authority.reaches(key, bilibi_authz::Reach::All));
                            let mut effect = json!({"operation":action.key,"title":action.title,"preview":"restricted"});
                            if visible {
                                let input = crate::run::action_input(
                                    action.definition, &action.input, &subject, &call.input, revision,
                                ).ok_or(Error::Invalid)?;
                                set_action_identity(&tx, None, position as i32, &logical).await?;
                                budget.check()?;
                                match self.jobs.preview_principal(&tx, &principal, action.key, &input, &reservations).await {
                                    Ok(output) => {
                                        effect["preview"] = json!("allowed");
                                        effect["effect"] = output["result"].clone();
                                    }
                                    Err(error) => {
                                        crate::run::refusal(error)?;
                                        effect["preview"] = json!("refused");
                                    }
                                }
                            }
                            effects.push(effect);
                        }
                    }
                    let shown_facts: serde_json::Map<String, Value> = facts.into_iter().map(|(key, value)| {
                        let value = if serde_json::to_vec(&value).map_or(true, |v| v.len() > 4096) {
                            json!({"omitted":"value exceeds 4 KiB"})
                        } else {
                            value
                        };
                        (key, value)
                    }).collect();
                    let mut item = base(if matched { "match" } else { "near_miss" });
                    item["current_record"] = json!({"facts":shown_facts,"condition_outcomes":outcomes});
                    item["would_be_effects"] = json!(effects);
                    Ok(Some(item))
                }.await;
                let item = match evidence {
                    Err(
                        error @ bilibi_agent_api::Error::Backend(Error::UnrecoveredTransaction(_)),
                    ) => {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_millis(250),
                            tx.rollback(),
                        )
                        .await;
                        return Err(error);
                    }
                    other => other?,
                };
                budget.commit_trial(tx).await?;
                if truncated {
                    break 'scan;
                }
                if let Some(item) = item {
                    let size = serde_json::to_vec(&item)
                        .map_err(|_| Error::Internal)?
                        .len();
                    if bytes + size > 512 * 1024 - 4096 {
                        truncated = true;
                        break 'scan;
                    }
                    bytes += size;
                    results.push(item);
                }
            }
        }
        Ok(
            json!({"trial_id":trial_id,"window_days":days,"truncated":truncated,"event_evidence":"event_id, recorded_at and subject identify the recorded event; record fields were read today", "coverage":"Retained, unerased events only; retention or erasure can leave gaps.", "effect_basis":"Owner previews use the current record independently.", "scan_notice":if stopped_early { Some("Scan stopped early; narrow the window to check for more events.") } else { None }, "results":results}),
        )
    }
}
