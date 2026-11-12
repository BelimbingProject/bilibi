//! Read-only SQL console: text guards, a PostgreSQL `READ ONLY` transaction,
//! and one action row per command whatever its outcome. The record is the
//! control: an unrecorded command returns no answer.
use crate::{bounded_text, record_action, Attribution, RequestInfo};
use bilibi_database::{Db, Error, Result};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use tokio_postgres::{error::SqlState, Client, IsolationLevel};

mod parameters;
pub use parameters::{extract_named_parameters, Parameters};

pub const MAX_ROWS: i64 = 1000;
pub const TIMEOUT_MS: i64 = 10_000;
const FORBIDDEN: [&str; 12] = [
    "INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "CREATE", "TRUNCATE", "REPLACE", "GRANT",
    "REVOKE", "LOCK", "UNLOCK",
];
/// Tables the console never reads. `sessions` (`base/session`) is every
/// signed-in browser of every tenant; its id is only the cookie's digest, but
/// no console should list who is signed in where. Text guards are advisory:
/// hashing the token at rest is what keeps a reader from using a session.
const HIDDEN_TABLES: [&str; 1] = ["sessions"];
/// Functions that run SQL given as text, or print whole tables or schemas,
/// so a hidden table would be read without being named.
fn reads_unnamed_tables(name: &str) -> bool {
    name.ends_with("_to_xml")
        || name.ends_with("_to_xmlschema")
        || name.ends_with("_to_xml_and_xmlschema")
        || name.starts_with("dblink")
        || matches!(name, "ts_stat" | "ts_rewrite")
}

#[derive(Clone, Debug)]
pub struct ConsoleRequest<'a> {
    pub sql: &'a str,
    /// A saved query's name, recorded beside the text; never executed.
    pub name: Option<&'a str>,
    pub page: i64,
    pub per_page: i64,
}
#[derive(Clone, Debug)]
pub struct ConsoleResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub total: i64,
    pub page: i64,
    pub per_page: i64,
    pub last_page: i64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Guard {
    Operator,
    Empty,
    Statement,
    Keyword,
    /// A hidden table, or a way to read one without naming it.
    Table,
    ReadOnlyTransaction,
    Capability,
    Actor,
    Bounds,
}
impl Guard {
    pub fn key(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Empty => "empty",
            Self::Statement => "statement",
            Self::Keyword => "keyword",
            Self::Table => "table",
            Self::ReadOnlyTransaction => "read_only_transaction",
            Self::Capability => "capability",
            Self::Actor => "actor",
            Self::Bounds => "bounds",
        }
    }
}
/// What the record says became of the command: a count, never its rows.
#[derive(Clone, Debug)]
pub enum Outcome {
    Succeeded { row_count: i64 },
    Refused { guard: Guard, message: String },
    Failed { message: String },
}
impl Outcome {
    pub fn event(&self) -> &'static str {
        match self {
            Self::Succeeded { .. } => "database_query.executed",
            Self::Refused { .. } => "database_query.refused",
            Self::Failed { .. } => "database_query.failed",
        }
    }
}
#[derive(Debug)]
pub struct ConsoleAnswer {
    pub outcome: Outcome,
    pub result: Option<ConsoleResult>,
    pub action_id: i64,
}

/// Operator identity is proven here and its refusal recorded, like every
/// other guard. Capability checks belong to the screen that offers the console.
pub async fn execute_readonly(
    db: &Db,
    scope: &Scope,
    request: ConsoleRequest<'_>,
    info: Option<&RequestInfo>,
) -> Result<ConsoleAnswer> {
    execute_readonly_with_options(db, scope, request, &ConsoleOptions::default(), info).await
}

/// Named values travel through PostgreSQL bind parameters, never SQL text or
/// audit payloads. Sorting is restricted to a column returned by the query.
#[derive(Clone, Debug, Default)]
pub struct ConsoleOptions {
    pub parameters: Parameters,
    pub order_by: Option<String>,
    pub descending: bool,
}

