//! Child-granted fan-out over Grid, not an acting identity. The same SQL engine serves ordinary and oversight reads.
use crate::{
    Agg, Bounds, Catalog, Lenses, Line, PageSpec, Projection, QueryError, Summary, SummarySpec,
};
use bilibi_audit::{record_action, record_oversight_read, Attribution, OversightRead};
use bilibi_database::{Connection, Db, Error};
use bilibi_settings::{Definition, Editor, ScopeKind, Setting, Settings, ValueType, DEFINITIONS};
use bilibi_tenancy::{ChildGrant, Oversight, OversightScope, Scope};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    task::JoinSet,
    time::{timeout, timeout_at, Instant},
};

pub const OVERSIGHT_QUERIES: &str = "base/grid.oversight";

const POLICY_TIMEOUT: Duration = Duration::from_secs(60);
const CLEANUP_TIMEOUT: Duration = Duration::from_millis(250);
const TOTAL_TIMEOUT: &str = "oversight total time bound exceeded";
const CHILD_TIMEOUT: &str = "oversight child read timed out";
const SUMMARY_TIMEOUT: &str = "oversight summary audit timed out";

#[derive(Clone, Debug)]
pub enum OversightSpec {
    Summary(SummarySpec),
    /// Totals of a Rows report must exclude aggregates-only grants too.
    RowsSummary(SummarySpec),
    Rows {
        page: PageSpec,
        columns: Vec<String>,
        /// Per-child pages for a caller composing one globally bounded page.
        /// Each entry is (offset, limit). Missing children return only their
        /// count, without reading or projecting keys. IDs never widen consent.
        pages: BTreeMap<i64, (usize, usize)>,
    },
}
impl OversightSpec {
    fn rows(&self) -> bool {
        matches!(self, Self::Rows { .. } | Self::RowsSummary(_))
    }
    fn bounded(&self, millis: u32) -> Self {
        let mut spec = self.clone();
        let bounds = match &mut spec {
            Self::Summary(s) | Self::RowsSummary(s) => &mut s.bounds,
            Self::Rows { page, .. } => &mut page.bounds,
        };
        bounds.statement_ms = bounds.statement_ms.min(millis);
        bounds.max_cost = bounds.max_cost.min(crate::MAX_ESTIMATED_COST);
        spec
    }
    fn evidence(&self) -> (Vec<String>, Vec<String>) {
        match self {
            Self::Summary(s) | Self::RowsSummary(s) => (
                s.groupings
                    .iter()
                    .map(|g| g.column.clone())
                    .chain(s.measures.iter().map(|m| m.column.clone()))
                    .collect(),
                s.filters.iter().map(|f| f.column.clone()).collect(),
            ),
            Self::Rows { page, columns, .. } => (
                columns.clone(),
                page.filters.iter().map(|f| f.column.clone()).collect(),
            ),
        }
    }
    fn bounds(&self) -> Bounds {
        match self {
            Self::Summary(s) | Self::RowsSummary(s) => s.bounds,
            Self::Rows { page, .. } => page.bounds,
        }
    }
}
#[derive(Clone, Debug)]
pub enum OversightData {
    Summary(Summary),
    Rows { projection: Projection, total: i64 },
}
#[derive(Debug)]
pub struct ChildResult {
    pub tenant: i64,
    pub name: String,
    pub grant_version: i64,
    /// ok, timed_out, over_cost, revoked, refused. Never database messages.
    pub outcome: &'static str,
    pub data: Option<OversightData>,
}
#[derive(Debug)]
pub struct OversightResult {
    pub children: Vec<ChildResult>,
    /// Summary measures combined over included children, never averages of averages.
    pub total: Option<Line>,
}
impl OversightResult {
    pub fn excluded(&self) -> usize {
        self.children.iter().filter(|c| c.data.is_none()).count()
    }
}

