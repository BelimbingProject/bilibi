//! `base_ai_interactions`: one metadata row per call. Recording is a machine
//! measurement on its own short connection, outside mutation capture, as
//! Integration records an exchange. Reads authorize the list capability on
//! every call.
use crate::{contributions::LIST, Reason};
use bilibi_authz::authorize_key;
use bilibi_database::{Db, Error, Result};
use bilibi_tenancy::Scope;
use std::time::{Duration, SystemTime};
use tokio_postgres::{GenericClient, Row};

pub const PAGE_SIZES: [i64; 4] = [25, 50, 100, 300];
/// The most rows one filter counts and pages through, as for API exchanges.
pub const LIST_CAP: i64 = 10_000;
const COLUMNS: &str = "public_id,tenant_id,user_id,principal,connection_id,impersonator_id,company_id,module,provider,model,input_characters,output_characters,outcome,reason,duration_ms,exchange_id,created_at";

/// One call's metadata. It has no text: not the instructions, not the text
/// sent and not the reply.
#[derive(Clone, Debug)]
pub struct Interaction {
    pub id: String,
    pub tenant_id: i64,
    /// The person, or `None` when a named system principal asked.
    pub user_id: Option<i64>,
    pub principal: Option<String>,
    /// The person's agent connection, when an AI app made the request.
    pub connection_id: Option<i64>,
    pub impersonator_id: Option<i64>,
    /// The scope's company, when it had one.
    pub company_id: Option<i64>,
    /// The calling module.
    pub module: String,
    /// The provider used, or the name asked for when none was configured.
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Characters of instructions and text sent together.
    pub input_characters: i64,
    pub output_characters: Option<i64>,
    /// `generated` or `unavailable`.
    pub outcome: String,
    /// `disabled`, `not_configured`, `timeout` or `provider_error`, for an
    /// unavailable answer.
    pub reason: Option<String>,
    pub duration_ms: i64,
    /// The Integration exchange of the HTTP call, when one was made.
    pub exchange_id: Option<String>,
    pub created_at: SystemTime,
}
impl Interaction {
    fn from_row(row: &Row) -> Self {
        Self {
            id: row.get("public_id"),
            tenant_id: row.get("tenant_id"),
            user_id: row.get("user_id"),
            principal: row.get("principal"),
            connection_id: row.get("connection_id"),
            impersonator_id: row.get("impersonator_id"),
            company_id: row.get("company_id"),
            module: row.get("module"),
            provider: row.get("provider"),
            model: row.get("model"),
            input_characters: row.get("input_characters"),
            output_characters: row.get("output_characters"),
            outcome: row.get("outcome"),
            reason: row.get("reason"),
            duration_ms: row.get("duration_ms"),
            exchange_id: row.get("exchange_id"),
            created_at: row.get("created_at"),
        }
    }
}

/// `generated` or `unavailable`, for the list filter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Generated,
    Unavailable,
}
impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Generated => "generated",
            Self::Unavailable => "unavailable",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        [Self::Generated, Self::Unavailable]
            .into_iter()
            .find(|outcome| outcome.as_str() == value)
    }
}

/// One search covers module, provider, model, person id and principal.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub outcome: Option<Outcome>,
    pub search: String,
}

#[derive(Clone, Debug)]
pub struct Page {
    pub entries: Vec<Interaction>,
    /// Rows the filter matches, counted up to [`LIST_CAP`].
    pub total_entries: i64,
    /// The filter matches more than [`LIST_CAP`] rows.
    pub more: bool,
    pub total_pages: i64,
    pub page: i64,
    pub page_size: i64,
}

/// What one finished call writes.
pub(crate) struct Record<'a> {
    pub public_id: &'a str,
    pub scope: &'a Scope,
    pub module: &'a str,
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    pub input_characters: usize,
    pub output_characters: Option<usize>,
    pub reason: Option<Reason>,
    pub duration: Duration,
    pub exchange_id: Option<&'a str>,
}

/// Writes one row on its own short connection.
pub(crate) async fn record(db: &Db, record: Record<'_>) -> Result<()> {
    let principal = record.scope.principal();
    let user = principal.is_none().then(|| record.scope.user());
    let connection = record.scope.agent().map(|agent| agent.connection());
    let impersonator = record.scope.impersonator().map(|who| who.user());
    let company = record.scope.current_company().map(|company| company.get());
    let count = |characters: usize| i64::try_from(characters).unwrap_or(i64::MAX);
    let client = db.get().await?;
    client
        .execute(
            "INSERT INTO base_ai_interactions(public_id,tenant_id,user_id,principal,connection_id,impersonator_id,company_id,\
             module,provider,model,input_characters,output_characters,outcome,reason,duration_ms,exchange_id) \
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)",
            &[
                &record.public_id,
                &record.scope.tenant(),
                &user,
                &principal,
                &connection,
                &impersonator,
                &company,
                &record.module,
                &record.provider,
                &record.model,
                &count(record.input_characters),
                &record.output_characters.map(count),
                &if record.reason.is_some() { "unavailable" } else { "generated" },
                &record.reason.map(Reason::as_str),
                &i64::try_from(record.duration.as_millis()).unwrap_or(i64::MAX),
                &record.exchange_id,
            ],
        )
        .await?;
    Ok(())
}

