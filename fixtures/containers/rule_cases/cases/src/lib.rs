//! Reference rule owner, the shape a Domain's case routing takes when a
//! tenant wants "send new urgent cases to the priority queue" without a
//! release. It owns one table (a case of a company, with a priority, an
//! amount, a margin some readers must not see and a queue), three events
//! (`opened`, `routed`, `flagged`) and three contracts (`open`, `route`,
//! `flag`). It offers the events as rule triggers and `route` and `flag` as
//! rule actions, admits the `rules` principal to those two contracts and
//! offers it the capabilities they need. A case takes tenant custom fields
//! (record type `rule_case`), so a tenant can offer one to rule conditions.
//! Mount it only to prove `base/rules`
//! (`apps/web/examples/rules-walkthrough.rs`); it is never a Platform member.
use bilibi_agent_api::{
    job::{PrincipalWrite, PRINCIPAL_WRITES},
    Channels, Definitions, Handler, Operation, OperationClient, OperationFuture, WriteHandler,
    DEFINITIONS,
};
use bilibi_authz::{
    CapabilityDefinition, OfferedCapabilities, VocabularyContribution, OFFERED_CAPABILITIES,
    VOCABULARY,
};
use bilibi_custom_fields::{CustomFields, GridSource, RecordType, GRID_SOURCES, RECORD_TYPES};
use bilibi_database::{Change, Error, MutationTransaction};
use bilibi_events::{EventType, Events, NewEvent, On, EVENTS};
use bilibi_grid::{Field, FieldType, GridContribution, LinkDecl, LinkKind, Source, TableDecl};
use bilibi_module_registry::{Contribution, Layer, Module, Provenance, Service};
use bilibi_rules::{Action, Trigger, ACTIONS, TRIGGERS};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

const MODULE: &str = "rule_cases/cases";
pub const VIEW: &str = "rule-cases.case.view";
pub const OPEN: &str = "rule-cases.case.open";
pub const ROUTE: &str = "rule-cases.case.route";
pub const FLAG: &str = "rule-cases.case.flag";
pub const OPENED: &str = "rule_cases/cases.opened";
pub const ROUTED: &str = "rule_cases/cases.routed";
pub const FLAGGED: &str = "rule_cases/cases.flagged";
pub const TABLE: &str = "rule_cases";
pub const RECORD_TYPE: &str = "rule_case";

macro_rules! provenance {
    () => {
        Provenance {
            module: MODULE,
            source: file!(),
            line: line!(),
        }
    };
}

inventory::submit! {
    Module {
        id: MODULE, layer: Layer::Domains,
        dependencies: &["base/agent_api", "base/authz", "base/custom_fields", "base/database", "base/events", "base/grid", "base/rules", "base/tenancy", "core/company"],
        migrations: &[("rule-cases-cases-001-cases", include_str!("../migrations/001-cases.sql")), ("rule-cases-cases-002-links", include_str!("../migrations/002-links.sql")), ("rule-cases-cases-003-custom-fields", include_str!("../migrations/003-custom-fields.sql"))],
        services: &[Service { id: "rule_cases/cases.events", factory: |services| {
            let events: Arc<Events> = services.get(bilibi_events::SERVICE)?;
            let _ = EVENT_STORE.set(events.clone());
            let _ = CUSTOM_FIELDS.set(services.get(bilibi_custom_fields::SERVICE)?);
            Ok(events)
        } }],
    }
}
static EVENT_STORE: OnceLock<Arc<Events>> = OnceLock::new();
static CUSTOM_FIELDS: OnceLock<Arc<CustomFields>> = OnceLock::new();
fn events() -> Result<&'static Events, Error> {
    EVENT_STORE.get().map(Arc::as_ref).ok_or(Error::Internal)
}
fn custom_fields() -> Result<&'static CustomFields, Error> {
    CUSTOM_FIELDS.get().map(Arc::as_ref).ok_or(Error::Internal)
}

inventory::submit! { Contribution::new(RECORD_TYPES, RECORD_TYPE, provenance!(), &RecordType { id: RECORD_TYPE, label: "Cases" }) }
inventory::submit! { Contribution::new(GRID_SOURCES, RECORD_TYPE, provenance!(), &GridSource { record_type: RECORD_TYPE, table: TABLE, column: "custom_fields" }) }

