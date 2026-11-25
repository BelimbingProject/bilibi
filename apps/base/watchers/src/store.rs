//! The `Watchers` facade and the reads the page and agents share: the tenant's
//! watchers, one watcher with its versions and receipts, and what watchers may use
//! here. Writing a watcher is `review.rs`.
use crate::catalog::{OfferedAction, OfferedLookup, OfferedTrigger, Offers};
use crate::definition;
use crate::{
    ACTIONS_PER_RUN, ACTIVE_PER_TRIGGER, CHAIN_DEPTH, CONDITIONS, EXAMPLES, FACTS_READ,
    MAX_ATTEMPTS, VALUES_PER_LIST, VIEW, WATCHER_ACTIONS,
};
use bilibi_agent_api::job::JobContracts;
use bilibi_authz::{SystemAuthentication, SystemScope};
use bilibi_database::{Db, Error, Result};
use bilibi_grid::Catalog;
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::{Authentication, Scope};
use jiff::Timestamp;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_postgres::GenericClient;

/// The watcher service (`base/watchers.watchers`).
pub struct Watchers {
    pub(crate) db: Db,
    pub(crate) jobs: Arc<JobContracts>,
    pub(crate) registry: Arc<bilibi_agent_api::Registry>,
    pub(crate) mutations: Arc<bilibi_database::MutationBoundary>,
    pub(crate) events: Arc<bilibi_events::Events>,
    pub(crate) queue: Arc<bilibi_queue::Queue>,
    pub(crate) settings: Arc<Settings>,
    pub(crate) authentication: Arc<Authentication>,
    pub(crate) directory: Arc<bilibi_principal_directory::PrincipalDirectory>,
    pub(crate) ai: Arc<bilibi_ai::Ai>,
    pub(crate) offers: Offers,
}