pub async fn execute_readonly_with_options(
    db: &Db,
    scope: &Scope,
    request: ConsoleRequest<'_>,
    options: &ConsoleOptions,
    info: Option<&RequestInfo>,
) -> Result<ConsoleAnswer> {
    let execution = async {
        let mut client = db.get().await?;
        run(&mut client, scope, &request, options).await
    }
    .await;
    let (outcome, result) = match execution {
        Ok(answer) => answer,
        // Connection, operator-proof and transaction setup errors also count as
        // attempts. If the recorder is unavailable, fail closed with no rows.
        Err(error) => (
            Outcome::Failed {
                message: error.to_string(),
            },
            None,
        ),
    };
    let action_id = record_outcome(db, scope, &request, &outcome, info).await?;
    Ok(ConsoleAnswer {
        outcome,
        result,
        action_id,
    })
}

/// The service records authorization refusals through the same payload builder.
/// This API accepts counts/outcomes only and cannot receive result rows.
pub async fn record_outcome(
    db: &Db,
    scope: &Scope,
    request: &ConsoleRequest<'_>,
    outcome: &Outcome,
    info: Option<&RequestInfo>,
) -> Result<i64> {
    let mut payload = json!({"sql": bounded_text(request.sql)});
    if let Some(name) = request.name {
        payload["name"] = Value::String(bounded_text(name));
    }
    match outcome {
        Outcome::Succeeded { row_count } => {
            payload["result"] = json!("succeeded");
            payload["row_count"] = json!(row_count);
        }
        Outcome::Refused { guard, .. } => {
            payload["result"] = json!("refused");
            payload["guard"] = json!(guard.key());
            payload["message"] = json!(format!("Query refused by {} guard.", guard.key()));
        }
        Outcome::Failed { .. } => {
            payload["result"] = json!("failed");
            // PostgreSQL errors can quote parameter values or data raised by a
            // function. Return details to the operator, never persist them.
            payload["message"] = json!("Query execution failed.");
        }
    }
    // The execution connection has been rolled back and released. Recording on
    // a separate checkout also survives a connection broken by the command.
    let client = db.get().await?;
    record_action(
        &**client,
        &Attribution::from_scope(scope),
        info,
        outcome.event(),
        payload,
        false,
    )
    .await
}

async fn run(
    client: &mut Client,
    scope: &Scope,
    request: &ConsoleRequest<'_>,
    options: &ConsoleOptions,
) -> Result<(Outcome, Option<ConsoleResult>)> {
    let refused = |guard: Guard, message: &str| {
        Ok((
            Outcome::Refused {
                guard,
                message: message.to_owned(),
            },
            None,
        ))
    };
    match bilibi_tenancy::prove_operator(&*client, scope).await {
        Ok(_) => {}
        Err(Error::Forbidden) => {
            return refused(
                Guard::Operator,
                "The database console is restricted to the platform operator.",
            )
        }
        Err(error) => return Err(error),
    }
    let sql = request.sql.trim().trim_end_matches(';').trim();
    if sql.is_empty() {
        return refused(Guard::Empty, "Query cannot be empty.");
    }
    if sql.len() > 100_000
        || options.parameters.len() > 1000
        || options
            .parameters
            .values()
            .flatten()
            .any(|value| value.len() > 100_000)
    {
        return refused(Guard::Bounds, "Query or parameter input is too large.");
    }
    let stripped = parameters::code_only(sql);
    let mut words = stripped
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_uppercase);
    if !matches!(words.next().as_deref(), Some("SELECT" | "WITH")) {
        return refused(
            Guard::Statement,
            "Only SELECT or WITH queries are permitted.",
        );
    }
    if words.any(|word| FORBIDDEN.contains(&word.as_str())) {
        return refused(
            Guard::Keyword,
            "Write or DDL statements are not permitted in queries.",
        );
    }
    for name in parameters::names(sql) {
        let Some(name) = name else {
            return refused(
                Guard::Table,
                "Write identifiers as plain or double-quoted names, not U&\"...\".",
            );
        };
        if HIDDEN_TABLES.contains(&name.as_str()) {
            return refused(
                Guard::Table,
                "Queries cannot name the sessions table: it holds every signed-in browser.",
            );
        }
        if reads_unnamed_tables(&name) {
            return refused(
                Guard::Table,
                "Functions that run SQL text or print whole tables are not permitted in queries.",
            );
        }
    }
    let page = request.page.max(1);
    let per_page = request.per_page.clamp(1, MAX_ROWS);
    if (page - 1).checked_mul(per_page).is_none() {
        return refused(Guard::Bounds, "Result page is too large.");
    }
    let bound = parameters::bind(sql, &options.parameters);
    match query(client, &bound, page, per_page, options).await {
        Ok(result) => Ok((
            Outcome::Succeeded {
                row_count: result.total,
            },
            Some(result),
        )),
        Err(error) => match error.as_db_error() {
            // Text guards are advisory; PostgreSQL's read-only mode is the boundary.
            Some(db) if *db.code() == SqlState::READ_ONLY_SQL_TRANSACTION => {
                refused(Guard::ReadOnlyTransaction, db.message())
            }
            Some(db) => Ok((
                Outcome::Failed {
                    message: db.message().to_owned(),
                },
                None,
            )),
            None => Err(error.into()),
        },
    }
}

