//! Runs, row results and saved mappings as stored. Every statement carries
//! the tenant, and a run is read only by the person who started it.
use crate::mapping::Mapping;
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use jiff::Timestamp;
use serde::Serialize;
use serde_json::{json, Value};
use tokio_postgres::{GenericClient, Row};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Uploaded; the mapping can change and either pass can start.
    Draft,
    Previewing,
    Previewed,
    Running,
    Completed,
    Undoing,
    Undone,
    Cancelled,
}
impl RunState {
    fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "draft" => Self::Draft,
            "previewing" => Self::Previewing,
            "previewed" => Self::Previewed,
            "running" => Self::Running,
            "completed" => Self::Completed,
            "undoing" => Self::Undoing,
            "undone" => Self::Undone,
            "cancelled" => Self::Cancelled,
            _ => return Err(Error::Internal),
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Previewing => "previewing",
            Self::Previewed => "previewed",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Undoing => "undoing",
            Self::Undone => "undone",
            Self::Cancelled => "cancelled",
        }
    }
    /// Whether the mapping may change and a pass may start.
    pub fn open(self) -> bool {
        matches!(self, Self::Draft | Self::Previewed)
    }
    /// Whether a job is working through the file.
    pub fn working(self) -> bool {
        matches!(self, Self::Previewing | Self::Running | Self::Undoing)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UndoOutcome {
    Removed,
    Edited,
    Error,
}
impl UndoOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Removed => "removed",
            Self::Edited => "edited",
            Self::Error => "error",
        }
    }
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "removed" => Some(Self::Removed),
            "edited" => Some(Self::Edited),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct UndoCounts {
    pub removed: i64,
    pub edited: i64,
    pub error: i64,
}
impl UndoCounts {
    pub fn done(&self) -> i64 {
        self.removed + self.edited + self.error
    }
    pub fn left(&self) -> i64 {
        self.edited + self.error
    }
}

/// The dry run writes nothing; the real run writes through the contracts.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Pass {
    Preview,
    Run,
}
impl Pass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Run => "run",
        }
    }
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "preview" => Some(Self::Preview),
            "run" => Some(Self::Run),
            _ => None,
        }
    }
}

/// What happened to one row. In a dry run, what would happen.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Create,
    Update,
    Skip,
    Action,
    Error,
}
impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Skip => "skip",
            Self::Action => "action",
            Self::Error => "error",
        }
    }
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "create" => Some(Self::Create),
            "update" => Some(Self::Update),
            "skip" => Some(Self::Skip),
            "action" => Some(Self::Action),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Counts {
    pub create: i64,
    pub update: i64,
    pub skip: i64,
    pub action: i64,
    pub error: i64,
}
impl Counts {
    pub fn done(&self) -> i64 {
        self.create + self.update + self.skip + self.action + self.error
    }
}

#[derive(Clone, Debug)]
pub struct Run {
    pub id: i64,
    pub record_type: String,
    pub company: i64,
    pub file_name: String,
    pub header: Vec<String>,
    pub row_count: i32,
    pub mapping: Option<Mapping>,
    pub state: RunState,
    pub pass: Option<Pass>,
    pub job_id: Option<i64>,
    pub trace_id: Option<String>,
    pub version: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub preview: Counts,
    pub run: Counts,
    pub undo: UndoCounts,
    /// The job working on the run ended without finishing it: its person
    /// lost access, or it failed every attempt. Cancel to start again.
    pub stopped: bool,
    /// An owner erased a person whose data this run's file or row details
    /// could hold: the file is gone (or about to go) and each row's source
    /// key and message are blank. Outcomes and record ids stay.
    pub erased: bool,
    pub(crate) next_row: i32,
    pub(crate) undo_next_row: i32,
    pub undo_started_at: Option<Timestamp>,
    pub undo_finished_at: Option<Timestamp>,
    pub(crate) source: Option<String>,
}

/// The only file name an erased import run exposes.
pub(crate) const ERASED_FILE_NAME: &str = "[erased]";