inventory::submit! { Contribution::new(VOCABULARY, "rule_cases/cases.vocabulary", provenance!(), &VocabularyContribution {
    domains: &["rule-cases"], verbs: &["open", "route", "flag"],
    capabilities: &[CapabilityDefinition::tenant(VIEW).with_reach(), CapabilityDefinition::tenant(OPEN), CapabilityDefinition::tenant(ROUTE), CapabilityDefinition::tenant(FLAG)],
    roles: &[], system_principals: &[],
}) }
// The rules principal may read cases and route and flag them; a tenant still
// grants each of these to it by name.
inventory::submit! { Contribution::new(OFFERED_CAPABILITIES, "rules:rule_cases/cases", provenance!(), &OfferedCapabilities {
    principal: "rules", capabilities: &[VIEW, ROUTE, FLAG],
}) }
inventory::submit! { Contribution::new(EVENTS, OPENED, provenance!(), &EventType { version: 1 }) }
inventory::submit! { Contribution::new(EVENTS, ROUTED, provenance!(), &EventType { version: 1 }) }
inventory::submit! { Contribution::new(EVENTS, FLAGGED, provenance!(), &EventType { version: 1 }) }

static GRID: GridContribution = GridContribution {
    record_fields: &[],
    tables: &[TableDecl::new("rule_cases", "Cases", VIEW,
        Source::tenant("SELECT id,title,priority,amount,margin,queue,routed,flags,company_id,created_at,version,parent_id,owner_user_id,custom_fields FROM rule_cases_cases WHERE tenant_id=$1"), "id", &[
            Field::new("id", "ID", FieldType::Integer).protected(),
            Field::new("version", "Version", FieldType::Integer).protected(),
            Field::new("parent_id", "Parent", FieldType::Integer).protected(),
            Field::new("owner_user_id", "Owner", FieldType::Integer).protected(),
            Field::new("title", "Title", FieldType::String).protected(),
            Field::choice("priority", "Priority", &["low", "normal", "urgent"]).protected(),
            Field::new("amount", "Amount", FieldType::Decimal).protected(),
            // Not protected: an operator may withhold it from some readers.
            Field::new("margin", "Margin", FieldType::Decimal),
            Field::new("queue", "Queue", FieldType::String).protected(),
            Field::new("routed", "Times routed", FieldType::Integer).protected(),
            Field::new("flags", "Flags", FieldType::Integer).protected(),
            Field::new("company_id", "Company ID", FieldType::Integer).protected(),
            Field::new("created_at", "Opened", FieldType::DateTime).protected(),
            Field::new("custom_fields", "Custom fields", FieldType::String).hidden(),
         ]).owner("owner_user_id").label_field("title").time_field("created_at").record_types(&["rule_cases/cases.case"])],
    links: &[
        LinkDecl::on("parent", "Parent", LinkKind::One, TABLE, "parent_id", TABLE, "id"),
        LinkDecl::edge("parent_edge", "Parent through an edge", LinkKind::One, TABLE, TABLE, Source::tenant("SELECT id AS from_key,parent_id AS to_key FROM rule_cases_cases WHERE tenant_id=$1 AND parent_id IS NOT NULL")),
    ],
};
bilibi_grid::contribute!("rule_cases/cases", "rule_cases/cases.grid", &GRID);

inventory::submit! { Contribution::new(TRIGGERS, OPENED, provenance!(), &Trigger {
    on: On { event: OPENED, versions: &[1] }, title: "A case is opened", table: TABLE, subject: "/case", logical_event: None, version: "version",
}) }
inventory::submit! { Contribution::new(TRIGGERS, ROUTED, provenance!(), &Trigger {
    on: On { event: ROUTED, versions: &[1] }, title: "A case is routed to a queue", table: TABLE, subject: "/case", logical_event: None, version: "version",
}) }
inventory::submit! { Contribution::new(TRIGGERS, FLAGGED, provenance!(), &Trigger {
    on: On { event: FLAGGED, versions: &[1] }, title: "A case is flagged", table: TABLE, subject: "/case", logical_event: None, version: "version",
}) }
inventory::submit! { Contribution::new(ACTIONS, "rule_cases.cases.route", provenance!(), &Action {
    title: "Send the case to a queue", table: TABLE, subject: "case_id", version: "expected_version", records: &[ROUTED], approval_capabilities: &[],
}) }
inventory::submit! { Contribution::new(ACTIONS, "rule_cases.cases.flag", provenance!(), &Action {
    title: "Flag the case", table: TABLE, subject: "case_id", version: "expected_version", records: &[FLAGGED], approval_capabilities: &[],
}) }
inventory::submit! { Contribution::new(PRINCIPAL_WRITES, "rule_cases.cases.route", provenance!(), &PrincipalWrite { principals: &["rules"] }) }
inventory::submit! { Contribution::new(PRINCIPAL_WRITES, "rule_cases.cases.flag", provenance!(), &PrincipalWrite { principals: &["rules"] }) }

fn invalid(field: &str, why: &str) -> bilibi_agent_api::Error {
    bilibi_agent_api::invalid(field, why)
}