/// The one cross-tenant read facade: bounded, read-only and audited before
/// returning. The bounded process cache is private to each reader and live
/// catalog/version; no rows are persisted in the parent tenant.
#[derive(Clone)]
pub struct OversightQueries {
    db: Db,
    settings: Arc<Settings>,
    tenancy: Arc<Oversight>,
    cache: Arc<Mutex<HashMap<String, (Instant, OversightData)>>>,
}
impl OversightQueries {
    pub fn new(db: Db, settings: Arc<Settings>, tenancy: Arc<Oversight>) -> Self {
        Self {
            db,
            settings,
            tenancy,
            cache: Arc::default(),
        }
    }
    pub async fn prove(&self, scope: &Scope) -> Result<OversightScope, QueryError> {
        let client = self.db.get().await.map_err(Error::from)?;
        Ok(self.tenancy.prove(&**client, scope).await?)
    }
    /// A builder receives a per-child catalog, not a union that might offer a
    /// field another child withheld. Callers can intersect these for one query.
    pub async fn catalog(
        &self,
        scope: &OversightScope,
        child: &ChildGrant,
        rows: bool,
    ) -> Result<Catalog, QueryError> {
        let client = self.db.get().await.map_err(Error::from)?;
        Ok(Catalog::for_oversight(&**client, scope, child, rows).await?)
    }
    /// A bounded intersection for builders; no results or child identities.
    pub async fn builder_catalog(&self, scope: &Scope, rows: bool) -> Result<Catalog, QueryError> {
        let deadline = Instant::now() + POLICY_TIMEOUT;
        let mut client = timeout_at(deadline, self.db.get())
            .await
            .map_err(|_| QueryError::TooHeavy("oversight policy lookup timed out"))?
            .map_err(Error::from)?;
        let result = async {
            let tx = timeout_at(deadline, client.transaction())
                .await
                .map_err(|_| Error::Internal)??;
            let catalog = self.builder_catalog_on(&tx, scope, rows).await?;
            timeout(POLICY_TIMEOUT, tx.commit())
                .await
                .map_err(|_| Error::Internal)??;
            Ok(catalog)
        }
        .await;
        if result.is_err() {
            drop(Connection::take(client));
        }
        result
    }
    pub async fn read(
        &self,
        scope: &OversightScope,
        root: &str,
        spec: &OversightSpec,
    ) -> Result<OversightResult, QueryError> {
        // Bound even pool admission and settings lookup; an unavailable policy
        // cannot leave an unbounded request or return unaudited data.
        timeout(Duration::from_secs(60), async {
            let total_ms = self.setting("total_timeout_ms").await?;
            timeout(
                Duration::from_millis(u64::from(total_ms)),
                self.read_inner(scope, root, spec),
            )
            .await
            .map_err(|_| QueryError::TooHeavy("oversight total time bound exceeded"))?
        })
        .await
        .map_err(|_| QueryError::TooHeavy("oversight policy lookup timed out"))?
    }
    async fn read_inner(
        &self,
        scope: &OversightScope,
        root: &str,
        spec: &OversightSpec,
    ) -> Result<OversightResult, QueryError> {
        // Re-prove the directory too: reusing an old proof never caches grants.
        let scope = self.prove(scope.parent()).await?;
        let max_children = self.setting("max_children").await? as usize;
        let concurrency = self.setting("concurrency").await? as usize;
        let child_ms = self.setting("child_timeout_ms").await?;
        let total_ms = self.setting("total_timeout_ms").await?;
        let cache_seconds = self.setting("cache_seconds").await?;
        if scope.children().len() > max_children {
            return Err(QueryError::Limit(
                "too many consenting children; narrow the grants",
            ));
        }
        let spec = spec.bounded(child_ms);
        spec.bounds().check()?;
        let deadline = Instant::now() + Duration::from_millis(u64::from(total_ms));
        let mut results = BTreeMap::new();
        let mut pending = scope.children().iter().cloned();
        let mut tasks = JoinSet::new();
        loop {
            while tasks.len() < concurrency {
                let Some(grant) = pending.next() else {
                    break;
                };
                let facade = self.clone();
                let proof = scope.clone();
                let root = root.to_owned();
                let query = spec.clone();
                tasks.spawn(async move {
                    let result = timeout(
                        Duration::from_millis(u64::from(child_ms)),
                        facade.child(&proof, &grant, &root, &query, cache_seconds),
                    )
                    .await;
                    let (outcome, data) = match result {
                        Ok(Ok(data)) => ("ok", Some(data)),
                        Err(_) => ("timed_out", None),
                        Ok(Err(error)) => (child_refusal(&error), None),
                    };
                    ChildResult {
                        tenant: grant.tenant(),
                        name: grant.name().to_owned(),
                        grant_version: grant.version(),
                        outcome,
                        data,
                    }
                });
            }
            if tasks.is_empty() {
                break;
            }
            match timeout_at(deadline, tasks.join_next()).await {
                Ok(Some(Ok(child))) => {
                    results.insert(child.tenant, child);
                }
                Ok(Some(Err(_))) => return Err(Error::Internal.into()),
                Ok(None) => break,
                Err(_) => {
                    tasks.abort_all();
                    break;
                }
            }
        }
        for grant in scope.children() {
            results
                .entry(grant.tenant())
                .or_insert_with(|| ChildResult {
                    tenant: grant.tenant(),
                    name: grant.name().to_owned(),
                    grant_version: grant.version(),
                    outcome: "timed_out",
                    data: None,
                });
        }
        let mut result = OversightResult {
            children: results.into_values().collect(),
            total: None,
        };
        if let OversightSpec::Summary(spec) | OversightSpec::RowsSummary(spec) = &spec {
            result.total = Some(self.combine(&result.children, spec).await?);
        }
        let client = self.db.get().await.map_err(Error::from)?;
        let payload = query_payload(
            &scope,
            root,
            result
                .children
                .iter()
                .filter_map(|child| child.data.as_ref())
                .map(OversightData::count)
                .sum(),
            result
                .children
                .iter()
                .map(|child| (child.tenant, child.outcome)),
        );
        record_query(&**client, &scope, payload).await?;
        Ok(result)
    }
    async fn child(
        &self,
        scope: &OversightScope,
        grant: &ChildGrant,
        root: &str,
        spec: &OversightSpec,
        cache_seconds: u32,
    ) -> Result<OversightData, QueryError> {
        let mut client = self.db.get().await.map_err(Error::from)?;
        let catalog = Catalog::for_oversight(&**client, scope, grant, spec.rows()).await?;
        // Debug here is an internal key, never logged or exposed. It includes
        // field restrictions/masks and all resolved source/link declarations.
        let policy = format!("{catalog:?}");
        let key = format!(
            "{}:{}:{}:{}:{root}:{spec:?}:{policy}",
            scope.parent().tenant(),
            scope.parent().user(),
            grant.tenant(),
            grant.version()
        );
        let cached = if cache_seconds == 0 {
            None
        } else {
            self.cache
                .lock()
                .map_err(|_| Error::Internal)?
                .get(&key)
                .filter(|(at, _)| at.elapsed() < Duration::from_secs(u64::from(cache_seconds)))
                .map(|(_, data)| data.clone())
        };
        let cache_hit = cached.is_some();
        let data = if let Some(data) = cached {
            data
        } else {
            let tx = client
                .build_transaction()
                .read_only(true)
                .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
                .start()
                .await?;
            tx.batch_execute(&spec.bounds().sql()).await?;
            let data = read_catalog(&tx, &catalog, grant, root, spec).await?;
            tx.commit().await?;
            data
        };
        audit_child(&**client, scope, grant, root, spec, &policy, &data).await?;
        if cache_seconds > 0 && !cache_hit {
            let mut cache = self.cache.lock().map_err(|_| Error::Internal)?;
            cache.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(u64::from(cache_seconds)));
            if cache.len() >= 128 {
                cache.clear();
            }
            cache.insert(key, (Instant::now(), data.clone()));
        }
        Ok(data)
    }
    async fn combine(
        &self,
        children: &[ChildResult],
        spec: &SummarySpec,
    ) -> Result<Line, QueryError> {
        let summaries: Vec<&Summary> = children
            .iter()
            .filter_map(|c| match &c.data {
                Some(OversightData::Summary(s)) => Some(s),
                _ => None,
            })
            .collect();
        let mut values = Vec::new();
        let mut client = self.db.get().await.map_err(Error::from)?;
        let tx = client.build_transaction().read_only(true).start().await?;
        tx.batch_execute(&Bounds::default().sql()).await?;
        for (i, measure) in spec.measures.iter().enumerate() {
            let parts: Value = summaries
                .iter()
                .map(|s| match &s.averages[i] {
                    Some((sum, count)) => json!({"value":sum,"count":count}),
                    None => json!({"value":s.combine_values[i],"count":0}),
                })
                .collect();
            let numeric = summaries
                .first()
                .is_none_or(|s| s.measures[i].0.ty.number());
            let expression = match measure.agg {
                Agg::Count => {
                    "coalesce(to_jsonb(sum((v->>'value')::numeric)), '0'::jsonb)".to_owned()
                }
                Agg::Sum => "to_jsonb(sum((v->>'value')::numeric))".into(),
                Agg::Avg => {
                    "to_jsonb(sum((v->>'value')::numeric)/nullif(sum((v->>'count')::bigint),0))"
                        .into()
                }
                Agg::Min | Agg::Max => format!(
                    "to_jsonb({}((v->>'value'){}))",
                    measure.agg.key(),
                    if numeric { "::numeric" } else { "" }
                ),
                _ => {
                    // Ordered values have no cross-child ordering contract.
                    // Keep them in each child's summary, not a fabricated total.
                    values.push(Value::Null);
                    continue;
                }
            };
            let value: Option<Value> = tx
                .query_one(
                    &format!("SELECT {expression} FROM jsonb_array_elements($1::jsonb) v"),
                    &[&parts],
                )
                .await?
                .get(0);
            values.push(value.unwrap_or(Value::Null));
        }
        tx.commit().await?;
        Ok(Line {
            keys: vec![],
            subtotal: false,
            rows: summaries.iter().map(|s| s.total.rows).sum(),
            values,
        })
    }
    async fn setting(&self, suffix: &str) -> Result<u32, QueryError> {
        let client = self.db.get().await.map_err(Error::from)?;
        self.setting_on(&**client, suffix).await
    }
}