/// `None` widens to every tenant: only the sealed platform operator scope
/// may do that, and it never bypasses the capability.
async fn visible_tenant(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
) -> Result<Option<i64>> {
    match bilibi_tenancy::prove_operator(client, scope).await {
        Ok(_) => Ok(None),
        Err(Error::Forbidden) => Ok(Some(scope.tenant())),
        Err(error) => Err(error),
    }
}

pub(crate) async fn list(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    filter: &Filter,
    page: i64,
    page_size: i64,
) -> Result<Page> {
    authorize_key(client, scope, LIST).await?;
    if page < 1 || !PAGE_SIZES.contains(&page_size) || filter.search.len() > 200 {
        return Err(Error::Invalid);
    }
    let tenant = visible_tenant(client, scope).await?;
    let pattern = {
        let search = filter.search.trim();
        (!search.is_empty()).then(|| {
            let escaped = search
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("%{escaped}%")
        })
    };
    let outcome = filter.outcome.map(Outcome::as_str);
    // One tenant's rows are an equality the timeline index serves; the
    // operator's every-tenant read names no tenant at all.
    let tenant_sql = if tenant.is_some() {
        "tenant_id=$1"
    } else {
        "$1::bigint IS NULL"
    };
    let where_ = format!(
        "{tenant_sql} AND ($2::text IS NULL OR outcome=$2) \
         AND ($3::text IS NULL OR module ILIKE $3 OR provider ILIKE $3 OR model ILIKE $3 \
              OR user_id::text ILIKE $3 OR principal ILIKE $3)"
    );
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 3] = [&tenant, &outcome, &pattern];
    let counted: i64 = client
        .query_one(
            &format!("SELECT count(*) FROM (SELECT 1 FROM base_ai_interactions WHERE {where_} LIMIT {}) AS capped", LIST_CAP + 1),
            &params,
        )
        .await?
        .get(0);
    let total = counted.min(LIST_CAP);
    let offset = (page - 1).checked_mul(page_size).ok_or(Error::Invalid)?;
    let rows = if offset >= LIST_CAP {
        Vec::new()
    } else {
        let mut paged = params.to_vec();
        paged.push(&page_size);
        paged.push(&offset);
        client
            .query(
                &format!("SELECT {COLUMNS} FROM base_ai_interactions WHERE {where_} ORDER BY created_at DESC,id DESC LIMIT $4 OFFSET $5"),
                &paged,
            )
            .await?
    };
    Ok(Page {
        entries: rows.iter().map(Interaction::from_row).collect(),
        total_entries: total,
        more: counted > LIST_CAP,
        total_pages: (total + page_size - 1) / page_size,
        page,
        page_size,
    })
}

pub(crate) async fn get(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    id: &str,
) -> Result<Interaction> {
    authorize_key(client, scope, LIST).await?;
    let tenant = visible_tenant(client, scope).await?;
    client
        .query_opt(
            &format!("SELECT {COLUMNS} FROM base_ai_interactions WHERE public_id=$1 AND ($2::bigint IS NULL OR tenant_id=$2)"),
            &[&id, &tenant],
        )
        .await?
        .map(|row| Interaction::from_row(&row))
        .ok_or(Error::NotFound)
}

/// Deletes rows older than `days` before `now`, oldest first, in batches,
/// and answers how many went. Like recording, expiry writes no audit row.
pub(crate) async fn prune(
    client: &(impl GenericClient + Sync),
    days: i64,
    now: SystemTime,
) -> Result<u64> {
    if !(1..=crate::contributions::RETENTION_MAX).contains(&days) {
        return Err(Error::Invalid);
    }
    let cutoff = now - Duration::from_secs(days as u64 * 86_400);
    let mut pruned = 0_u64;
    loop {
        let deleted = client
            .execute(
                "DELETE FROM base_ai_interactions WHERE id IN (SELECT id FROM base_ai_interactions WHERE created_at<$1 ORDER BY created_at,id LIMIT 500)",
                &[&cutoff],
            )
            .await?;
        pruned += deleted;
        if deleted < 500 {
            return Ok(pruned);
        }
    }
}