/// A case's company must be live and not archived for any write.
async fn writable(
    sql: &(impl tokio_postgres::GenericClient + Sync),
    tenant: i64,
    company: i64,
) -> bilibi_agent_api::Result<()> {
    match bilibi_company::require_writable_in(sql, tenant, company, true).await {
        Ok(_) => Ok(()),
        Err(bilibi_company::WriteError::Archived) => {
            Err(Error::Refused("the case's company is archived".into()).into())
        }
        Err(_) => Err(invalid("company_id", "is not a company of this tenant")),
    }
}

struct Open;
impl Open {
    fn read(
        input: &Value,
    ) -> bilibi_agent_api::Result<(i64, String, String, String, Option<String>)> {
        let title = bilibi_agent_api::text_field(input, "title");
        if title.is_empty() {
            return Err(invalid("title", "is required"));
        }
        Ok((
            input["company_id"].as_i64().unwrap_or_default(),
            title,
            bilibi_agent_api::text_field(input, "priority"),
            bilibi_agent_api::text_field(input, "amount"),
            bilibi_agent_api::optional_text_field(input, "margin"),
        ))
    }
}
impl WriteHandler for Open {
    fn owner(&self) -> &'static str {
        MODULE
    }
    fn preview<'a>(
        &'a self,
        _client: &'a dyn OperationClient,
        _scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (company, title, priority, amount, _) = Self::read(input)?;
            Ok(json!({"summary": format!("Open \u{201c}{title}\u{201d} ({priority}, {amount}) for company {company}")}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            bilibi_authz::authorize_key(tx.sql(), scope, OPEN).await?;
            let (company, title, priority, amount, margin) = Self::read(input)?;
            let tenant = scope.tenant();
            let parent = input["parent_id"].as_i64();
            let owner = scope.user();
            writable(tx.sql(), tenant, company).await?;
            let patch: serde_json::Map<String, Value> = input["custom"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|pair| (bilibi_agent_api::text_field(pair, "key"), pair["value"].clone()))
                .collect();
            let custom = match custom_fields()?
                .validate_locked(tx.sql(), scope, RECORD_TYPE, &patch, &serde_json::Map::new())
                .await?
            {
                Ok(values) => Value::Object(values),
                Err(errors) => {
                    let first = errors.first().map_or("is not valid", |error| error.message.as_str());
                    return Err(invalid("custom", first));
                }
            };
            let id = tx.mutate(move |sql| Box::pin(async move {
                let id: i64 = sql.query_one(
                    "INSERT INTO rule_cases_cases(tenant_id,company_id,title,priority,amount,margin,parent_id,owner_user_id,custom_fields) VALUES($1,$2,$3,$4,$5::text::numeric,$6::text::numeric,$7,$8,$9) RETURNING id",
                    &[&tenant, &company, &title, &priority, &amount, &margin, &parent, &owner, &custom],
                ).await.map_err(|_| Error::Invalid)?.get(0);
                Ok((id, vec![Change { kind: "rule_cases/cases.case".into(), id, old: Value::Null, new: json!({"title": title, "priority": priority, "amount": amount, "company": company}) }]))
            })).await?;
            events()?
                .record(
                    tx,
                    NewEvent {
                        name: OPENED,
                        subject: &format!("case:{id}"),
                        key: &format!("opened:{id}"),
                        payload: json!({"case": id}),
                    },
                )
                .await?;
            Ok(json!({"case_id": id}).into())
        })
    }
}