macro_rules! bound {
    ($key:literal, $label:literal, $default:literal, $min:literal, $max:literal) => {
        inventory::submit! { bilibi_module_registry::Contribution::new(DEFINITIONS, concat!("tenancy.oversight.",$key),
            bilibi_module_registry::Provenance { module:"base/grid", source:file!(), line:line!() }, &Setting::Definition(Definition {
                value_type:ValueType::Integer,scopes:&[ScopeKind::Global],default:||json!($default),nullable:false,encrypted:false,minimum:Some($min as f64),maximum:Some($max as f64),validator:None,
                editor:Some(Editor { group:"Parent oversight",label:$label,help:"Limits each read across consenting child tenants.",capability:None,reveal_duration_ms:15000 }),
            })) }
    };
}
bound!("max_children", "Maximum children per read", 50, 1, 500);
bound!("concurrency", "Concurrent child reads", 4, 1, 16);
bound!(
    "cache_seconds",
    "Reader cache lifetime (seconds; 0 disables)",
    60,
    0,
    3600
);
bound!(
    "child_timeout_ms",
    "Child read timeout (ms)",
    2000,
    100,
    30000
);
bound!(
    "total_timeout_ms",
    "Total read timeout (ms)",
    10000,
    100,
    60000
);

pub(crate) fn service(
    services: &bilibi_module_registry::Services,
) -> bilibi_module_registry::Result<bilibi_module_registry::Value> {
    Ok(Arc::new(OversightQueries::new(
        services.get::<Db>("base/database.pool")?.as_ref().clone(),
        services.get("base/settings.store")?,
        services.get(bilibi_tenancy::OVERSIGHT)?,
    )))
}

