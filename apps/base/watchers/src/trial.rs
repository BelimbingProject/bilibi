//! Bounded historical trials: the live condition evaluator and owner previews,
//! with no admission, execution, or saved record facts.
use crate::definition::Definition;
use crate::review::Draft;
use crate::run::{evaluate, event_subject, logical_event, record_revision, set_action_identity};
use crate::store::Watchers;
use bilibi_agent_api::Result;
use bilibi_database::{Change, Error};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

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
        let mut client = self.db.get().await.map_err(Error::from)?;
        let result = self
            .trial_on(&mut client, scope, draft, problems, days, limit)
            .await;
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

    async fn trial_on(
        &self,
        client: &mut tokio_postgres::Client,
        scope: &Scope,
        draft: Draft,
        problems: crate::definition::Problems,
        days: i64,
        limit: i64,
    ) -> Result<Value> {
        let review = self.review_draft(client, scope, &draft, problems).await?;
        if !review.ok() {
            return Err(super::agent::refused(&review));
        }
        let max_days = self.limit(client, scope, crate::TRIAL_DAYS).await?;
        let max_events = self.limit(client, scope, crate::TRIAL_EVENTS).await?;
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
        let trigger = self.trigger(&draft.trigger).ok_or(Error::Invalid)?;
        let definition: &Definition = draft.definition.as_ref().ok_or(Error::Invalid)?;
        let fields = definition.fact_fields(trigger.definition.version);
        let digest: String =
            Sha256::digest(serde_json::to_vec(&definition.to_json()).map_err(|_| Error::Internal)?)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
        let tenant = scope.tenant();
        let trigger_name = draft.trigger.clone();
        // Log the start before reading events, so a failed or interrupted trial
        // remains visible without storing its facts or action preview.
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
        let end: String = client
            .query_one("SELECT now()::text", &[])
            .await
            .map_err(Error::from)?
            .get(0);
        let mut cursor_at: Option<String> = None;
        let mut cursor_id = 0i64;
        let reservations = Default::default();
        let mut results = Vec::new();
        let mut bytes = 0usize;
        let mut truncated = false;
        let mut stopped_early = false;
        let mut candidates = 0usize;
        'scan: loop {
            if candidates == MAX_CANDIDATES {
                stopped_early = true;
                break;
            }
            let page_size = (MAX_CANDIDATES - candidates).min(CANDIDATE_PAGE) as i64;
            let rows = client
                .query(
                    "SELECT id,version,payload,recorded_at::text FROM base_events
                 WHERE tenant_id=$1 AND name=$2 AND erased_at IS NULL
                   AND recorded_at >= $3::text::timestamptz - make_interval(days => $4::int)
                   AND recorded_at <= $3::text::timestamptz
                   AND ($5::text IS NULL OR (recorded_at,id) > ($5::text::timestamptz,$6))
                 ORDER BY recorded_at,id LIMIT $7",
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
                    let reader = self.read_facts(&tx, scope, trigger.definition.table, &subject, &fields).await?;
                    if reader.is_err() {
                        return Ok(None);
                    }
                    bilibi_authz::authorize_key(&tx, scope, crate::MANAGE).await?;
                    bilibi_authz::authorize_key(&tx, scope, crate::VIEW).await?;
                    if results.len() == limit as usize {
                        truncated = true;
                        return Ok(None);
                    }
                    let base = |state: &str| json!({"event_id":id,"recorded_at":at,"subject":subject,"state":state});
                    let principal = match self.principal(&tx, tenant).await {
                        Ok(principal) => principal,
                        Err(Error::Forbidden | Error::Unauthorized) => return Ok(Some(base("automation_unavailable"))),
                        Err(error) => return Err(error.into()),
                    };
                    let automated = self.read_facts(&tx, &principal, trigger.definition.table, &subject, &fields).await?;
                    let Ok((facts, types)) = automated else {
                        return Ok(Some(base("automation_read_refused")));
                    };
                    let (matched, outcomes) = evaluate(definition, &facts, &types);
                    let mut effects = Vec::new();
                    if matched {
                        let revision = record_revision(facts.get(trigger.definition.version))?;
                        let authority = bilibi_authz::effective_grants(&tx, scope).await?;
                        let unrestricted = scope.impersonator().is_none()
                            && authority.grant_all
                            && authority.direct_denied.is_empty()
                            && bilibi_authz::fields::restricted(&tx, scope).await?.is_empty()
                            && bilibi_authz::fields::masked(&tx, scope).await?.is_empty();
                        let calls = match self.plan_steps(&tx, &principal, definition, &facts).await? {
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
                tx.commit().await.map_err(Error::from)?;
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