async fn query(
    client: &mut Client,
    bound: &parameters::Bound,
    page: i64,
    per_page: i64,
    options: &ConsoleOptions,
) -> std::result::Result<ConsoleResult, tokio_postgres::Error> {
    let tx = client
        .build_transaction()
        .read_only(true)
        .isolation_level(IsolationLevel::RepeatableRead)
        .start()
        .await?;
    let result = query_in_transaction(&tx, bound, page, per_page, options).await;
    // Await rollback even after a SQL failure, before the connection is reused.
    let rollback = tx.rollback().await;
    match result {
        Err(error) => Err(error),
        Ok(result) => {
            rollback?;
            Ok(result)
        }
    }
}

async fn query_in_transaction(
    tx: &tokio_postgres::Transaction<'_>,
    bound: &parameters::Bound,
    page: i64,
    per_page: i64,
    options: &ConsoleOptions,
) -> std::result::Result<ConsoleResult, tokio_postgres::Error> {
    tx.batch_execute(&format!("SET LOCAL statement_timeout = {TIMEOUT_MS}"))
        .await?;
    let sql = &bound.sql;
    let params = bound.values();
    let total: i64 = tx
        .query_one(
            &format!("SELECT count(*) FROM ({sql}\n) AS __blb_count"),
            &params,
        )
        .await?
        .get(0);
    let columns: Vec<String> = tx
        .prepare(&format!("SELECT * FROM ({sql}\n) AS __blb_sample LIMIT 0"))
        .await?
        .columns()
        .iter()
        .map(|column| column.name().to_owned())
        .collect();
    let order = options
        .order_by
        .as_ref()
        .and_then(|column| columns.iter().position(|candidate| candidate == column))
        .map(|index| {
            format!(
                " ORDER BY __blb_view.\"c{index}\" {}",
                if options.descending { "DESC" } else { "ASC" }
            )
        })
        .unwrap_or_default();
    // Unique positional aliases preserve duplicate SQL column names and
    // their values. row_to_json otherwise collapses duplicate object keys.
    let aliases = if columns.is_empty() {
        String::new()
    } else {
        format!(
            "({})",
            (0..columns.len())
                .map(|i| format!("\"c{i}\""))
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    let page_sql = format!(
        "SELECT row_to_json(__blb_view) FROM ({sql}\n) AS __blb_view{aliases}{order} LIMIT {per_page} OFFSET {}",
        (page - 1) * per_page,
    );
    let raw_rows = tx.query(&page_sql, &params).await?;
    let rows = raw_rows
        .iter()
        .map(|row| {
            let mut object: Value = row.try_get(0)?;
            Ok((0..columns.len())
                .map(|i| {
                    object
                        .get_mut(format!("c{i}"))
                        .map(Value::take)
                        .unwrap_or(Value::Null)
                })
                .collect())
        })
        .collect::<std::result::Result<Vec<_>, tokio_postgres::Error>>()?;
    Ok(ConsoleResult {
        columns,
        rows,
        total,
        page,
        per_page,
        last_page: (total / per_page + i64::from(total % per_page != 0)).max(1),
    })
}