async fn read_catalog(
    tx: &tokio_postgres::Transaction<'_>,
    catalog: &Catalog,
    grant: &ChildGrant,
    root: &str,
    spec: &OversightSpec,
) -> Result<OversightData, QueryError> {
    Ok(match spec {
        OversightSpec::Summary(spec) | OversightSpec::RowsSummary(spec) => {
            let summary = crate::summary::summarize_catalog(tx, catalog, root, spec).await?;
            OversightData::Summary(summary)
        }
        OversightSpec::Rows {
            page,
            columns,
            pages,
        } => {
            let mut page = page.clone();
            let (keys, total) = if let Some((offset, limit)) = pages.get(&grant.tenant()) {
                page.offset = *offset;
                page.limit = *limit;
                let keys = crate::summary::page_keys_catalog(tx, catalog, root, &page).await?;
                (keys.keys, keys.total)
            } else {
                let summary = crate::summary::summarize_catalog(
                    tx,
                    catalog,
                    root,
                    &SummarySpec {
                        bounds: page.bounds,
                        filters: page.filters.clone(),
                        groupings: vec![],
                        measures: vec![],
                        timezone: page.timezone.clone(),
                        max_groups: 1,
                    },
                )
                .await?;
                (vec![], summary.total.rows)
            };
            let projection = crate::query::project_catalog(
                tx,
                catalog,
                root,
                &keys,
                columns,
                &Lenses::default(),
                page.bounds,
            )
            .await?;
            OversightData::Rows { projection, total }
        }
    })
}

