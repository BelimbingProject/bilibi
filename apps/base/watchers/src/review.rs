//! Writing watchers: a draft's checks and its version, the approval review and
//! the approval, switching a watcher on and off, and a version in words. Every
//! write runs in the caller's `MutationTransaction` with its sealed scope and
//! returns audited changes; every check is also made in the commit, on the
//! commit's own transaction.
use crate::catalog::OfferedTrigger;
use crate::definition::{self, Definition, Example, Problems};
use crate::store::Watchers;
use crate::{ACTIVE_PER_TRIGGER, APPROVE, MANAGE};
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_grid::{Catalog, CustomTable, FieldType, Installed, Link, LinkKind, Table};
use bilibi_tenancy::Scope;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio_postgres::GenericClient;

/// What a draft or an approval would do, and why it would be refused.
#[derive(Clone, Debug, Default)]
pub struct Review {
    /// Field path → problems; empty when the write may go ahead.
    pub problems: Problems,
    pub conditions: Vec<String>,
    pub actions: Vec<String>,
    /// Each example: what it expects and what the conditions answer.
    pub examples: Vec<(bool, bool)>,
    pub script_examples: Vec<String>,
    pub warnings: Vec<String>,
    pub trigger_title: String,
}
impl Review {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "trigger": self.trigger_title,
            "conditions": self.conditions,
            "actions": self.actions,
            "examples": self.examples.iter().map(|(expect, got)| json!({
                "expect": if *expect {"match"} else {"no_match"},
                "result": if *got {"match"} else {"no_match"},
                "passes": expect == got,
            })).collect::<Vec<_>>(),
            "script_examples": self.script_examples,
            "warnings": self.warnings,
            "problems": self.problems.iter().map(|(field, why)| json!({"field": field, "problem": why})).collect::<Vec<_>>(),
        })
    }
}

/// A draft as the contract and page submit it.
pub(crate) struct Draft {
    pub watcher_id: Option<i64>,
    pub name: String,
    pub trigger: String,
    pub definition: Option<Definition>,
    pub examples: Vec<Example>,
    pub explanation: Option<String>,
}
impl Draft {
    pub(crate) fn parse(input: &Value, problems: &mut Problems) -> Self {
        let text = |key: &str| bilibi_agent_api::text_field(input, key);
        let name = text("name");
        if name.is_empty() || name.chars().count() > 120 {
            problems.push(("name".into(), "give a name of 1 to 120 characters".into()));
        }
        let definition = Definition::parse_draft(input, problems);
        let examples = definition::parse_examples(&input["examples"], problems);
        let explanation = bilibi_agent_api::optional_text_field(input, "explanation")
            .map(|text| text.chars().take(2000).collect());
        Self {
            watcher_id: input["watcher_id"].as_i64(),
            name,
            trigger: text("watch"),
            definition,
            examples,
            explanation,
        }
    }
}

/// A condition path that ends in a tenant custom field: the one-links walked
/// before it (empty on the trigger's own record) and the field's key. So
/// `cf_segment` is `("", "segment")` and `company.cf_segment` is
/// `("company", "segment")`.
pub(crate) fn custom_path(path: &str) -> Option<(&str, &str)> {
    let (links, field) = path.rsplit_once('.').unwrap_or(("", path));
    bilibi_grid::custom_key(field).map(|key| (links, key))
}

/// The one-links `links` walks from `root` in the installed catalog, and the
/// table they reach: where a linked record's custom field lives. The installed
/// catalog holds no tenant field, so its own resolver stops at `cf_<key>`.
pub(crate) fn linked<'g>(
    grid: &'g Installed,
    root: &'g Table,
    links: &str,
) -> Option<(Vec<Link>, &'g Table)> {
    let mut table = root;
    let mut hops = Vec::new();
    if links.is_empty() {
        return Some((hops, table));
    }
    for segment in links.split('.') {
        let link = table
            .link(segment)
            .filter(|link| link.kind() == LinkKind::One)?;
        table = grid.table(link.to())?;
        hops.push(*link);
    }
    Some((hops, table))
}

/// The one-links a condition path walks from `root`: a declared field's, or
/// a linked record's custom field's, which the installed resolver cannot name.
pub(crate) fn hops(grid: &Installed, root: &Table, path: &str) -> Vec<Link> {
    match bilibi_grid::column::resolve(path, root, &|id| grid.table(id)) {
        Ok(column) => column.hops,
        Err(_) => custom_path(path)
            .and_then(|(links, _)| linked(grid, root, links))
            .map(|(hops, _)| hops)
            .unwrap_or_default(),
    }
}