pub(crate) const COLUMNS: &str = "id,record_type,company_id,file_name,source_artifact,header,row_count,mapping,state,pass,next_row,undo_next_row,job_id,trace_id,version,floor(extract(epoch FROM created_at))::bigint AS created,floor(extract(epoch FROM updated_at))::bigint AS updated,floor(extract(epoch FROM finished_at))::bigint AS finished,floor(extract(epoch FROM undo_started_at))::bigint AS undo_started,floor(extract(epoch FROM undo_finished_at))::bigint AS undo_finished,cardinality(erased_for)>0 AS erased";

pub(crate) fn instant(seconds: i64) -> Timestamp {
    Timestamp::from_second(seconds).unwrap_or(Timestamp::UNIX_EPOCH)
}

impl Run {
    pub(crate) fn from_row(row: &Row) -> Result<Self> {
        let erased: bool = row.get("erased");
        let header = if erased {
            Vec::new()
        } else {
            serde_json::from_value(row.get("header")).map_err(|_| Error::Internal)?
        };
        let mapping = if erased {
            None
        } else {
            row.get::<_, Option<Value>>("mapping")
                .map(serde_json::from_value)
                .transpose()
                .map_err(|_| Error::Internal)?
        };
        Ok(Self {
            id: row.get("id"),
            record_type: row.get("record_type"),
            company: row.get("company_id"),
            file_name: if erased {
                ERASED_FILE_NAME.to_owned()
            } else {
                row.get("file_name")
            },
            header,
            row_count: row.get("row_count"),
            mapping,
            state: RunState::parse(row.get("state"))?,
            pass: row
                .get::<_, Option<&str>>("pass")
                .map(|pass| Pass::parse(pass).ok_or(Error::Internal))
                .transpose()?,
            job_id: row.get("job_id"),
            trace_id: row.get("trace_id"),
            version: row.get("version"),
            created_at: instant(row.get("created")),
            updated_at: instant(row.get("updated")),
            finished_at: row.get::<_, Option<i64>>("finished").map(instant),
            preview: Counts::default(),
            run: Counts::default(),
            undo: UndoCounts::default(),
            stopped: false,
            erased,
            next_row: row.get("next_row"),
            undo_next_row: row.get("undo_next_row"),
            undo_started_at: row.get::<_, Option<i64>>("undo_started").map(instant),
            undo_finished_at: row.get::<_, Option<i64>>("undo_finished").map(instant),
            // Not offered once erased, even while the bytes wait for clean-up.
            source: if erased {
                None
            } else {
                row.get("source_artifact")
            },
        })
    }

    /// Rows the current pass has worked through.
    pub fn progress(&self) -> i32 {
        self.next_row
    }

    /// What a contract answers about the run: facts, never the file's data.
    pub fn json(&self, label: &str) -> Value {
        json!({
            "id": self.id,
            "record_type": self.record_type,
            "record_type_label": label,
            "file_name": self.file_name,
            "columns": self.header,
            "rows": self.row_count,
            "state": if self.stopped { "stopped" } else { self.state.as_str() },
            "erased": self.erased,
            "pass": self.pass.map(Pass::as_str),
            "progress": self.next_row,
            "mapping": self.mapping,
            "preview": self.preview,
            "result": self.run,
            "undo": self.undo,
            "trace_id": self.trace_id,
            "version": self.version,
            "created_at": self.created_at.to_string(),
            "updated_at": self.updated_at.to_string(),
            "finished_at": self.finished_at.map(|at| at.to_string()),
            "undo_started_at": self.undo_started_at.map(|at| at.to_string()),
            "undo_finished_at": self.undo_finished_at.map(|at| at.to_string()),
        })
    }
}