impl OversightQueries {
    /// The same bounded builder intersection on the caller's connection.
    pub async fn builder_catalog_on(
        &self,
        tx: &tokio_postgres::Transaction<'_>,
        scope: &Scope,
        rows: bool,
    ) -> Result<Catalog, QueryError> {
        let deadline = self.deadline_on(tx).await?;
        budgeted(tx, deadline, TOTAL_TIMEOUT, None, async {
            let proof = self.tenancy.prove(tx, scope).await?;
            if proof.children().len() > self.setting_on(tx, "max_children").await? as usize {
                return Err(QueryError::Limit("too many consenting children"));
            }
            Ok(Catalog::oversight_builder(tx, &proof, rows).await?)
        })
        .await?
    }
    /// Validate a saved question on the caller's transaction. Each child read
    /// is bounded, audited, and SQL refusals recover at a savepoint. No child
    /// data leaves this probe. For rows, sample the given page from each child.
    pub async fn probe_on(
        &self,
        tx: &tokio_postgres::Transaction<'_>,
        scope: &Scope,
        root: &str,
        spec: &OversightSpec,
    ) -> Result<bool, QueryError> {
        let deadline = self.deadline_on(tx).await?;
        let (scope, child_ms) = budgeted(tx, deadline, TOTAL_TIMEOUT, None, async {
            let scope = self.tenancy.prove(tx, scope).await?;
            if scope.children().len() > self.setting_on(tx, "max_children").await? as usize {
                return Err(QueryError::Limit("too many consenting children"));
            }
            Ok((scope, self.setting_on(tx, "child_timeout_ms").await?))
        })
        .await??;
        let mut audit = Some(
            timeout_at(deadline, self.db.try_get_detached("oversight audit"))
                .await
                .map_err(|_| QueryError::TooHeavy(TOTAL_TIMEOUT))?
                .map_err(Error::from)?,
        );
        let mut spec = spec.bounded(child_ms);
        if let OversightSpec::Rows { page, pages, .. } = &mut spec {
            *pages = scope
                .children()
                .iter()
                .map(|child| (child.tenant(), (page.offset, page.limit)))
                .collect();
        }
        let mut supported = scope.children().is_empty();
        let mut children = Vec::new();
        let mut rows = 0;
        let mut failure = None;
        let child_budget = Duration::from_millis(u64::from(child_ms));
        for grant in scope.children() {
            let start = Instant::now();
            if start >= deadline {
                break;
            }
            let child_deadline = deadline.min(start + child_budget);
            let result = budgeted(
                tx,
                child_deadline,
                CHILD_TIMEOUT,
                Some(spec.bounds()),
                async {
                    let catalog = Catalog::for_oversight(tx, &scope, grant, spec.rows()).await?;
                    let data = read_catalog(tx, &catalog, grant, root, &spec).await?;
                    recheck_policy(tx, &scope, grant, &spec, &format!("{catalog:?}")).await?;
                    Ok(data)
                },
            )
            .await;
            let outcome = match result {
                Ok(Ok(data)) => {
                    let audited =
                        audit_before(&mut audit, child_deadline, CHILD_TIMEOUT, async |client| {
                            record_child(client, &scope, grant, root, &spec, &data).await
                        })
                        .await;
                    match audited {
                        Ok(()) => {
                            rows += data.count();
                            Ok("ok")
                        }
                        Err(error) if audit.is_some() => Ok(child_refusal(&error)),
                        Err(error) => Err(error),
                    }
                }
                Ok(Err(error)) => Ok(child_refusal(&error)),
                Err(error) => Err(error.into()),
            };
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => {
                    let outcome = if matches!(error, QueryError::AuditCommitUnknown) {
                        "outcome unknown"
                    } else {
                        child_refusal(&error)
                    };
                    children.push((grant.tenant(), outcome));
                    failure = Some(error);
                    break;
                }
            };
            supported |= matches!(outcome, "ok" | "timed_out" | "over_cost");
            children.push((grant.tenant(), outcome));
        }
        let now = Instant::now();
        let expired = now >= deadline;
        let payload = query_payload(&scope, root, rows, children.into_iter());
        let summary = if audit.is_some() {
            Some(
                audit_before(
                    &mut audit,
                    deadline.max(Instant::now() + child_budget),
                    SUMMARY_TIMEOUT,
                    async |client| record_query(client, &scope, payload.clone()).await,
                )
                .await,
            )
        } else {
            None
        };
        let summary = match summary {
            Some(result @ Err(QueryError::AuditCommitUnknown)) => result,
            Some(result) if audit.is_some() => result,
            _ => {
                let wait = self.db.timeouts().wait.unwrap_or(child_budget);
                let replacement = timeout(wait, self.db.get_detached("oversight audit"))
                    .await
                    .map_err(|_| QueryError::TooHeavy("oversight summary audit checkout timed out"))
                    .and_then(|result| result.map_err(|error| Error::from(error).into()));
                match replacement {
                    Ok(client) => {
                        audit = Some(client);
                        audit_before(
                            &mut audit,
                            deadline.max(Instant::now() + child_budget),
                            SUMMARY_TIMEOUT,
                            async |client| record_query(client, &scope, payload).await,
                        )
                        .await
                    }
                    Err(error) => {
                        eprintln!(
                            "{}",
                            json!({
                                "level": "error",
                                "event": "tenancy.oversight.query",
                                "failure": "replacement_checkout",
                                "tenant_id": scope.parent().tenant(),
                                "actor_id": scope.parent().user(),
                                "trace_id": scope.parent().agent().map(|agent| agent.trace()),
                                "error": error.to_string(),
                                "payload": payload,
                            })
                        );
                        Err(error)
                    }
                }
            }
        };
        match (failure, summary) {
            (Some(prior), Err(summary)) => Err(QueryError::AuditSummaryFailed {
                prior: Box::new(prior),
                summary: Box::new(summary),
            }),
            (_, Err(error)) | (Some(error), Ok(())) => Err(error),
            (None, Ok(())) if expired => Err(QueryError::TooHeavy(TOTAL_TIMEOUT)),
            (None, Ok(())) => Ok(supported),
        }
    }

    async fn deadline_on(
        &self,
        tx: &tokio_postgres::Transaction<'_>,
    ) -> Result<Instant, QueryError> {
        let start = Instant::now();
        let millis = budgeted(
            tx,
            start + POLICY_TIMEOUT,
            "oversight policy lookup timed out",
            None,
            self.setting_on(tx, "total_timeout_ms"),
        )
        .await??;
        let deadline = start + Duration::from_millis(u64::from(millis));
        if Instant::now() >= deadline {
            return Err(QueryError::TooHeavy(TOTAL_TIMEOUT));
        }
        Ok(deadline)
    }

    async fn setting_on(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        suffix: &str,
    ) -> Result<u32, QueryError> {
        Ok(self
            .settings
            .get_global(client, &format!("tenancy.oversight.{suffix}"))
            .await?
            .as_u64()
            .ok_or(Error::Internal)? as u32)
    }
}