/// Where a watcher stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatcherState {
    /// No version approved yet: nothing runs.
    Draft,
    NeedsReview,
    /// An approved version runs on every new trigger event.
    On,
    /// Switched off: the approved version is kept but nothing runs.
    Off,
}
impl WatcherState {
    pub(crate) fn from_flags(approved: bool, off: bool, needs_review: bool) -> Self {
        match (approved, off, needs_review) {
            (false, _, _) => Self::Draft,
            (true, _, true) => Self::NeedsReview,
            (true, false, false) => Self::On,
            (true, true, false) => Self::Off,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::NeedsReview => "needs_review",
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

/// One watcher as lists show it.
#[derive(Clone, Debug)]
pub struct WatcherSummary {
    pub id: i64,
    pub name: String,
    pub trigger: String,
    pub trigger_title: String,
    pub state: WatcherState,
    /// The running version's number.
    pub running: Option<i32>,
    pub latest: i32,
    /// The latest version waits for approval.
    pub pending: bool,
    pub last_run: Option<Timestamp>,
    pub updated_at: Timestamp,
    pub version: i32,
}

#[derive(Clone, Debug)]
pub struct VersionView {
    pub id: i64,
    pub number: i32,
    pub definition: Value,
    pub examples: Value,
    pub explanation: Option<String>,
    pub digest: String,
    pub drafted_via: String,
    pub drafted_by: String,
    pub drafted_at: Timestamp,
    pub approved_by: Option<String>,
    pub approved_at: Option<Timestamp>,
}

#[derive(Clone, Debug)]
pub struct EffectView {
    pub position: i32,
    pub operation: String,
    pub state: String,
    pub reason: Option<String>,
    /// The owner's result, or its own words for a refusal.
    pub result: Value,
}

/// A receipt: why the watcher matched (or not) and what it did.
#[derive(Clone, Debug)]
pub struct RunView {
    pub id: i64,
    pub watcher_id: i64,
    pub version: i32,
    pub event_id: i64,
    pub event_name: String,
    pub subject: String,
    pub depth: i32,
    pub state: String,
    pub reason: Option<String>,
    pub facts: Value,
    pub outcomes: Value,
    /// The AI observation the conditions read, or null when none was asked.
    pub observation: Value,
    pub created_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub effects: Vec<EffectView>,
}

#[derive(Clone, Debug)]
pub struct WatcherView {
    pub summary: WatcherSummary,
    pub versions: Vec<VersionView>,
    pub runs: Vec<RunView>,
}

/// A stored instant (UTC) as a `Timestamp`.
fn time(row: &tokio_postgres::Row, i: usize) -> Timestamp {
    time_opt(row, i).unwrap_or(Timestamp::UNIX_EPOCH)
}
fn time_opt(row: &tokio_postgres::Row, i: usize) -> Option<Timestamp> {
    row.get::<_, Option<std::time::SystemTime>>(i)
        .and_then(|at| Timestamp::try_from(at).ok())
}

const SUMMARY: &str = "SELECT r.id,r.name,r.trigger,(r.active_version_id IS NOT NULL),(r.disabled_at IS NOT NULL),a.number,l.number,(l.approved_at IS NULL),(SELECT max(created_at) FROM base_watcher_runs u WHERE u.tenant_id=r.tenant_id AND u.watcher_id=r.id),r.updated_at,r.version,r.needs_review FROM base_watchers r LEFT JOIN base_watcher_versions a ON a.tenant_id=r.tenant_id AND a.id=r.active_version_id JOIN LATERAL (SELECT number,approved_at FROM base_watcher_versions v WHERE v.tenant_id=r.tenant_id AND v.watcher_id=r.id ORDER BY number DESC LIMIT 1) l ON true";

impl Watchers {
    pub(crate) fn trigger(&self, event: &str) -> Option<&OfferedTrigger> {
        self.offers.triggers.get(event)
    }
    pub(crate) fn action(&self, key: &str) -> Option<&OfferedAction> {
        self.offers.actions.get(key)
    }
    pub(crate) fn lookup(&self, key: &str) -> Option<&OfferedLookup> {
        self.offers.lookups.get(key)
    }
    fn summary(&self, row: &tokio_postgres::Row) -> WatcherSummary {
        let trigger: String = row.get(2);
        let (approved, off): (bool, bool) = (row.get(3), row.get(4));
        WatcherSummary {
            id: row.get(0),
            name: row.get(1),
            trigger_title: self
                .trigger(&trigger)
                .map_or_else(|| trigger.clone(), |t| t.definition.title.to_owned()),
            trigger,
            state: WatcherState::from_flags(approved, off, row.get(11)),
            running: row.get(5),
            latest: row.get(6),
            pending: row.get(7),
            last_run: time_opt(row, 8),
            updated_at: time(row, 9),
            version: row.get(10),
        }
    }

    /// A tenant setting as the scope reads it, within its bounds.
    pub(crate) async fn limit(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        key: &str,
    ) -> Result<i64> {
        self.settings
            .get(client, scope, Target::Tenant, key)
            .await?
            .value
            .as_i64()
            .ok_or(Error::Invalid)
    }

    pub(crate) async fn check_limits(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        definition: &definition::Definition,
        examples: usize,
        version_field: &str,
    ) -> Result<definition::Problems> {
        let mut problems = Vec::new();
        for (field, count, key) in [
            ("if.conditions", definition.conditions.len(), CONDITIONS),
            ("do", definition.actions().count(), WATCHER_ACTIONS),
            ("do", definition.actions().count(), ACTIONS_PER_RUN),
            ("examples", examples, EXAMPLES),
            (
                "if.conditions",
                definition.fact_fields(version_field, true).len(),
                FACTS_READ,
            ),
        ] {
            let most = self.limit(client, scope, key).await?;
            if count as i64 > most {
                problems.push((field.into(), format!("at most {most} are allowed by {key}")));
            }
        }
        let most = self.limit(client, scope, VALUES_PER_LIST).await?;
        for (i, condition) in definition.conditions.iter().enumerate() {
            if condition
                .value
                .as_array()
                .is_some_and(|values| values.len() as i64 > most)
            {
                problems.push((
                    format!("if.conditions.{i}.values"),
                    format!("at most {most} values per list are allowed"),
                ));
            }
        }
        Ok(problems)
    }

    /// The `watchers` principal sealed in the scope's tenant, as runs see it.
    pub(crate) async fn principal(
        &self,
        client: &(impl GenericClient + Sync),
        tenant: i64,
    ) -> Result<Scope> {
        let system = SystemScope::rehydrate(client, tenant, Some(crate::PRINCIPAL)).await?;
        self.authentication.for_system(client, &system).await
    }

    /// The tenant's watchers, newest change first, matching `search` in the name.
    pub async fn list(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        search: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<WatcherSummary>, u64)> {
        bilibi_authz::authorize_key(client, scope, VIEW).await?;
        let pattern = format!("%{}%", search.replace(['%', '_', '\\'], ""));
        let total: i64 = client
            .query_one(
                "SELECT count(*) FROM base_watchers WHERE tenant_id=$1 AND name ILIKE $2",
                &[&scope.tenant(), &pattern],
            )
            .await?
            .get(0);
        let rows = client
            .query(
                &format!("{SUMMARY} WHERE r.tenant_id=$1 AND r.name ILIKE $2 ORDER BY r.updated_at DESC, r.id DESC LIMIT $3 OFFSET $4"),
                &[&scope.tenant(), &pattern, &limit, &offset],
            )
            .await?;
        Ok((
            rows.iter().map(|row| self.summary(row)).collect(),
            total as u64,
        ))
    }

    /// One watcher with its newest 20 versions, the running version and every
    /// version referenced by its latest 25 receipts, all newest first.
    pub async fn get(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        id: i64,
    ) -> Result<WatcherView> {
        bilibi_authz::authorize_key(client, scope, VIEW).await?;
        let tenant = scope.tenant();
        let row = client
            .query_opt(
                &format!("{SUMMARY} WHERE r.tenant_id=$1 AND r.id=$2"),
                &[&tenant, &id],
            )
            .await?
            .ok_or(Error::NotFound)?;
        let summary = self.summary(&row);
        let runs = self.runs(client, scope, Some(id), 25).await?;
        let referenced: Vec<i32> = runs
            .iter()
            .map(|run| run.version)
            .chain(summary.running)
            .collect();
        let rows = client.query(
            "SELECT id,number,definition,examples,explanation,digest,drafted_via,drafted_by,drafted_at,approved_by,approved_at FROM base_watcher_versions WHERE tenant_id=$1 AND watcher_id=$2 AND (id IN (SELECT id FROM base_watcher_versions WHERE tenant_id=$1 AND watcher_id=$2 ORDER BY number DESC LIMIT 20) OR number=ANY($3)) ORDER BY number DESC",
            &[&tenant, &id, &referenced],
        ).await?;
        let mut people = Vec::new();
        for row in &rows {
            for user in [Some(row.get::<_, i64>(7)), row.get::<_, Option<i64>>(9)]
                .into_iter()
                .flatten()
            {
                if let Ok(person) = bilibi_principal_directory::PrincipalRef::new(
                    bilibi_principal_directory::PrincipalKind::User,
                    user,
                ) {
                    people.push(person);
                }
            }
        }
        let named = self
            .directory
            .rank(
                client,
                scope,
                &people,
                &bilibi_principal_directory::Query::default(),
            )
            .await
            .ok();
        let name = |user: i64| {
            bilibi_principal_directory::PrincipalRef::new(
                bilibi_principal_directory::PrincipalKind::User,
                user,
            )
            .map(|person| {
                named
                    .as_ref()
                    .map_or_else(|| person.durable_label(), |n| n.label(person))
            })
            .unwrap_or_default()
        };
        let versions = rows
            .iter()
            .map(|row| VersionView {
                id: row.get(0),
                number: row.get(1),
                definition: row.get(2),
                examples: row.get(3),
                explanation: row.get(4),
                digest: row.get(5),
                drafted_via: row.get(6),
                drafted_by: name(row.get(7)),
                drafted_at: time(row, 8),
                approved_by: row.get::<_, Option<i64>>(9).map(name),
                approved_at: time_opt(row, 10),
            })
            .collect();
        Ok(WatcherView {
            summary,
            versions,
            runs,
        })
    }

    /// The tenant's latest receipts, of one watcher or of all, newest first.
    pub async fn runs(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        watcher: Option<i64>,
        limit: i64,
    ) -> Result<Vec<RunView>> {
        bilibi_authz::authorize_key(client, scope, VIEW).await?;
        let tenant = scope.tenant();
        let rows = client.query(
            "SELECT u.id,u.watcher_id,v.number,u.event_id,u.event_name,u.subject,u.depth,u.state,u.reason,u.facts,u.outcomes,u.created_at,u.finished_at,v.definition,v.catalog_digest,u.observation FROM base_watcher_runs u JOIN base_watcher_versions v ON v.tenant_id=u.tenant_id AND v.id=u.version_id WHERE u.tenant_id=$1 AND ($2::bigint IS NULL OR u.watcher_id=$2) ORDER BY u.id DESC LIMIT $3",
            &[&tenant, &watcher, &limit.clamp(1, 200)],
        ).await?;
        let ids: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
        let effects = client.query(
            "SELECT run_id,position,operation,state,reason,result FROM base_watcher_effects WHERE tenant_id=$1 AND run_id=ANY($2) ORDER BY run_id,position",
            &[&tenant, &ids],
        ).await?;
        let definitions: Vec<Option<definition::Definition>> = rows
            .iter()
            .map(|row| definition::Definition::parse(&row.get::<_, Value>(13), &mut Vec::new()))
            .collect();
        let custom = self
            .custom_tables(client, scope, definitions.iter().flatten())
            .await?;
        let mut readable = std::collections::BTreeMap::new();
        for (row, definition) in rows.iter().zip(&definitions) {
            let event: &str = row.get(4);
            let facts: Option<Value> = row.get(9);
            let fields: Vec<String> = facts
                .as_ref()
                .and_then(Value::as_object)
                .map(|facts| facts.keys().cloned().collect())
                .unwrap_or_default();
            let compatible = definition.as_ref().is_some_and(|definition| {
                let saved: Option<String> = row.get(14);
                saved.is_some() && saved == self.catalog_digest(event, definition, &custom)
            });
            let allowed = match self.trigger(event).filter(|_| compatible) {
                Some(trigger) => {
                    // A script's or an observation's outcome tells about the
                    // fields it read, so the reader must read them all.
                    let fields = match definition {
                        Some(definition)
                            if definition.scripts().next().is_some()
                                || definition.observe.is_some() =>
                        {
                            definition.fact_fields(trigger.definition.version, true)
                        }
                        _ => fields,
                    };
                    self.read_facts(client, scope, trigger.definition.table, row.get(5), &fields)
                        .await?
                        .is_ok()
                }
                None => false,
            };
            let scripted = definition.as_ref().is_none_or(|definition| {
                definition.scripts().next().is_some() || definition.observe.is_some()
            });
            readable.insert(row.get::<_, i64>(0), (allowed, scripted));
        }
        let hidden = json!({"restricted": true});
        Ok(rows
            .iter()
            .map(|row| {
                let (shown, scripted) = readable
                    .get(&row.get::<_, i64>(0))
                    .copied()
                    .unwrap_or_default();
                let id: i64 = row.get(0);
                let hidden_outcome = scripted && !shown;
                RunView {
                    id,
                    watcher_id: row.get(1),
                    version: row.get(2),
                    event_id: row.get(3),
                    event_name: row.get(4),
                    subject: if shown { row.get(5) } else { String::new() },
                    depth: row.get(6),
                    state: if hidden_outcome {
                        "restricted".into()
                    } else {
                        row.get(7)
                    },
                    reason: if hidden_outcome { None } else { row.get(8) },
                    facts: if shown {
                        row.get::<_, Option<Value>>(9).unwrap_or(Value::Null)
                    } else {
                        hidden.clone()
                    },
                    outcomes: if shown {
                        row.get::<_, Option<Value>>(10).unwrap_or(Value::Null)
                    } else {
                        hidden.clone()
                    },
                    observation: match row.get::<_, Option<Value>>(15) {
                        Some(observation) if shown => observation,
                        Some(_) => hidden.clone(),
                        None => Value::Null,
                    },
                    created_at: time(row, 11),
                    finished_at: if hidden_outcome {
                        None
                    } else {
                        time_opt(row, 12)
                    },
                    effects: effects
                        .iter()
                        .filter(|e| (shown || !scripted) && e.get::<_, i64>(0) == id)
                        .map(|e| EffectView {
                            position: e.get(1),
                            operation: e.get(2),
                            state: e.get(3),
                            reason: e.get(4),
                            result: if shown {
                                e.get::<_, Option<Value>>(5).unwrap_or(Value::Null)
                            } else {
                                hidden.clone()
                            },
                        })
                        .collect(),
                }
            })
            .collect())
    }

    pub(crate) async fn read_facts(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        table: &str,
        subject: &str,
        fields: &[String],
    ) -> Result<
        std::result::Result<
            (
                serde_json::Map<String, Value>,
                Vec<(String, bilibi_grid::FieldType)>,
            ),
            &'static str,
        >,
    > {
        let read = async {
            let catalog = Catalog::for_scope(client, scope)
                .await
                .map_err(Error::from)?;
            let Some(root) = catalog.table(table) else {
                return Ok(Err("read_refused"));
            };
            let mut types = Vec::new();
            let mut expressions = Vec::new();
            let mut reached = Vec::new();
            for path in fields {
                let column = match catalog.resolve(root, path) {
                    Ok(column)
                        if column.agg.is_none()
                            && column.many.is_none()
                            && column.field.is_some_and(|field| {
                                !catalog.masked(column.table, field.id)
                                    && (crate::review::custom_path(path).is_some()
                                        || catalog
                                            .table(column.table)
                                            .is_some_and(|owner| !owner.is_custom(field.id)))
                            }) =>
                    {
                        column
                    }
                    _ => {
                        let closed = crate::review::custom_path(path)
                            .and_then(|(link, _)| {
                                let grid = bilibi_grid::installed().ok()?;
                                crate::review::custom_owner(grid, grid.table(table)?, Some(link?))
                            })
                            .is_some_and(|(_, owner)| catalog.table(owner.id()).is_none());
                        return Ok(Err(if closed {
                            "read_refused"
                        } else {
                            "restricted_field"
                        }));
                    }
                };
                reached.push(
                    bilibi_grid::scalar_access(&catalog, table, path)
                        .map_err(|_| Error::Invalid)?,
                );
                types.push((path.clone(), column.ty));
                let expression = bilibi_grid::scalar_expression(&catalog, table, path)
                    .map_err(|_| Error::Invalid)?;
                let expression = if matches!(
                    column.ty,
                    bilibi_grid::FieldType::Integer
                        | bilibi_grid::FieldType::Decimal
                        | bilibi_grid::FieldType::Float
                ) {
                    format!("({expression})::text")
                } else {
                    expression
                };
                expressions.push(format!("to_jsonb({expression})"));
            }
            let key = bilibi_grid::scalar_expression(&catalog, table, root.key())
                .map_err(|_| Error::Invalid)?;
            let values = if expressions.is_empty() {
                "NULL::jsonb".into()
            } else {
                expressions.join(",")
            };
            let reachable = if reached.is_empty() {
                "true".into()
            } else {
                reached.join(" AND ")
            };
            let shape = client
                .prepare(&format!("SELECT {key} FROM {} r", root.scoped_source()))
                .await?;
            let key_type = shape.columns()[0].type_();
            let cast = format!(
                "\"{}\".\"{}\"",
                key_type.schema().replace('"', "\"\""),
                key_type.name().replace('"', "\"\"")
            );
            let statement = format!("SELECT {values} FROM {} r WHERE {key}=$4::text::{cast} AND $1::bigint IS NOT NULL AND $2::bigint IS NOT NULL AND $3::bigint IS NOT DISTINCT FROM $3::bigint AND ({reachable}) LIMIT 1", root.scoped_source());
            let company = scope.current_company().map(|company| company.get());
            let explain: Value = client
                .query_one(
                    &format!("EXPLAIN (FORMAT JSON) {statement}"),
                    &[&scope.tenant(), &scope.user(), &company, &subject],
                )
                .await?
                .get(0);
            let cost = explain[0]["Plan"]["Total Cost"]
                .as_f64()
                .ok_or(Error::Internal)?;
            if !cost.is_finite() || cost > bilibi_grid::MAX_ESTIMATED_COST {
                return Err(Error::Refused(
                    "The watcher's record read exceeds its cost bound".into(),
                ));
            }
            let Some(row) = client
                .query_opt(
                    &statement,
                    &[&scope.tenant(), &scope.user(), &company, &subject],
                )
                .await?
            else {
                return Ok(Err("subject_unavailable"));
            };
            let facts = fields
                .iter()
                .enumerate()
                .map(|(i, path)| {
                    (
                        path.clone(),
                        row.get::<_, Option<Value>>(i).unwrap_or(Value::Null),
                    )
                })
                .collect();
            Ok(Ok((facts, types)))
        };
        match tokio::time::timeout(
            std::time::Duration::from_millis(u64::from(
                bilibi_grid::Bounds::default().statement_ms,
            )),
            read,
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                client
                    .client()
                    .cancel_token()
                    .cancel_query(tokio_postgres::NoTls)
                    .await?;
                Err(Error::Refused(
                    "The watcher's record read exceeded its time bound".into(),
                ))
            }
        }
    }

    /// What a watcher may use here: the triggers with the fields the reader may
    /// read, the actions with their inputs, and the tenant's limits.
    pub async fn vocabulary(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
    ) -> Result<Value> {
        bilibi_authz::authorize_key(client, scope, VIEW).await?;
        let catalog = Catalog::for_scope(client, scope)
            .await
            .map_err(Error::from)?;
        let catalog = &catalog;
        let triggers: Vec<Value> = self
            .offers
            .triggers
            .values()
            .map(|t| {
                let fields: Vec<Value> = catalog
                    .table(t.definition.table)
                    .map(|table| {
                        // A tenant custom field is offered only when the tenant
                        // opted it into watcher conditions.
                        let own = table
                            .visible_fields()
                            .filter(|f| !table.is_custom(f.id) || table.custom_watcher_condition(f.id))
                            .map(|f| json!({"field": f.id, "label": table.field_label(f), "type": format!("{:?}", f.ty).to_lowercase(), "values": f.values, "masked": catalog.masked(table.id(), f.id), "custom": table.is_custom(f.id)}));
                        // So are the offered custom fields of a record one
                        // link away (`company.cf_segment`, #862), when the
                        // reader may read that record's table.
                        let linked = table
                            .links()
                            .iter()
                            .filter(|link| link.kind() == bilibi_grid::LinkKind::One)
                            .filter_map(|link| Some((link, catalog.table(link.to())?)))
                            .flat_map(|(link, target)| {
                                target
                                    .visible_fields()
                                    .filter(|f| target.custom_watcher_condition(f.id))
                                    .map(move |f| json!({"field": format!("{}.{}", link.id(), f.id), "label": format!("{} \u{203a} {}", link.label(), target.field_label(f)), "type": format!("{:?}", f.ty).to_lowercase(), "values": f.values, "masked": catalog.masked(target.id(), f.id), "custom": true}))
                            });
                        own.chain(linked).collect()
                    })
                    .unwrap_or_default();
                json!({"trigger": t.event, "title": t.definition.title, "table": t.definition.table, "fields": fields,
                    "readable": catalog.table(t.definition.table).is_some()})
            })
            .collect();
        let actions: Vec<Value> = self
            .offers
            .actions
            .values()
            .map(|a| {
                let mut input = a.input.clone();
                if let Some(properties) = input["properties"].as_object_mut() {
                    properties.remove(a.definition.subject);
                    properties.remove(a.definition.version);
                }
                if let Some(required) = input["required"].as_array_mut() {
                    required.retain(|r| r.as_str() != Some(a.definition.subject) && r.as_str() != Some(a.definition.version));
                }
                json!({"operation": a.key, "title": a.title, "summary": a.summary, "table": a.definition.table,
                    "subject": a.definition.subject, "records": a.definition.records, "local_script": a.local_script, "input": input,
                    "outbound": a.outbound.map(|o| json!({"destination": o.destination, "data": o.data}))})
            })
            .collect();
        let mut lookups = Vec::new();
        for lookup in self.offers.lookups.values() {
            lookups.push(json!({
                "operation": lookup.key,
                "title": lookup.title,
                "input": lookup.input,
                "output": lookup.output,
                "available": bilibi_authz::decide(client, scope, lookup.capability).await?.allowed,
            }));
        }
        Ok(json!({
            "triggers": triggers,
            "actions": actions,
            "lookups": lookups,
            "operators": definition::Op::ALL,
            "observation": {
                "facts": [
                    {"field": crate::observe::LABEL, "type": "enum", "values": "the draft's observe.labels"},
                ],
                "available_to_automation": match self.principal(client, scope.tenant()).await {
                    Ok(principal) => self.observation_available(client, &principal).await?.map_or_else(|| json!(true), |reason| json!(reason)),
                    Err(Error::Unauthorized) => return Err(Error::Unauthorized),
                    Err(_) => json!("automation_unavailable"),
                },
            },
            "limits": {
                "actions_per_run": self.limit(client, scope, ACTIONS_PER_RUN).await?,
                "chain_depth": self.limit(client, scope, CHAIN_DEPTH).await?,
                "active_per_trigger": self.limit(client, scope, ACTIVE_PER_TRIGGER).await?,
                "conditions": self.limit(client, scope, CONDITIONS).await?,
                "actions": self.limit(client, scope, WATCHER_ACTIONS).await?,
                "examples": self.limit(client, scope, EXAMPLES).await?,
                "values_per_list": self.limit(client, scope, VALUES_PER_LIST).await?,
                "facts_read": self.limit(client, scope, FACTS_READ).await?,
                "max_attempts": self.limit(client, scope, MAX_ATTEMPTS).await?,
                "trial_days": self.limit(client, scope, crate::TRIAL_DAYS).await?,
                "trial_events": self.limit(client, scope, crate::TRIAL_EVENTS).await?,
                "ai_observations_per_day": self.limit(client, scope, crate::AI_OBSERVATIONS_PER_DAY).await?,
                "trial_observations": self.limit(client, scope, crate::TRIAL_OBSERVATIONS).await?,
            },
        }))
    }
}