/// One row's result.
#[derive(Clone, Debug, Serialize)]
pub struct RowResult {
    pub row: i32,
    pub outcome: Outcome,
    pub record_id: Option<String>,
    pub source_key: Option<String>,
    pub message: Option<String>,
    pub links: Vec<crate::ResultLink>,
    #[serde(skip_serializing)]
    pub(crate) record_version: Option<i64>,
    pub undo_outcome: Option<UndoOutcome>,
    pub undo_message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SavedMapping {
    pub id: i64,
    pub record_type: String,
    pub name: String,
    pub columns: Vec<String>,
    pub mapping: Mapping,
    pub version: i64,
}

/// The person's own run with its counts, optionally locked for an update.
pub(crate) async fn load(
    client: &impl GenericClient,
    scope: &Scope,
    id: i64,
    lock: bool,
) -> Result<Run> {
    let mut run = row(client, scope, id, lock).await?;
    count(client, scope.tenant(), std::slice::from_mut(&mut run)).await?;
    Ok(run)
}

/// The person's own run, locked, without its counts. The import job takes
/// this once per row: `count` reads every recorded row of the run, so
/// counting here would make an import quadratic in its rows.
pub(crate) async fn lock(client: &impl GenericClient, scope: &Scope, id: i64) -> Result<Run> {
    row(client, scope, id, true).await
}

async fn row(client: &impl GenericClient, scope: &Scope, id: i64, lock: bool) -> Result<Run> {
    let company = scope.current_company().ok_or(Error::Forbidden)?.get();
    let sql = format!(
        "SELECT {COLUMNS} FROM base_bulk_data_runs WHERE tenant_id=$1 AND id=$2 AND started_by=$3 AND company_id=$4{}",
        if lock { " FOR UPDATE" } else { "" }
    );
    let row = client
        .query_opt(&sql, &[&scope.tenant(), &id, &scope.user(), &company])
        .await?
        .ok_or(Error::NotFound)?;
    Run::from_row(&row)
}

/// Fills each run's counts and whether its job stopped short.
pub(crate) async fn count(
    client: &impl GenericClient,
    tenant: i64,
    runs: &mut [Run],
) -> Result<()> {
    if runs.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = runs.iter().map(|run| run.id).collect();
    for row in client
        .query(
            "SELECT run_id,pass,outcome,count(*) FROM base_bulk_data_rows WHERE tenant_id=$1 AND run_id=ANY($2) GROUP BY run_id,pass,outcome",
            &[&tenant, &ids],
        )
        .await?
    {
        let Some(run) = runs.iter_mut().find(|run| run.id == row.get::<_, i64>(0)) else {
            continue;
        };
        let counts = if row.get::<_, &str>(1) == "preview" {
            &mut run.preview
        } else {
            &mut run.run
        };
        let n: i64 = row.get(3);
        match Outcome::parse(row.get(2)) {
            Some(Outcome::Create) => counts.create = n,
            Some(Outcome::Update) => counts.update = n,
            Some(Outcome::Skip) => counts.skip = n,
            Some(Outcome::Action) => counts.action = n,
            _ => counts.error = n,
        }
    }
    for row in client
        .query(
            "SELECT run_id,undo_outcome,count(*) FROM base_bulk_data_rows WHERE tenant_id=$1 AND run_id=ANY($2) AND undo_outcome IS NOT NULL GROUP BY run_id,undo_outcome",
            &[&tenant, &ids],
        )
        .await?
    {
        let Some(run) = runs.iter_mut().find(|run| run.id == row.get::<_, i64>(0)) else {
            continue;
        };
        let n: i64 = row.get(2);
        match UndoOutcome::parse(row.get(1)) {
            Some(UndoOutcome::Removed) => run.undo.removed = n,
            Some(UndoOutcome::Edited) => run.undo.edited = n,
            _ => run.undo.error = n,
        }
    }
    let jobs: Vec<i64> = runs
        .iter()
        .filter(|run| run.state.working())
        .filter_map(|run| run.job_id)
        .collect();
    if !jobs.is_empty() {
        for status in bilibi_queue::states(client, tenant, &jobs).await? {
            if status.state.terminal() {
                for run in runs.iter_mut().filter(|run| run.job_id == Some(status.id)) {
                    run.stopped = true;
                }
            }
        }
    }
    Ok(())
}

pub(crate) async fn list(
    client: &impl GenericClient,
    scope: &Scope,
    offset: i64,
    limit: i64,
) -> Result<(Vec<Run>, i64)> {
    let company = scope.current_company().ok_or(Error::Forbidden)?.get();
    let total: i64 = client
        .query_one(
            "SELECT count(*) FROM base_bulk_data_runs WHERE tenant_id=$1 AND started_by=$2 AND company_id=$3",
            &[&scope.tenant(), &scope.user(), &company],
        )
        .await?
        .get(0);
    let mut runs = client
        .query(
            &format!("SELECT {COLUMNS} FROM base_bulk_data_runs WHERE tenant_id=$1 AND started_by=$2 AND company_id=$3 ORDER BY id DESC OFFSET $4 LIMIT $5"),
            &[&scope.tenant(), &scope.user(), &company, &offset, &limit],
        )
        .await?
        .iter()
        .map(Run::from_row)
        .collect::<Result<Vec<_>>>()?;
    count(client, scope.tenant(), &mut runs).await?;
    Ok((runs, total))
}

/// The person's completed runs of these record types in this company:
/// the parents a child import may name. A run an erasure cleaned keeps no
/// source keys, so it is not offered.
pub(crate) async fn completed(
    client: &impl GenericClient,
    scope: &Scope,
    company: i64,
    record_types: &[&str],
) -> Result<Vec<Run>> {
    let types: Vec<String> = record_types.iter().map(|key| (*key).to_owned()).collect();
    client
        .query(
            &format!("SELECT {COLUMNS} FROM base_bulk_data_runs WHERE tenant_id=$1 AND started_by=$2 AND company_id=$3 AND record_type=ANY($4) AND state='completed' AND cardinality(erased_for)=0 ORDER BY id DESC LIMIT 50"),
            &[&scope.tenant(), &scope.user(), &company, &types],
        )
        .await?
        .iter()
        .map(Run::from_row)
        .collect()
}

pub(crate) async fn rows(
    client: &impl GenericClient,
    tenant: i64,
    run: i64,
    pass: Pass,
    outcome: Option<Outcome>,
    undo: Option<&str>,
    page: (i64, i64),
) -> Result<(Vec<RowResult>, i64)> {
    let (offset, limit) = page;
    let outcome = outcome.map(Outcome::as_str);
    let total: i64 = client
        .query_one(
            "SELECT count(*) FROM base_bulk_data_rows WHERE tenant_id=$1 AND run_id=$2 AND pass=$3 AND ($4::text IS NULL OR outcome=$4) AND ($5::text IS NULL OR ($5='left' AND undo_outcome IN ('edited','error')) OR undo_outcome=$5)",
            &[&tenant, &run, &pass.as_str(), &outcome, &undo],
        )
        .await?
        .get(0);
    let rows = client
        .query(
            "SELECT row_number,outcome,record_id,source_key,message,links,record_version,undo_outcome,undo_message FROM base_bulk_data_rows WHERE tenant_id=$1 AND run_id=$2 AND pass=$3 AND ($4::text IS NULL OR outcome=$4) AND ($5::text IS NULL OR ($5='left' AND undo_outcome IN ('edited','error')) OR undo_outcome=$5) ORDER BY row_number OFFSET $6 LIMIT $7",
            &[&tenant, &run, &pass.as_str(), &outcome, &undo, &offset, &limit],
        )
        .await?
        .iter()
        .map(|row| {
            Ok(RowResult {
                row: row.get(0),
                outcome: Outcome::parse(row.get(1)).ok_or(Error::Internal)?,
                record_id: row.get(2),
                source_key: row.get(3),
                message: row.get(4),
                links: serde_json::from_value(row.get(5)).map_err(|_| Error::Internal)?,
                record_version: row.get(6),
                undo_outcome: row
                    .get::<_, Option<&str>>(7)
                    .map(|value| UndoOutcome::parse(value).ok_or(Error::Internal))
                    .transpose()?,
                undo_message: row.get(8),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((rows, total))
}

pub(crate) async fn mappings(
    client: &impl GenericClient,
    scope: &Scope,
    company: i64,
    record_type: &str,
) -> Result<Vec<SavedMapping>> {
    client
        .query(
            "SELECT id,record_type,name,columns,mapping,version FROM base_bulk_data_mappings WHERE tenant_id=$1 AND company_id=$2 AND record_type=$3 ORDER BY lower(name),id LIMIT 200",
            &[&scope.tenant(), &company, &record_type],
        )
        .await?
        .iter()
        .map(saved)
        .collect()
}

pub(crate) fn saved(row: &Row) -> Result<SavedMapping> {
    Ok(SavedMapping {
        id: row.get("id"),
        record_type: row.get("record_type"),
        name: row.get("name"),
        columns: serde_json::from_value(row.get("columns")).map_err(|_| Error::Internal)?,
        mapping: serde_json::from_value(row.get("mapping")).map_err(|_| Error::Internal)?,
        version: row.get("version"),
    })
}