impl OversightData {
    fn count(&self) -> i64 {
        match self {
            Self::Summary(summary) => summary.total.rows,
            Self::Rows { projection, .. } => projection.rows.len() as i64,
        }
    }
}

fn child_refusal(error: &QueryError) -> &'static str {
    match error {
        QueryError::Database(Error::Forbidden)
        | QueryError::Catalog(crate::CatalogError::Database(Error::Forbidden)) => "revoked",
        QueryError::TooHeavy(why) if why.contains("time") => "timed_out",
        QueryError::TooHeavy(_) => "over_cost",
        _ => "refused",
    }
}

async fn budgeted<T>(
    tx: &tokio_postgres::Transaction<'_>,
    deadline: Instant,
    time_error: &'static str,
    bounds: Option<Bounds>,
    work: impl std::future::Future<Output = Result<T, QueryError>>,
) -> Result<Result<T, QueryError>, Error> {
    if Instant::now() >= deadline {
        return Ok(Err(QueryError::TooHeavy(time_error)));
    }
    if let Some(Err(error)) = bounds.map(|bounds| bounds.check()) {
        return Ok(Err(error));
    }
    at_probe_savepoint(tx, deadline, bounds.is_none(), async {
        match timeout_at(deadline, async {
            let previous = if bounds.is_none() {
                Some(
                    tx.query_one("SHOW statement_timeout", &[])
                        .await?
                        .get::<_, String>(0),
                )
            } else {
                None
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(QueryError::TooHeavy(time_error));
            }
            let millis = (remaining.as_millis() + 1) as u32;
            let sql = match bounds {
                Some(mut bounds) => {
                    bounds.statement_ms = bounds.statement_ms.min(millis);
                    bounds.sql()
                }
                None => format!("SET LOCAL statement_timeout = '{millis}ms'"),
            };
            tx.batch_execute(&sql).await?;
            let value = work.await?;
            if let Some(previous) = previous {
                tx.query_one(
                    "SELECT set_config('statement_timeout', $1, true)",
                    &[&previous],
                )
                .await?;
            }
            Ok(value)
        })
        .await
        {
            Ok(result) if Instant::now() < deadline => result.map_err(|error| {
                if sql_cancelled(&error) {
                    QueryError::TooHeavy(time_error)
                } else {
                    error
                }
            }),
            Ok(_) | Err(_) => Err(QueryError::TooHeavy(time_error)),
        }
    })
    .await
}