/// Route or flag: both always change the case and record their event, so a
/// loop of rules over them would never settle on its own.
enum Change2 {
    Route,
    Flag,
}
impl Change2 {
    fn capability(&self) -> &'static str {
        match self {
            Self::Route => ROUTE,
            Self::Flag => FLAG,
        }
    }
}
impl WriteHandler for Change2 {
    fn owner(&self) -> &'static str {
        MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let id = input["case_id"].as_i64().unwrap_or_default();
            let row = client
                .query_opt(
                    "SELECT title FROM rule_cases_cases WHERE tenant_id=$1 AND id=$2",
                    &[&scope.tenant(), &id],
                )
                .await?
                .ok_or_else(|| invalid("case_id", "is not a case of this tenant"))?;
            let title: String = row.get(0);
            Ok(json!({"summary": match self {
                Self::Route => format!("Send \u{201c}{title}\u{201d} to {}", bilibi_agent_api::text_field(input, "queue")),
                Self::Flag => format!("Flag \u{201c}{title}\u{201d}"),
            }}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            bilibi_authz::authorize_key(tx.sql(), scope, self.capability()).await?;
            let tenant = scope.tenant();
            let id = input["case_id"].as_i64().unwrap_or_default();
            let row = tx.sql()
                .query_opt("SELECT company_id,queue,routed,flags,version FROM rule_cases_cases WHERE tenant_id=$1 AND id=$2 FOR UPDATE", &[&tenant, &id])
                .await
                .map_err(Error::from)?
                .ok_or_else(|| invalid("case_id", "is not a case of this tenant"))?;
            let (company, queue, routed, flags): (i64, Option<String>, i32, i32) =
                (row.get(0), row.get(1), row.get(2), row.get(3));
            if input["expected_version"]
                .as_i64()
                .is_some_and(|expected| expected != i64::from(row.get::<_, i32>(4)))
            {
                return Err(Error::Conflict.into());
            }
            writable(tx.sql(), tenant, company).await?;
            let (name, key, payload) = match self {
                Self::Route => {
                    let to = bilibi_agent_api::text_field(input, "queue");
                    let (old, new) = (queue.clone(), to.clone());
                    tx.mutate(move |sql| Box::pin(async move {
                        sql.execute("UPDATE rule_cases_cases SET queue=$3,routed=routed+1,version=version+1 WHERE tenant_id=$1 AND id=$2", &[&tenant, &id, &new]).await?;
                        Ok(((), vec![Change { kind: "rule_cases/cases.case".into(), id, old: json!({"queue": old}), new: json!({"queue": new}) }]))
                    })).await?;
                    (
                        ROUTED,
                        format!("routed:{id}:{}", routed + 1),
                        json!({"case": id, "queue": to}),
                    )
                }
                Self::Flag => {
                    tx.mutate(move |sql| Box::pin(async move {
                        sql.execute("UPDATE rule_cases_cases SET flags=flags+1,version=version+1 WHERE tenant_id=$1 AND id=$2", &[&tenant, &id]).await?;
                        Ok(((), vec![Change { kind: "rule_cases/cases.case".into(), id, old: json!({"flags": flags}), new: json!({"flags": flags + 1}) }]))
                    })).await?;
                    (
                        FLAGGED,
                        format!("flagged:{id}:{}", flags + 1),
                        json!({"case": id}),
                    )
                }
            };
            events()?
                .record(
                    tx,
                    NewEvent {
                        name,
                        subject: &format!("case:{id}"),
                        key: &key,
                        payload,
                    },
                )
                .await?;
            Ok(json!({"case_id": id}).into())
        })
    }
}

fn open_schema() -> Value {
    json!({"type":"object","required":["company_id","title","priority","amount"],"properties":{
        "company_id":{"type":"integer","minimum":1},
        "parent_id":{"type":"integer","minimum":1},
        "title":{"type":"string","maxLength":160},
        "priority":{"type":"string","enum":["low","normal","urgent"]},
        "amount":{"type":"string","maxLength":24,"description":"A decimal amount, such as 1500.00"},
        "margin":{"type":"string","maxLength":24},
        "custom":{"type":"array","description":"Tenant custom field values as {key, value} pairs","items":{"type":"object","required":["key"],"properties":{
            "key":{"type":"string","maxLength":40},
            "value":{"type":"string","maxLength":1000,"nullable":true}
        }}}
    }})
}
fn route_schema() -> Value {
    json!({"type":"object","required":["case_id","queue"],"properties":{
        "case_id":{"type":"integer","minimum":1},
        "expected_version":{"type":"integer","minimum":1},
        "queue":{"type":"string","maxLength":60,"description":"The queue the case goes to"}
    }})
}
fn flag_schema() -> Value {
    json!({"type":"object","required":["case_id"],"properties":{
        "case_id":{"type":"integer","minimum":1},
        "expected_version":{"type":"integer","minimum":1},
        "note":{"type":"string","maxLength":200}
    }})
}
fn output() -> Value {
    json!({"type":"object"})
}
static ROUTE_CASE: Change2 = Change2::Route;
static FLAG_CASE: Change2 = Change2::Flag;
inventory::submit! { Contribution::new(DEFINITIONS, MODULE, provenance!(), &Definitions {
    operations: &[
        Operation { key: "rule_cases.cases.open", title: "Open a case", summary: "Open a case for a company.", keywords: &["case"], capability: OPEN, input: open_schema, output, handler: Handler::SafeWrite(&Open), channels: Channels::Every, step_up: &[] },
        Operation { key: "rule_cases.cases.route", title: "Route a case", summary: "Send a case to a queue.", keywords: &["case", "queue"], capability: ROUTE, input: route_schema, output, handler: Handler::SafeWrite(&ROUTE_CASE), channels: Channels::Every, step_up: &[] },
        Operation { key: "rule_cases.cases.flag", title: "Flag a case", summary: "Raise a flag on a case.", keywords: &["case", "flag"], capability: FLAG, input: flag_schema, output, handler: Handler::SafeWrite(&FLAG_CASE), channels: Channels::Every, step_up: &[] },
    ],
    guides: &[],
}) }
