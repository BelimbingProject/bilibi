//! Bounded historical trials: the live condition evaluator and owner previews,
//! with no admission, execution, or saved record facts.
use crate::definition::Definition;
use crate::review::Draft;
use crate::store::Watchers;
use bilibi_agent_api::Result;
use bilibi_database::{Change, Error};
use bilibi_grid::FieldType;
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

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
        let review = self
            .review_draft(&**client, scope, &draft, problems)
            .await?;
        if !review.ok() {
            return Err(super::agent::refused(&review));
        }
        let max_days = self.limit(&**client, scope, crate::TRIAL_DAYS).await?;
        let max_events = self.limit(&**client, scope, crate::TRIAL_EVENTS).await?;
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
            .begin(
                &mut client,
                scope.mutation_context("watchers.trial.started"),
            )
            .await?;
        bilibi_authz::authorize_key(tx.sql(), scope, crate::MANAGE).await?;
        let trial_id = tx.mutate(move |sql| Box::pin(async move {
            let id: i64 = sql.query_one("INSERT INTO base_watcher_trials (tenant_id,trigger,definition_digest,window_days,event_limit) VALUES ($1,$2,$3,$4,$5) RETURNING id", &[&tenant,&trigger_name,&digest,&(days as i32),&(limit as i32)]).await?.get(0);
            Ok((id, vec![Change { kind:"base/watchers.trial".into(), id, old:Value::Null, new:json!({"trigger":trigger_name,"definition_digest":digest,"window_days":days,"event_limit":limit,"completed":false}) }]))
        })).await?;
        tx.commit().await?;
        let rows = client.query(
            "SELECT id,version,payload,recorded_at::text FROM base_events WHERE tenant_id=$1 AND name=$2 AND erased_at IS NULL AND recorded_at >= now() - make_interval(days => $3::int) AND recorded_at <= now() ORDER BY recorded_at DESC,id DESC LIMIT $4",
            &[&scope.tenant(), &draft.trigger, &(days as i32), &(limit + 1)],
        ).await.map_err(Error::from)?;
        let mut truncated = rows.len() > limit as usize;
        let principal = match self.principal(&**client, scope.tenant()).await {
            Ok(scope) => Some(scope),
            Err(Error::Forbidden | Error::Unauthorized) => None,
            Err(error) => return Err(error.into()),
        };
        // The owner's preview can inspect fields beyond the conditions.
        let restricted = scope.impersonator().is_some()
            || !bilibi_authz::fields::restricted(&**client, scope)
                .await?
                .is_empty();
        let authority = bilibi_authz::effective_grants(&**client, scope).await?;
        let mut results = Vec::new();
        let mut bytes = 0usize;
        let mut examined = 0i32;
        for row in rows.iter().take(limit as usize) {
            examined += 1;
            let version: i32 = row.get(1);
            if !trigger.definition.on.versions.contains(&(version as u16)) {
                continue;
            }
            let payload: Value = row.get(2);
            let subject = match payload.pointer(trigger.definition.subject) {
                Some(Value::Number(n)) => n.to_string(),
                Some(Value::String(s)) if !s.is_empty() && s.len() <= 200 => s.clone(),
                _ => continue,
            };
            // The reader and principal see the same snapshot. Evaluate the
            // principal's facts, as the live engine does, only after the reader
            // has proved access to that record and every condition field.
            let tx = (**client)
                .build_transaction()
                .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
                .start()
                .await
                .map_err(Error::from)?;
            let reader = self
                .read_facts(&tx, scope, trigger.definition.table, &subject, &fields)
                .await?;
            if reader.is_err() {
                tx.commit().await.map_err(Error::from)?;
                continue;
            }
            let Some(principal) = principal.as_ref() else {
                tx.commit().await.map_err(Error::from)?;
                let item = json!({"event_id":row.get::<_, i64>(0), "recorded_at":row.get::<_, String>(3), "subject":subject, "state":"automation_unavailable"});
                let size = serde_json::to_vec(&item)
                    .map_err(|_| Error::Internal)?
                    .len();
                if bytes + size > 512 * 1024 - 4096 {
                    truncated = true;
                    break;
                }
                bytes += size;
                results.push(item);
                continue;
            };
            let automated = self
                .read_facts(&tx, principal, trigger.definition.table, &subject, &fields)
                .await?;
            tx.commit().await.map_err(Error::from)?;
            let Ok((facts, types)) = automated else {
                let item = json!({"event_id":row.get::<_, i64>(0), "recorded_at":row.get::<_, String>(3), "subject":subject, "state":"automation_read_refused"});
                let size = serde_json::to_vec(&item)
                    .map_err(|_| Error::Internal)?
                    .len();
                if bytes + size > 512 * 1024 - 4096 {
                    truncated = true;
                    break;
                }
                bytes += size;
                results.push(item);
                continue;
            };
            let (matched, outcomes) = definition.evaluate(&facts, &|path| {
                types
                    .iter()
                    .find(|(p, _)| p == path)
                    .map_or(FieldType::String, |(_, ty)| *ty)
            });
            let mut effects = Vec::new();
            if matched {
                let revision = facts
                    .get(trigger.definition.version)
                    .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
                    .ok_or(Error::Invalid)?;
                for call in &definition.actions {
                    let action = self.action(&call.operation).ok_or(Error::Invalid)?;
                    let mut input = crate::review::bind(
                        &action.input,
                        action.definition.subject,
                        &subject,
                        &call.input,
                    )
                    .ok_or(Error::Invalid)?;
                    input[action.definition.version] = json!(revision);
                    let state = match self
                        .jobs
                        .preview_principal(&mut **client, principal, action.key, &input)
                        .await
                    {
                        Ok(_) => "allowed",
                        Err(error) => {
                            crate::run::refusal(error)?;
                            "refused"
                        }
                    };
                    // Never disclose an unrestricted owner preview. Its outcome
                    // may also reveal a restricted field or out-of-reach record.
                    let visible = !restricted
                        && authority.grant_all
                        && authority.direct_denied.is_empty()
                        && std::iter::once(action.capability)
                            .chain(action.definition.approval_capabilities.iter().copied())
                            .all(|key| authority.reaches(key, bilibi_authz::Reach::All));
                    effects.push(json!({"operation":action.key,"title":action.title,"preview":if visible { state } else { "restricted" }}));
                }
            }
            let shown_facts: serde_json::Map<String, Value> = facts
                .into_iter()
                .map(|(key, value)| {
                    let value = if serde_json::to_vec(&value).map_or(true, |v| v.len() > 4096) {
                        json!({"omitted":"value exceeds 4 KiB"})
                    } else {
                        value
                    };
                    (key, value)
                })
                .collect();
            let item = json!({"event_id":row.get::<_, i64>(0), "recorded_at":row.get::<_, String>(3), "subject":subject,
                "state":if matched {"match"} else {"near_miss"}, "current_record":{"facts":shown_facts,"condition_outcomes":outcomes}, "would_be_effects":effects});
            let size = serde_json::to_vec(&item)
                .map_err(|_| Error::Internal)?
                .len();
            if bytes + size > 512 * 1024 - 4096 {
                truncated = true;
                break;
            }
            bytes += size;
            results.push(item);
        }
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                scope.mutation_context("watchers.trial.finished"),
            )
            .await?;
        bilibi_authz::authorize_key(tx.sql(), scope, crate::MANAGE).await?;
        tx.mutate(move |sql| Box::pin(async move {
            sql.execute("UPDATE base_watcher_trials SET examined=$3,completed_at=now() WHERE tenant_id=$1 AND id=$2", &[&tenant,&trial_id,&examined]).await?;
            Ok(((), vec![Change { kind:"base/watchers.trial".into(), id:trial_id, old:json!({"completed":false}), new:json!({"completed":true,"examined":examined}) }]))
        })).await?;
        tx.commit().await?;
        Ok(
            json!({"trial_id":trial_id,"window_days":days,"truncated":truncated,"event_evidence":"event_id, recorded_at and subject identify the recorded event; record fields were read today", "coverage":"Retained, unerased events only; retention or erasure can leave gaps.", "results":results}),
        )
    }
}