async fn at_probe_savepoint<T>(
    tx: &tokio_postgres::Transaction<'_>,
    deadline: Instant,
    keep_success: bool,
    work: impl std::future::Future<Output = Result<T, QueryError>>,
) -> Result<Result<T, QueryError>, Error> {
    if !matches!(
        timeout_at(deadline, tx.batch_execute("SAVEPOINT grid_oversight_probe")).await,
        Ok(Ok(()))
    ) {
        return Err(Error::UnrecoveredTransaction(
            "Oversight savepoint setup could not be confirmed.".into(),
        ));
    }
    let result = work.await;
    let cleanup = timeout_at(deadline.max(Instant::now() + CLEANUP_TIMEOUT), async {
        if !keep_success || result.is_err() {
            tx.batch_execute("ROLLBACK TO SAVEPOINT grid_oversight_probe")
                .await?;
        }
        tx.batch_execute("RELEASE SAVEPOINT grid_oversight_probe")
            .await
    })
    .await;
    if !matches!(cleanup, Ok(Ok(()))) {
        return Err(Error::UnrecoveredTransaction(
            "Oversight savepoint recovery could not be confirmed.".into(),
        ));
    }
    Ok(result)
}

fn sql_cancelled(error: &QueryError) -> bool {
    let traced = match error {
        QueryError::Database(Error::Database(traced))
        | QueryError::Catalog(crate::CatalogError::Database(Error::Database(traced))) => traced,
        _ => return false,
    };
    std::error::Error::source(traced)
        .and_then(|source| source.downcast_ref::<tokio_postgres::error::DbError>())
        .is_some_and(|error| error.code() == &tokio_postgres::error::SqlState::QUERY_CANCELED)
}

async fn audit_before(
    audit: &mut Option<Connection>,
    deadline: Instant,
    time_error: &'static str,
    work: impl for<'a, 't> AsyncFnOnce(&'a tokio_postgres::Transaction<'t>) -> Result<(), QueryError>,
) -> Result<(), QueryError> {
    if Instant::now() >= deadline {
        return Err(QueryError::TooHeavy(time_error));
    }
    let mut client = audit.take().ok_or(Error::Internal)?;
    if client.is_closed() {
        drop(Connection::take(client));
        return Err(Error::refused("The oversight audit connection is closed.").into());
    }
    match audit_transaction(&mut client, deadline, time_error, work).await {
        Ok(result) => {
            *audit = Some(client);
            result
        }
        Err(error) => {
            drop(Connection::take(client));
            Err(error)
        }
    }
}

