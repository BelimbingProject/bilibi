//! The `Rules` facade and the reads the page and agents share: the tenant's
//! rules, one rule with its versions and receipts, and what rules may use
//! here. Writing a rule is `review.rs`.
use crate::catalog::{OfferedAction, OfferedTrigger, Offers};
use crate::definition;
use crate::{ACTIONS_PER_RUN, ACTIVE_PER_TRIGGER, CHAIN_DEPTH, VIEW};
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

/// The rule service (`base/rules.rules`).
pub struct Rules {
    pub(crate) db: Db,
    pub(crate) jobs: Arc<JobContracts>,
    pub(crate) events: Arc<bilibi_events::Events>,
    pub(crate) queue: Arc<bilibi_queue::Queue>,
    pub(crate) settings: Arc<Settings>,
    pub(crate) authentication: Arc<Authentication>,
    pub(crate) directory: Arc<bilibi_principal_directory::PrincipalDirectory>,
    pub(crate) offers: Offers,
}

/// Where a rule stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuleState {
    /// No version approved yet: nothing runs.
    Draft,
    /// An approved version runs on every new trigger event.
    On,
    /// Switched off: the approved version is kept but nothing runs.
    Off,
}
impl RuleState {
    pub fn key(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

/// One rule as lists show it.
#[derive(Clone, Debug)]
pub struct RuleSummary {
    pub id: i64,
    pub name: String,
    pub trigger: String,
    pub trigger_title: String,
    pub state: RuleState,
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

/// A receipt: why the rule matched (or not) and what it did.
#[derive(Clone, Debug)]
pub struct RunView {
    pub id: i64,
    pub rule_id: i64,
    pub version: i32,
    pub event_id: i64,
    pub event_name: String,
    pub subject: String,
    pub depth: i32,
    pub state: String,
    pub reason: Option<String>,
    pub facts: Value,
    pub outcomes: Value,
    pub created_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub effects: Vec<EffectView>,
}

#[derive(Clone, Debug)]
pub struct RuleView {
    pub summary: RuleSummary,
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

const SUMMARY: &str = "SELECT r.id,r.name,r.trigger,(r.active_version_id IS NOT NULL),(r.disabled_at IS NOT NULL),a.number,l.number,(l.approved_at IS NULL),(SELECT max(created_at) FROM base_rule_runs u WHERE u.tenant_id=r.tenant_id AND u.rule_id=r.id),r.updated_at,r.version FROM base_rules r LEFT JOIN base_rule_versions a ON a.tenant_id=r.tenant_id AND a.id=r.active_version_id JOIN LATERAL (SELECT number,approved_at FROM base_rule_versions v WHERE v.tenant_id=r.tenant_id AND v.rule_id=r.id ORDER BY number DESC LIMIT 1) l ON true";

impl Rules {
    pub(crate) fn trigger(&self, event: &str) -> Option<&OfferedTrigger> {
        self.offers.triggers.get(event)
    }
    pub(crate) fn action(&self, key: &str) -> Option<&OfferedAction> {
        self.offers.actions.get(key)
    }
    fn summary(&self, row: &tokio_postgres::Row) -> RuleSummary {
        let trigger: String = row.get(2);
        let (approved, off): (bool, bool) = (row.get(3), row.get(4));
        RuleSummary {
            id: row.get(0),
            name: row.get(1),
            trigger_title: self
                .trigger(&trigger)
                .map_or_else(|| trigger.clone(), |t| t.definition.title.to_owned()),
            trigger,
            state: match (approved, off) {
                (false, _) => RuleState::Draft,
                (true, false) => RuleState::On,
                (true, true) => RuleState::Off,
            },
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

    /// The `rules` principal sealed in the scope's tenant, as runs see it.
    pub(crate) async fn principal(
        &self,
        client: &(impl GenericClient + Sync),
        tenant: i64,
    ) -> Result<Scope> {
        let system = SystemScope::rehydrate(client, tenant, Some(crate::PRINCIPAL)).await?;
        self.authentication.for_system(client, &system).await
    }

    /// The tenant's rules, newest change first, matching `search` in the name.
    pub async fn list(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        search: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<RuleSummary>, u64)> {
        bilibi_authz::authorize_key(client, scope, VIEW).await?;
        let pattern = format!("%{}%", search.replace(['%', '_', '\\'], ""));
        let total: i64 = client
            .query_one(
                "SELECT count(*) FROM base_rules WHERE tenant_id=$1 AND name ILIKE $2",
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

    /// One rule with its versions (newest first) and its latest receipts.
    pub async fn get(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        id: i64,
    ) -> Result<RuleView> {
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
        let rows = client.query(
            "SELECT id,number,definition,examples,explanation,digest,drafted_via,drafted_by,drafted_at,approved_by,approved_at FROM base_rule_versions WHERE tenant_id=$1 AND rule_id=$2 ORDER BY number DESC LIMIT 20",
            &[&tenant, &id],
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
        let runs = self.runs(client, scope, Some(id), 25).await?;
        Ok(RuleView {
            summary,
            versions,
            runs,
        })
    }

    /// The tenant's latest receipts, of one rule or of all, newest first.
    pub async fn runs(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        rule: Option<i64>,
        limit: i64,
    ) -> Result<Vec<RunView>> {
        bilibi_authz::authorize_key(client, scope, VIEW).await?;
        let tenant = scope.tenant();
        let rows = client.query(
            "SELECT u.id,u.rule_id,v.number,u.event_id,u.event_name,u.subject,u.depth,u.state,u.reason,u.facts,u.outcomes,u.created_at,u.finished_at FROM base_rule_runs u JOIN base_rule_versions v ON v.tenant_id=u.tenant_id AND v.id=u.version_id WHERE u.tenant_id=$1 AND ($2::bigint IS NULL OR u.rule_id=$2) ORDER BY u.id DESC LIMIT $3",
            &[&tenant, &rule, &limit.clamp(1, 200)],
        ).await?;
        let ids: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
        let effects = client.query(
            "SELECT run_id,position,operation,state,reason,result FROM base_rule_effects WHERE tenant_id=$1 AND run_id=ANY($2) ORDER BY run_id,position",
            &[&tenant, &ids],
        ).await?;
        // A receipt's facts and outcomes are the record's values: shown only
        // to a reader who may read the trigger's records themselves.
        let grid = bilibi_grid::installed()?;
        let mut readable: std::collections::BTreeMap<String, bool> = Default::default();
        for row in &rows {
            let event: String = row.get(4);
            if readable.contains_key(&event) {
                continue;
            }
            let capability = self
                .trigger(&event)
                .and_then(|t| grid.table(t.definition.table))
                .map(|table| table.capability());
            let allowed = match capability {
                Some(key) => bilibi_authz::decide(client, scope, key).await?.allowed,
                None => false,
            };
            readable.insert(event, allowed);
        }
        let hidden = json!({"restricted": true});
        Ok(rows
            .iter()
            .map(|row| {
                let shown = readable
                    .get(row.get::<_, &str>(4))
                    .copied()
                    .unwrap_or(false);
                let id: i64 = row.get(0);
                RunView {
                    id,
                    rule_id: row.get(1),
                    version: row.get(2),
                    event_id: row.get(3),
                    event_name: row.get(4),
                    subject: row.get(5),
                    depth: row.get(6),
                    state: row.get(7),
                    reason: row.get(8),
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
                    created_at: time(row, 11),
                    finished_at: time_opt(row, 12),
                    effects: effects
                        .iter()
                        .filter(|e| e.get::<_, i64>(0) == id)
                        .map(|e| EffectView {
                            position: e.get(1),
                            operation: e.get(2),
                            state: e.get(3),
                            reason: e.get(4),
                            result: e.get::<_, Option<Value>>(5).unwrap_or(Value::Null),
                        })
                        .collect(),
                }
            })
            .collect())
    }

    /// What a rule may use here: the triggers with the fields the reader may
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
        let triggers: Vec<Value> = self
            .offers
            .triggers
            .values()
            .map(|t| {
                let fields: Vec<Value> = catalog
                    .table(t.definition.table)
                    .map(|table| {
                        table
                            .visible_fields()
                            .map(|f| json!({"field": f.id, "label": f.label, "type": format!("{:?}", f.ty).to_lowercase(), "values": f.values, "masked": catalog.masked(table.id(), f.id)}))
                            .collect()
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
                }
                if let Some(required) = input["required"].as_array_mut() {
                    required.retain(|r| r.as_str() != Some(a.definition.subject));
                }
                json!({"operation": a.key, "title": a.title, "summary": a.summary, "table": a.definition.table,
                    "subject": a.definition.subject, "records": a.definition.records, "input": input})
            })
            .collect();
        Ok(json!({
            "triggers": triggers,
            "actions": actions,
            "operators": definition::Op::ALL,
            "limits": {
                "actions_per_run": self.limit(client, scope, ACTIONS_PER_RUN).await?,
                "chain_depth": self.limit(client, scope, CHAIN_DEPTH).await?,
                "active_per_trigger": self.limit(client, scope, ACTIVE_PER_TRIGGER).await?,
                "conditions": definition::MAX_CONDITIONS,
                "examples": definition::MAX_EXAMPLES,
            },
        }))
    }
}