fn digest(definition: &Value, examples: &Value) -> String {
    let bytes = serde_json::to_vec(&json!({"definition": definition, "examples": examples}))
        .unwrap_or_default();
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl Watchers {
    /// The tenant's custom fields, as a fingerprint pins them
    /// (`catalog_digest`). Read only when one of `definitions` has a custom
    /// condition: a watcher on declared fields reads no definitions.
    pub(crate) async fn custom_tables<'d>(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        definitions: impl IntoIterator<Item = &'d Definition>,
    ) -> Result<Vec<CustomTable>> {
        let custom = definitions.into_iter().any(|definition| {
            definition
                .fields()
                .iter()
                .any(|path| custom_path(path).is_some())
        });
        if !custom {
            return Ok(Vec::new());
        }
        bilibi_grid::custom_tables(client, scope).await
    }

    /// The installed bindings a version runs on. A condition on a tenant
    /// custom field, of the trigger's table or of a record one-links reach,
    /// pins the field's type and value SQL from `custom` (`custom_tables`) and
    /// the links walked to it; a field that is archived, gone or no longer
    /// offered to watchers has no fingerprint, so the version is refused.
    pub(crate) fn catalog_digest(
        &self,
        event: &str,
        definition: &Definition,
        custom: &[CustomTable],
    ) -> Option<String> {
        let trigger = self.trigger(event)?.definition;
        let grid = bilibi_grid::installed().ok()?;
        let root = grid.table(trigger.table)?;
        let mut paths = definition.fields();
        paths.push(trigger.version.to_owned());
        let hop = |link: &Link| -> Option<Value> {
            let from = grid.table(link.decl.from)?;
            let to = grid.table(link.to())?;
            let join = match link.decl.join {
                bilibi_grid::Join::On {
                    from_field,
                    to_field,
                } => json!({
                    "from": from.field(from_field)?.source_key(),
                    "to": to.field(to_field)?.source_key(),
                }),
                bilibi_grid::Join::Edge(source) => {
                    json!({"edge": source.sql, "tenant_scoped": source.tenant_scoped})
                }
            };
            Some(
                json!({"id": link.id(), "from": from.id(), "to": to.id(), "kind": format!("{:?}", link.kind()),
                "join": join, "source": to.source().sql, "tenant_scoped": to.source().tenant_scoped,
                "key": to.key(), "key_column": to.key_field().source_key(), "capability": to.capability()}),
            )
        };
        let fields: Option<Vec<Value>> = paths.iter().map(|path| {
            let Ok(column) = bilibi_grid::column::resolve(path, root, &|id| grid.table(id)) else {
                let (links, key) = custom_path(path)?;
                let (hops, owner) = linked(grid, root, links)?;
                let (table, column) = custom.iter().filter(|table| table.table == owner.id()).find_map(|table| {
                    table.columns.iter().find(|column| column.key == key).map(|column| (table, column))
                })?;
                let hops: Option<Vec<Value>> = hops.iter().map(hop).collect();
                return column.watcher_condition.then_some(hops?).map(|hops| json!({"path": path, "table": table.table, "type": format!("{:?}", column.ty),
                    "custom": {"column": table.column, "value": column.value}, "hops": hops}));
            };
            let field = column.field?;
            let hops: Option<Vec<Value>> = column.hops.iter().map(hop).collect();
            Some(json!({"path": path, "table": column.table, "type": format!("{:?}", column.ty), "values": field.values, "column": field.source_key(), "hops": hops?}))
        }).collect();
        let actions: Option<Vec<Value>> = definition.actions().map(|call| {
            let action = self.action(&call.operation)?;
            let mut binding = json!({"key": action.key, "table": action.definition.table, "subject": action.definition.subject,
                "version": action.definition.version, "input": action.input, "records": action.definition.records, "capability": action.capability, "approval_capabilities": action.definition.approval_capabilities});
            if definition.scripts().next().is_some() {
                binding["script_local"] = json!(action.local_script);
            }
            Some(binding)
        }).collect();
        let lookups: Option<Vec<Value>> = definition
            .scripts()
            .flat_map(|script| &script.lookups)
            .map(|call| {
                let lookup = self.lookup(&call.operation)?;
                Some(
                    json!({"key": lookup.key, "input": lookup.input, "output": lookup.output,
                "capability": lookup.capability}),
                )
            })
            .collect();
        let mut binding = json!({"event": event, "versions": trigger.on.versions, "table": trigger.table,
            "subject": trigger.subject, "logical_event": trigger.logical_event, "version": trigger.version, "source": root.source().sql, "key": root.key(), "key_column": root.key_field().source_key(), "tenant_scoped": root.source().tenant_scoped,
            "capability": root.capability(), "fields": fields?, "actions": actions?});
        let lookups = lookups?;
        if !lookups.is_empty() {
            binding["lookups"] = json!(lookups);
        }
        Some(digest(&binding, &Value::Null))
    }

    pub(crate) async fn approved_field_uses(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        table: &str,
        key: &str,
    ) -> Result<Vec<String>> {
        let rows = client
            .query(
                "SELECT r.name,r.trigger,v.definition FROM base_watchers r JOIN base_watcher_versions v
             ON v.tenant_id=r.tenant_id AND v.id=r.active_version_id
             WHERE r.tenant_id=$1 AND v.approved_at IS NOT NULL ORDER BY r.name,r.id",
                &[&scope.tenant()],
            )
            .await?;
        let may_name = bilibi_authz::holds_proof(client, scope, crate::VIEW).await?;
        let grid = bilibi_grid::installed()?;
        let mut affected = Vec::new();
        for row in rows {
            let Some(root) = self
                .trigger(row.get(1))
                .and_then(|trigger| grid.table(trigger.definition.table))
            else {
                continue;
            };
            let definition =
                Definition::parse(&row.get(2), &mut Vec::new()).ok_or(Error::Invalid)?;
            // The field's own table is the trigger's, or one its links reach.
            if definition.fields().iter().any(|path| {
                custom_path(path).is_some_and(|(links, used)| {
                    used == key
                        && linked(grid, root, links).is_some_and(|(_, owner)| owner.id() == table)
                })
            }) {
                affected.push(if may_name {
                    row.get(0)
                } else {
                    "Restricted watcher".into()
                });
            }
        }
        Ok(affected)
    }

    pub(crate) async fn lock_admission(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
    ) -> Result<()> {
        let key = format!("base/watchers:{}", scope.tenant());
        client
            .execute(
                "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
                &[&key],
            )
            .await?;
        Ok(())
    }

    /// The checks a draft passes: a mounted trigger, fields the drafter may
    /// read with values of their type, installed actions on the same record
    /// with inputs their contract accepts, and examples that come out as
    /// expected. Writes nothing.
    pub(crate) async fn review_draft(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        draft: &Draft,
        mut problems: Problems,
    ) -> Result<Review> {
        bilibi_authz::authorize_key(client, scope, MANAGE).await?;
        let mut review = Review::default();
        if let Some(id) = draft.watcher_id {
            let row = client
                .query_opt(
                    "SELECT trigger FROM base_watchers WHERE tenant_id=$1 AND id=$2",
                    &[&scope.tenant(), &id],
                )
                .await?
                .ok_or(Error::NotFound)?;
            if row.get::<_, String>(0) != draft.trigger {
                problems.push((
                    "watch".into(),
                    "a watcher keeps its trigger; draft a new watcher for another one".into(),
                ));
            }
        }
        let Some(trigger) = self.trigger(&draft.trigger) else {
            problems.push(("watch".into(), "is not an installed trigger".into()));
            review.problems = problems;
            return Ok(review);
        };
        let catalog = Catalog::for_scope(client, scope)
            .await
            .map_err(Error::from)?;
        self.review_new_draft(client, scope, draft, problems, &catalog, trigger)
            .await
    }

    pub(crate) async fn review_new_draft(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        draft: &Draft,
        mut problems: Problems,
        catalog: &Catalog,
        trigger: &OfferedTrigger,
    ) -> Result<Review> {
        let mut review = Review {
            trigger_title: trigger.definition.title.to_owned(),
            ..Review::default()
        };
        let Some(definition) = &draft.definition else {
            review.problems = problems;
            return Ok(review);
        };
        self.check(
            catalog,
            trigger,
            definition,
            &draft.examples,
            &mut review,
            &mut problems,
        )?;
        self.review_script_examples(
            client,
            scope,
            (catalog, trigger, definition),
            &mut review,
            &mut problems,
        )
        .await?;
        problems.extend(
            self.check_limits(
                client,
                scope,
                definition,
                draft.examples.len(),
                trigger.definition.version,
            )
            .await?,
        );
        review.problems = problems;
        Ok(review)
    }

    /// Fields, values, actions and examples against `catalog` (the reader's).
    fn check(
        &self,
        catalog: &Catalog,
        trigger: &OfferedTrigger,
        definition: &Definition,
        examples: &[Example],
        review: &mut Review,
        problems: &mut Problems,
    ) -> Result<()> {
        let table = catalog.table(trigger.definition.table);
        if !definition.conditions.is_empty() && examples.is_empty() {
            problems.push((
                "examples".into(),
                "give at least one example for a watcher with conditions".into(),
            ));
        }
        // A tenant custom field is a condition only when the tenant offered
        // it to watchers (#730), on the trigger's own record or on a record
        // one-links reach (#862).
        for (i, condition) in definition.conditions.iter().enumerate() {
            let Some(column) =
                table.and_then(|table| catalog.resolve(table, &condition.field).ok())
            else {
                continue;
            };
            let Some(owner) = catalog.table(column.table) else {
                continue;
            };
            let Some(field) = column.field.filter(|field| owner.is_custom(field.id)) else {
                continue;
            };
            if !owner.custom_watcher_condition(field.id) {
                problems.push((format!("if.conditions.{i}.field"), format!("{} is a custom field not offered to watchers; an administrator turns on Watcher condition for it on Custom fields", condition.field)));
            }
        }
        let field = |path: &str| -> Option<(FieldType, &'static [&'static str], String)> {
            let table = table?;
            let column = catalog.resolve(table, path).ok()?;
            if column.agg.is_some() || column.many.is_some() {
                return None;
            }
            let field = column.field?;
            if catalog.masked(column.table, field.id) {
                return None;
            }
            Some((column.ty, field.values, column.label.clone()))
        };
        if table.is_none() {
            problems.push((
                "watch".into(),
                format!(
                    "you may not read {}, so you cannot write conditions on it",
                    trigger.definition.table
                ),
            ));
        }
        definition.check_types(
            |path| field(path).map(|(ty, values, _)| (ty, values)),
            problems,
        );
        for script in definition.scripts() {
            for path in &script.fields {
                let allowed = table
                    .and_then(|table| catalog.resolve(table, path).ok())
                    .is_some_and(|column| {
                        field(path).is_some()
                            && column.field.is_some_and(|f| {
                                catalog.table(column.table).is_some_and(|owner| {
                                    !owner.is_custom(f.id)
                                        || (column.hops.is_empty()
                                            && owner.custom_watcher_condition(f.id))
                                })
                            })
                    });
                if !allowed {
                    problems.push(("do".into(), format!("{} cannot read {path}.", script.name)));
                }
            }
            for (index, call) in script.lookups.iter().enumerate() {
                let Some(lookup) = self.lookup(&call.operation) else {
                    problems.push((
                        "do".into(),
                        format!("{} lookup {} is not installed", script.name, call.operation),
                    ));
                    continue;
                };
                if let Err(fields) = bilibi_agent_api::schema::validate(
                    &lookup.input,
                    &Value::Object(call.input.clone()),
                ) {
                    for (field, why) in fields {
                        problems
                            .push((format!("do.{index}.script.lookups.{field}"), why.join("; ")));
                    }
                }
            }
            review.actions.push(script.describe());
        }
        review.conditions = definition
            .conditions
            .iter()
            .map(|c| {
                definition::sentence(
                    c,
                    &field(&c.field).map_or_else(|| c.field.clone(), |(_, _, label)| label),
                )
            })
            .collect();
        for (i, call) in definition.actions().enumerate() {
            let at = |part: &str| format!("do.{i}.{part}");
            let Some(action) = self.action(&call.operation) else {
                problems.push((
                    at("operation"),
                    format!("{} is not an installed action", call.operation),
                ));
                continue;
            };
            if !action.local_script
                && definition
                    .script_calls()
                    .any(|declared| declared.operation == action.key)
            {
                problems.push((
                    at("operation"),
                    format!("{} is unavailable in scripted watchers", action.key),
                ));
            }
            if action.definition.table != trigger.definition.table {
                problems.push((
                    at("operation"),
                    format!(
                        "acts on {}, not on the trigger's {}",
                        action.definition.table, trigger.definition.table
                    ),
                ));
            }
            if call.input.contains_key(action.definition.subject)
                || call.input.contains_key(action.definition.version)
            {
                problems.push((
                    at("input"),
                    format!(
                        "{} is the trigger's record; leave it out",
                        action.definition.subject
                    ),
                ));
            }
            let mut input = call.input.clone();
            input.insert(action.definition.version.into(), json!(1));
            input.insert(
                action.definition.subject.into(),
                placeholder(&action.input, action.definition.subject),
            );
            if let Err(bilibi_agent_api::Error::InvalidInput(fields)) =
                self.jobs.validate(action.key, &Value::Object(input))
            {
                for (field, why) in fields {
                    problems.push((format!("do.{i}.input.{field}"), why.join("; ")));
                }
            }
            let parameters: Vec<String> = call
                .input
                .iter()
                .map(|(key, value)| format!("{key}: {}", definition::shown(value)))
                .collect();
            review.actions.push(if parameters.is_empty() {
                action.title.to_owned()
            } else {
                format!("{} ({})", action.title, parameters.join(", "))
            });
        }
        let types = |path: &str| field(path).map_or(FieldType::String, |(ty, _, _)| ty);
        let fields = definition.condition_fields();
        for (i, example) in examples.iter().enumerate() {
            if let Some(unknown) = example.facts.keys().find(|key| !fields.contains(key)) {
                problems.push((
                    format!("examples.{i}.facts"),
                    format!("{unknown} is not a field the conditions read"),
                ));
            }
            let (matched, _) = definition.evaluate(&example.facts, &types);
            if matched != example.expect {
                problems.push((
                    format!("examples.{i}"),
                    format!(
                        "expects {} but the conditions answer {}",
                        if example.expect {
                            "a match"
                        } else {
                            "no match"
                        },
                        if matched { "a match" } else { "no match" },
                    ),
                ));
            }
            review.examples.push((example.expect, matched));
        }
        Ok(())
    }

    async fn review_script_examples(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        context: (&Catalog, &OfferedTrigger, &Definition),
        review: &mut Review,
        problems: &mut Problems,
    ) -> Result<()> {
        let (catalog, trigger, definition) = context;
        if definition.scripts().next().is_none() || !problems.is_empty() {
            return Ok(());
        }
        let table = catalog
            .table(trigger.definition.table)
            .ok_or(Error::Invalid)?;
        let time = self.limit(client, scope, crate::SCRIPT_TIME).await? as u64;
        let (heap, limit) = self.script_limits(client, scope).await?;
        let permits = match self
            .reviewed_script_permits(client, scope, definition)
            .await?
        {
            Ok(permits) => permits,
            Err(_) => {
                problems.push((
                    "do".into(),
                    "You need the script execution grant to run examples.".into(),
                ));
                return Ok(());
            }
        };
        for script in definition.scripts() {
            let permit = permits.for_script(script).map_err(|_| Error::Invalid)?;
            for (index, example) in script.examples.iter().enumerate() {
                let mut facts = serde_json::Map::new();
                let mut valid = true;
                for (path, value) in &example.facts {
                    let column = catalog.resolve(table, path).map_err(|_| Error::Invalid)?;
                    let field = column.field.ok_or(Error::Invalid)?;
                    let value = match definition::script_fact(column.ty, field.values, value) {
                        Ok(value) => value,
                        Err(why) => {
                            problems.push((
                                "do".into(),
                                format!(
                                    "{} example {} field {path} {why}.",
                                    script.name,
                                    index + 1
                                ),
                            ));
                            valid = false;
                            continue;
                        }
                    };
                    facts.insert(path.clone(), value);
                }
                if !valid {
                    continue;
                }
                let actual = tokio::time::timeout(
                    std::time::Duration::from_millis(time),
                    script.capture(&permit, &facts, &example.lookup_fixtures, time, heap),
                )
                .await
                .unwrap_or(Err("script_timeout"));
                let passes = actual.as_ref().is_ok_and(|calls| {
                    *calls == example.expect && script.permits(calls) && calls.len() <= limit
                });
                let actual = match actual {
                    Ok(calls) => json!(calls).to_string(),
                    Err(reason) => reason.to_owned(),
                };
                let line = format!(
                    "{} example {} expected {} and returned {}.",
                    script.name,
                    index + 1,
                    json!(example.expect),
                    actual
                );
                if !passes {
                    problems.push(("do".into(), line.clone()));
                }
                review.script_examples.push(line);
            }
        }
        Ok(())
    }

    /// Writes the draft as the watcher's next version (a new watcher when it names
    /// none). Nothing runs until an administrator approves the version.
    pub(crate) async fn draft(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        draft: Draft,
        problems: Problems,
    ) -> Result<(i64, i32, Review)> {
        self.lock_admission(tx.sql(), scope).await?;
        if let Some(id) = draft.watcher_id {
            tx.sql()
                .query_opt(
                    "SELECT 1 FROM base_watchers WHERE tenant_id=$1 AND id=$2 FOR UPDATE",
                    &[&scope.tenant(), &id],
                )
                .await?
                .ok_or(Error::NotFound)?;
        }
        let review = self.review_draft(tx.sql(), scope, &draft, problems).await?;
        if !review.ok() {
            return Err(Error::Invalid);
        }
        let definition = draft.definition.as_ref().ok_or(Error::Invalid)?.to_json();
        let examples = definition::examples_json(&draft.examples);
        let digest = digest(&definition, &examples);
        let (tenant, user) = (scope.tenant(), scope.user());
        let via = if scope.agent().is_some() {
            "agent"
        } else {
            "person"
        };
        let Draft {
            watcher_id,
            name,
            trigger,
            explanation,
            ..
        } = draft;
        let (id, number) = tx.mutate(move |sql| Box::pin(async move {
            let mut changes = Vec::new();
            let id = match watcher_id {
                Some(id) => {
                    let old = sql.query_one("SELECT name,version,updated_at::text FROM base_watchers WHERE tenant_id=$1 AND id=$2", &[&tenant, &id]).await?;
                    changes.push(Change { kind: "base/watchers.watcher".into(), id, old: json!({"name": old.get::<_, String>(0), "version": old.get::<_, i32>(1), "updated_at": old.get::<_, String>(2)}), new: json!({"name": name, "version": old.get::<_, i32>(1) + 1}) });
                    sql.execute("UPDATE base_watchers SET name=$3,updated_at=now(),version=version+1 WHERE tenant_id=$1 AND id=$2", &[&tenant, &id, &name]).await?;
                    id
                }
                None => {
                    let id: i64 = sql.query_one("INSERT INTO base_watchers(tenant_id,name,trigger) VALUES($1,$2,$3) RETURNING id", &[&tenant, &name, &trigger]).await?.get(0);
                    changes.push(Change { kind: "base/watchers.watcher".into(), id, old: Value::Null, new: json!({"name": name, "trigger": trigger, "version": 1, "state": "draft", "rule": crate::discard::draft_summary(&definition, &examples)}) });
                    id
                }
            };
            let row = sql.query_one(
                "INSERT INTO base_watcher_versions(tenant_id,watcher_id,number,definition,examples,explanation,digest,drafted_via,drafted_by) SELECT $1,$2,COALESCE(max(number),0)+1,$3,$4,$5,$6,$7,$8 FROM base_watcher_versions WHERE tenant_id=$1 AND watcher_id=$2 RETURNING id,number",
                &[&tenant, &id, &definition, &examples, &explanation, &digest, &via, &user],
            ).await?;
            let (version, number): (i64, i32) = (row.get(0), row.get(1));
            changes.push(Change { kind: "base/watchers.version".into(), id: version, old: Value::Null,
                new: json!({"watcher": id, "number": number, "definition": definition, "examples": examples, "digest": digest, "drafted_via": via}) });
            Ok(((id, number), changes))
        })).await?;
        Ok((id, number, review))
    }

    /// The approval review of version `number`: the draft checks again, now
    /// as the approver, plus what only an approval needs. The approver must
    /// hold All reach for the root, linked tables and action capabilities;
    /// the `watchers` principal must not meet a restricted or masked field;
    /// actions must not close a cycle through active watchers; and the tenant's
    /// configured limits hold.
    pub async fn review_approval(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        watcher: i64,
        number: i32,
    ) -> Result<Review> {
        self.review_activation(client, scope, watcher, number, Activation::Approve)
            .await
    }

    pub async fn review_restore(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        watcher: i64,
        number: i32,
    ) -> Result<Review> {
        self.review_activation(client, scope, watcher, number, Activation::Restore)
            .await
    }

    async fn review_activation(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        watcher: i64,
        number: i32,
        activation: Activation,
    ) -> Result<Review> {
        bilibi_authz::authorize_key(client, scope, APPROVE).await?;
        let tenant = scope.tenant();
        let row = client.query_opt(
            "SELECT r.name,r.trigger,v.definition,v.examples,v.approved_at IS NOT NULL,(SELECT max(number) FROM base_watcher_versions l WHERE l.tenant_id=r.tenant_id AND l.watcher_id=r.id),r.active_version_id=v.id AND r.disabled_at IS NULL FROM base_watchers r JOIN base_watcher_versions v ON v.tenant_id=r.tenant_id AND v.watcher_id=r.id AND v.number=$3 WHERE r.tenant_id=$1 AND r.id=$2",
            &[&tenant, &watcher, &number],
        ).await?.ok_or(Error::NotFound)?;
        let mut problems = Problems::new();
        let latest: i32 = row.get(5);
        if activation == Activation::Approve && latest != number {
            problems.push((
                "version".into(),
                format!("version {latest} is newer; review and approve that one"),
            ));
        }
        if activation != Activation::Reenable && row.get::<_, Option<bool>>(6) == Some(true) {
            problems.push(("version".into(), "this version already runs".into()));
        }
        if activation == Activation::Restore && !row.get::<_, bool>(4) {
            problems.push((
                "version".into(),
                "only an earlier approved version can be restored".into(),
            ));
        }
        let trigger: String = row.get(1);
        if row.get::<_, bool>(4) {
            let definition = Definition::parse(&row.get::<_, Value>(2), &mut Vec::new())
                .ok_or(Error::Invalid)?;
            let custom = self.custom_tables(client, scope, [&definition]).await?;
            let saved = client.query_one("SELECT catalog_digest FROM base_watcher_versions WHERE tenant_id=$1 AND watcher_id=$2 AND number=$3", &[&tenant, &watcher, &number]).await?;
            let fingerprint: Option<String> = saved.get(0);
            if fingerprint.is_none()
                || fingerprint != self.catalog_digest(&trigger, &definition, &custom)
            {
                problems.push((
                    "version".into(),
                    "the installed fields or actions changed; draft a new version for review"
                        .into(),
                ));
            }
        }
        let mut draft = Draft {
            watcher_id: None,
            name: row.get(0),
            trigger: trigger.clone(),
            definition: Definition::parse(&row.get::<_, Value>(2), &mut problems),
            examples: definition::parse_examples(&row.get::<_, Value>(3), &mut problems),
            explanation: None,
        };
        // Drafted under MANAGE; the approver reviews it under APPROVE, as
        // themselves: what they cannot read they cannot approve.
        let mut review = Review::default();
        let Some(offered) = self.trigger(&trigger) else {
            problems.push(("watch".into(), "is no longer installed".into()));
            review.problems = problems;
            return Ok(review);
        };
        review.trigger_title = offered.definition.title.to_owned();
        let Some(definition) = draft.definition.take() else {
            review.problems = problems;
            return Ok(review);
        };
        let catalog = Catalog::for_scope(client, scope)
            .await
            .map_err(Error::from)?;
        self.check(
            &catalog,
            offered,
            &definition,
            &draft.examples,
            &mut review,
            &mut problems,
        )?;
        problems.extend(
            self.check_limits(
                client,
                scope,
                &definition,
                draft.examples.len(),
                offered.definition.version,
            )
            .await?,
        );
        self.review_script_examples(
            client,
            scope,
            (&catalog, offered, &definition),
            &mut review,
            &mut problems,
        )
        .await?;
        // The approver's own authority: approving never delegates more.
        let grid = bilibi_grid::installed()?;
        let mut needed: std::collections::BTreeSet<&str> = grid
            .table(offered.definition.table)
            .map(|table| std::collections::BTreeSet::from([table.capability()]))
            .unwrap_or_default();
        if let Some(root) = grid.table(offered.definition.table) {
            for path in definition.fields() {
                let hops = hops(grid, root, &path);
                needed.extend(
                    hops.iter()
                        .filter_map(|hop| grid.table(hop.to()))
                        .map(|table| table.capability()),
                );
            }
        }
        if definition.scripts().next().is_some() {
            needed.insert(crate::RUN_SCRIPT);
        }
        let authority = bilibi_authz::effective_grants(client, scope).await?;
        needed.extend(
            definition
                .actions()
                .filter_map(|a| self.action(&a.operation))
                .flat_map(|a| {
                    std::iter::once(a.capability)
                        .chain(a.definition.approval_capabilities.iter().copied())
                }),
        );
        needed.extend(
            definition
                .scripts()
                .flat_map(|script| &script.lookups)
                .filter_map(|lookup| self.lookup(&lookup.operation))
                .map(|lookup| lookup.capability),
        );
        for key in needed {
            if !bilibi_authz::decide(client, scope, key).await?.allowed {
                problems.push(("version".into(), format!("you may not do what this watcher does yourself ({key}), so you cannot approve it")));
            } else if !authority.reaches(key, bilibi_authz::Reach::All) {
                problems.push((
                    "version".into(),
                    format!("you need access to all records for {key} to approve this watcher"),
                ));
            }
        }
        // The principal reads every field the conditions name; a restricted or
        // masked one is refused at the source, never read and hidden later.
        match self.principal(client, tenant).await {
            Ok(principal) => {
                let theirs = Catalog::for_scope(client, &principal)
                    .await
                    .map_err(Error::from)?;
                match theirs.table(offered.definition.table) {
                    None => review.warnings.push(format!(
                        "Automation may not read {} yet: the watcher will not act until an administrator allows it in Automation access.",
                        grid.table(offered.definition.table).map_or(offered.definition.table, |t| t.label())
                    )),
                    Some(table) => {
                        for path in definition.fields() {
                            let readable = theirs.resolve(table, &path).ok().filter(|column| {
                                column.field.is_some_and(|f| !theirs.masked(column.table, f.id))
                            });
                            if readable.is_none() {
                                problems.push(("fields".into(), format!("{path} is hidden from automation, and a watcher never reads a hidden field")));
                            }
                        }
                    }
                }
                for key in definition
                    .scripts()
                    .flat_map(|script| &script.lookups)
                    .filter_map(|lookup| self.lookup(&lookup.operation))
                    .map(|lookup| lookup.capability)
                    .collect::<std::collections::BTreeSet<_>>()
                {
                    if !bilibi_authz::decide(client, &principal, key).await?.allowed {
                        problems.push((
                            "lookups".into(),
                            format!(
                                "automation may not use {key}; restore its live grant before activating this version"
                            ),
                        ));
                    }
                }
            }
            Err(Error::Unauthorized) => return Err(Error::Unauthorized),
            Err(_) => review
                .warnings
                .push("Automation is not available here.".into()),
        }
        let records: Vec<&str> = definition
            .actions()
            .filter_map(|a| self.action(&a.operation))
            .flat_map(|a| a.definition.records.iter().copied())
            .collect();
        if records.contains(&trigger.as_str()) {
            problems.push((
                "do".into(),
                format!(
                    "its action causes \u{201c}{}\u{201d} again, so the watcher would start itself",
                    offered.definition.title
                ),
            ));
        }
        let active = client.query(
            "SELECT r.id,r.name,r.trigger,v.definition FROM base_watchers r JOIN base_watcher_versions v ON v.tenant_id=r.tenant_id AND v.id=r.active_version_id WHERE r.tenant_id=$1 AND r.id<>$2 AND r.disabled_at IS NULL",
            &[&tenant, &watcher],
        ).await?;
        let same = active
            .iter()
            .filter(|row| row.get::<_, &str>(2) == trigger)
            .count() as i64;
        let most = self.limit(client, scope, ACTIVE_PER_TRIGGER).await?;
        if same >= most {
            problems.push((
                "watch".into(),
                format!("{most} watchers already run on this trigger, the most allowed"),
            ));
        }
        let mut graph: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for row in &active {
            let mut ignored = Vec::new();
            if let Some(other) = Definition::parse(&row.get::<_, Value>(3), &mut ignored) {
                let edges = graph.entry(row.get(2)).or_default();
                edges.extend(
                    other
                        .actions()
                        .filter_map(|call| self.action(&call.operation))
                        .flat_map(|action| {
                            action
                                .definition
                                .records
                                .iter()
                                .map(|event| (*event).to_owned())
                        }),
                );
            }
        }
        let mut pending: Vec<String> = records.iter().map(|event| (*event).to_owned()).collect();
        let mut visited = std::collections::BTreeSet::new();
        while let Some(event) = pending.pop() {
            if event == trigger {
                problems.push(("do".into(), "its actions close a cycle of watchers; change the actions or switch off a watcher in the cycle".into()));
                break;
            }
            if visited.insert(event.clone()) {
                pending.extend(graph.get(&event).into_iter().flatten().cloned());
            }
        }
        review.problems = problems;
        Ok(review)
    }

    /// Approves version `number` and makes it the one that runs, switching
    /// the watcher on. Runs already queued for an earlier version are refused
    /// as stale when they come to act.
    pub async fn approve(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        watcher: i64,
        number: i32,
    ) -> Result<Review> {
        self.activate(tx, scope, watcher, number, false, None).await
    }

    /// Restores an earlier approved version only after repeating every current
    /// permission, binding, example, limit and cycle check. A refusal leaves
    /// the running version unchanged.
    pub async fn restore(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        watcher: i64,
        number: i32,
        expected_activation: Option<i32>,
    ) -> Result<Review> {
        self.activate(tx, scope, watcher, number, true, expected_activation)
            .await
    }

    async fn activate(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        watcher: i64,
        number: i32,
        restoring: bool,
        expected_activation: Option<i32>,
    ) -> Result<Review> {
        let tenant = scope.tenant();
        self.lock_admission(tx.sql(), scope).await?;
        let locked = tx.sql()
            .query_opt(
                "SELECT version,disabled_at IS NULL FROM base_watchers WHERE tenant_id=$1 AND id=$2 FOR UPDATE",
                &[&tenant, &watcher],
            )
            .await?
            .ok_or(Error::NotFound)?;
        if restoring
            && expected_activation.is_some_and(|expected| {
                locked.get::<_, i32>(0) != expected || !locked.get::<_, bool>(1)
            })
        {
            return Err(Error::Conflict);
        }
        let review = if restoring {
            self.review_restore(tx.sql(), scope, watcher, number)
                .await?
        } else {
            self.review_approval(tx.sql(), scope, watcher, number)
                .await?
        };
        if !review.ok() {
            return Err(Error::Invalid);
        }
        let approved = tx.sql().query_one("SELECT definition FROM base_watcher_versions WHERE tenant_id=$1 AND watcher_id=$2 AND number=$3", &[&tenant, &watcher, &number]).await?;
        let definition = Definition::parse(&approved.get::<_, Value>(0), &mut Vec::new())
            .ok_or(Error::Invalid)?;
        let event: String = tx
            .sql()
            .query_one(
                "SELECT trigger FROM base_watchers WHERE tenant_id=$1 AND id=$2",
                &[&tenant, &watcher],
            )
            .await?
            .get(0);
        let custom = self.custom_tables(tx.sql(), scope, [&definition]).await?;
        let catalog_digest = self
            .catalog_digest(&event, &definition, &custom)
            .ok_or(Error::Invalid)?;
        let script_digests: Vec<String> = definition
            .scripts()
            .map(|script| script.permission_digest())
            .collect();
        let user = scope.user();
        tx.mutate(move |sql| Box::pin(async move {
            let previous = sql.query_one("SELECT id,approved_by,approved_at::text,catalog_digest FROM base_watcher_versions WHERE tenant_id=$1 AND watcher_id=$2 AND number=$3", &[&tenant, &watcher, &number]).await?;
            let version: i64 = sql.query_one(
                "UPDATE base_watcher_versions SET approved_by=COALESCE(approved_by,$4),approved_at=COALESCE(approved_at,clock_timestamp()),catalog_digest=COALESCE(catalog_digest,$5) WHERE tenant_id=$1 AND watcher_id=$2 AND number=$3 RETURNING id",
                &[&tenant, &watcher, &number, &user, &catalog_digest],
            ).await?.get(0);
            let old = sql.query_one(
                "SELECT r.active_version_id,v.number,r.version,r.disabled_at IS NULL FROM base_watchers r LEFT JOIN base_watcher_versions v ON v.tenant_id=r.tenant_id AND v.id=r.active_version_id WHERE r.tenant_id=$1 AND r.id=$2",
                &[&tenant, &watcher],
            ).await?;
            let old_id: Option<i64> = old.get(0);
            let old_number: Option<i32> = old.get(1);
            let old_activation: i32 = old.get(2);
            let old_enabled: bool = old.get(3);
            for digest in &script_digests {
                sql.execute(
                    "INSERT INTO base_watcher_script_permissions(tenant_id,watcher_id,version_id,script_digest,principal,granted_by) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT (tenant_id,version_id,script_digest,principal) DO UPDATE SET granted_by=EXCLUDED.granted_by,granted_at=clock_timestamp()",
                    &[&tenant, &watcher, &version, digest, &crate::PRINCIPAL, &user],
                ).await?;
            }
            let activation: i32 = sql.query_one(
                "UPDATE base_watchers SET active_version_id=$3,disabled_at=NULL,enabled_at=clock_timestamp(),needs_review=false,updated_at=now(),version=version+1 WHERE tenant_id=$1 AND id=$2 RETURNING version",
                &[&tenant, &watcher, &version],
            ).await?.get(0);
            let evidence = sql.query_one("SELECT approved_by,approved_at::text,catalog_digest FROM base_watcher_versions WHERE tenant_id=$1 AND id=$2", &[&tenant, &version]).await?;
            Ok(((), vec![Change { kind: "base/watchers.watcher".into(), id: watcher,
                old: json!({"active_version": old_id}), new: json!({"active_version": version, "number": number, "approved": true}) },
                Change { kind: "base/watchers.activation".into(), id: watcher,
                    old: json!({"active_version": old_id, "number": old_number, "activation": old_activation, "enabled": old_enabled}),
                    new: json!({"active_version": version, "number": number, "activation": activation, "enabled": true}) },
                Change { kind: "base/watchers.version".into(), id: previous.get(0),
                    old: json!({"approved_by": previous.get::<_, Option<i64>>(1), "approved_at": previous.get::<_, Option<String>>(2), "catalog_digest": previous.get::<_, Option<String>>(3)}),
                    new: json!({"approved_by": evidence.get::<_, Option<i64>>(0), "approved_at": evidence.get::<_, Option<String>>(1), "catalog_digest": evidence.get::<_, Option<String>>(2)}) }]))
        })).await?;
        Ok(review)
    }

    /// Switches a watcher's approved version on or off. Off stops new runs and
    /// refuses queued ones when they come to act; on runs the same approved
    /// version again after its approval checks pass once more.
    pub async fn set_enabled(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        watcher: i64,
        enabled: bool,
    ) -> Result<Option<Review>> {
        bilibi_authz::authorize_key(tx.sql(), scope, APPROVE).await?;
        let tenant = scope.tenant();
        self.lock_admission(tx.sql(), scope).await?;
        let row = tx.sql().query_opt(
            "SELECT r.disabled_at IS NOT NULL,v.number FROM base_watchers r LEFT JOIN base_watcher_versions v ON v.tenant_id=r.tenant_id AND v.id=r.active_version_id WHERE r.tenant_id=$1 AND r.id=$2 FOR UPDATE OF r",
            &[&tenant, &watcher],
        ).await?.ok_or(Error::NotFound)?;
        let (off, running): (bool, Option<i32>) = (row.get(0), row.get(1));
        let Some(running) = running else {
            return Err(Error::Conflict);
        };
        if off != enabled {
            return Ok(None);
        }
        let mut review = None;
        if enabled {
            let checked = self
                .review_activation(tx.sql(), scope, watcher, running, Activation::Reenable)
                .await?;
            if !checked.ok() {
                return Err(Error::Invalid);
            }
            review = Some(checked);
        }
        tx.mutate(move |sql| Box::pin(async move {
            sql.execute(
                "UPDATE base_watchers SET disabled_at=CASE WHEN $3 THEN NULL ELSE clock_timestamp() END,enabled_at=CASE WHEN $3 THEN clock_timestamp() ELSE enabled_at END,updated_at=now(),version=version+1 WHERE tenant_id=$1 AND id=$2",
                &[&tenant, &watcher, &enabled],
            ).await?;
            Ok(((), vec![Change { kind: "base/watchers.watcher".into(), id: watcher, old: json!({"enabled": !enabled}), new: json!({"enabled": enabled}) }]))
        })).await?;
        Ok(review)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Activation {
    Approve,
    Reenable,
    Restore,
}

/// A stand-in for the bound subject, typed as the contract's schema wants it,
/// so the rest of an action's input can be checked before any record exists.
pub(crate) fn placeholder(schema: &Value, field: &str) -> Value {
    match schema["properties"][field]["type"].as_str() {
        Some("integer") | Some("number") => json!(1),
        _ => json!("1"),
    }
}

/// The subject key bound into an action's input, typed as its schema wants.
pub(crate) fn bind(
    schema: &Value,
    field: &str,
    subject: &str,
    input: &Map<String, Value>,
) -> Option<Value> {
    let value = match schema["properties"][field]["type"].as_str() {
        Some("integer") | Some("number") => json!(subject.parse::<i64>().ok()?),
        _ => json!(subject),
    };
    let mut input = input.clone();
    input.insert(field.into(), value);
    Some(Value::Object(input))
}