async fn audit_transaction(
    client: &mut tokio_postgres::Client,
    deadline: Instant,
    time_error: &'static str,
    work: impl for<'a, 't> AsyncFnOnce(&'a tokio_postgres::Transaction<'t>) -> Result<(), QueryError>,
) -> Result<Result<(), QueryError>, QueryError> {
    let tx = timeout_at(deadline, client.transaction())
        .await
        .map_err(|_| QueryError::TooHeavy(time_error))??;
    let result = budgeted(&tx, deadline, time_error, None, async {
        work(&tx).await?;
        tx.batch_execute("SET CONSTRAINTS ALL IMMEDIATE").await?;
        Ok(())
    })
    .await
    .map_err(Error::refused)?;
    if result.is_ok() && Instant::now() < deadline {
        return match timeout_at(deadline, tx.commit()).await {
            Ok(Ok(())) => Ok(Ok(())),
            Ok(Err(error))
                if error.as_db_error().is_some_and(|db| {
                    use tokio_postgres::error::{Severity, SqlState};
                    db.parsed_severity() == Some(Severity::Error)
                        && !db.code().code().starts_with("08")
                        && db.code() != &SqlState::T_R_STATEMENT_COMPLETION_UNKNOWN
                }) =>
            {
                Ok(Err(error.into()))
            }
            _ => Err(QueryError::AuditCommitUnknown),
        };
    }
    timeout_at(
        deadline.max(Instant::now() + CLEANUP_TIMEOUT),
        tx.rollback(),
    )
    .await
    .map_err(|_| Error::Internal)??;
    Ok(result.and(Err(QueryError::TooHeavy(time_error))))
}

async fn audit_child(
    client: &(impl tokio_postgres::GenericClient + Sync),
    scope: &OversightScope,
    grant: &ChildGrant,
    root: &str,
    spec: &OversightSpec,
    policy: &str,
    data: &OversightData,
) -> Result<(), QueryError> {
    recheck_policy(client, scope, grant, spec, policy).await?;
    record_child(client, scope, grant, root, spec, data).await
}

async fn recheck_policy(
    client: &(impl tokio_postgres::GenericClient + Sync),
    scope: &OversightScope,
    grant: &ChildGrant,
    spec: &OversightSpec,
    policy: &str,
) -> Result<(), QueryError> {
    let fresh = Catalog::for_oversight(client, scope, grant, spec.rows()).await?;
    if format!("{fresh:?}") != policy {
        return Err(Error::Forbidden.into());
    }
    Ok(())
}

async fn record_child(
    client: &(impl tokio_postgres::GenericClient + Sync),
    scope: &OversightScope,
    grant: &ChildGrant,
    root: &str,
    spec: &OversightSpec,
    data: &OversightData,
) -> Result<(), QueryError> {
    let (mut columns, filters) = spec.evidence();
    if matches!(spec, OversightSpec::Rows { pages, .. } if !pages.contains_key(&grant.tenant())) {
        columns.clear();
    }
    record_oversight_read(
        client,
        scope,
        grant,
        OversightRead {
            table: root,
            level: if spec.rows() { "rows" } else { "aggregates" },
            columns: &columns,
            filters: &filters,
            rows: data.count(),
        },
    )
    .await?;
    Ok(())
}

fn query_payload(
    scope: &OversightScope,
    root: &str,
    rows: i64,
    children: impl Iterator<Item = (i64, &'static str)>,
) -> Value {
    let mut excluded = 0;
    let children: Vec<_> = children
        .map(|(tenant, outcome)| {
            excluded += usize::from(outcome != "ok");
            json!({"tenant_id": tenant, "outcome": outcome})
        })
        .collect();
    json!({
            "table": root,
            "reader_name": scope.parent().user_name,
            "agent_connection": scope.parent().agent().map(|agent| agent.connection()),
            "row_count": rows,
            "children": children,
            "excluded": excluded,
    })
}

async fn record_query(
    client: &(impl tokio_postgres::GenericClient + Sync),
    scope: &OversightScope,
    payload: Value,
) -> Result<(), QueryError> {
    let mut who = Attribution::from_scope(scope.parent());
    who.trace_id = scope.parent().agent().map(|agent| agent.trace().to_owned());
    record_action(
        client,
        &who,
        None,
        "tenancy.oversight.query",
        payload,
        false,
    )
    .await?;
    Ok(())
}
