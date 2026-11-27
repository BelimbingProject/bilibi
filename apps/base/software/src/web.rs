//! `/system/software` (`base/software.pages`, operation `index`): the Software
//! page. Everything it shows comes from the module's own contracts through
//! `PageContracts` (`base.software.status` and `.catalogue` as reads;
//! `.install`, `.undo` as reviewed plans committed with the owner's
//! authenticator code; `.cancel` as a plain submit), so the page and an agent
//! see the same facts and get the same refusals. The
//! Software page is Bilibi's own (the design is the Firstmate scout report
//! `bilibi-domain-chooser`).
//!
//! The page keeps only what the person is doing: proposed catalogue entries,
//! the release number typed, the
//! review being confirmed, and a flash. Every render reads the status again,
//! and a mounted page is woken by each job write through `follow.rs`, so the
//! installer's progress rows appear as they are written.
use crate::{follow::Follow, release_files::UpdatePrompt, MANAGE};
use askama::Template;
use bilibi_agent_api::{
    page::{Call, PageContracts},
    Error as ContractError,
};
use bilibi_authz::effective_capabilities;
use bilibi_database::{Db, Error, Result};
use bilibi_datetime::{DisplayContext, DisplayPolicy, Format, Precision};
use bilibi_locale::Locales;
use bilibi_settings::Settings;
use bilibi_system::Instance;
use bilibi_tenancy::Scope;
use bilibi_ui::{
    page::{
        LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
        PageFuture, PageRequest, Subscription,
    },
    params, Alert, Badge, BadgeKind, BuiltinColumn, Button, ButtonVariant, Card, Choice,
    ColumnType, ColumnsOutcome, ConfirmDialog, Fact, Facts, Field, FlashMap, FlexRow, FlexTable,
    Fold, Header, Html, InputKind, Instant, ListColumns, ListSpec, ListState, StatusKind, Tab,
    TableEmpty, Tabs, NO_FLASH,
};
use jiff::Timestamp;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};

mod modules;

pub const PATH: &str = "/system/software";
pub(crate) const PAGES: &str = "base/software.pages";
const DENIED: &str = "You do not have permission to change the software.";
const UNAVAILABLE: &str = "The software service did not answer. Nothing was queued; try again.";
/// How long a read catalogue is shown before it is read again.
const CATALOGUE_KEEP: Duration = Duration::from_secs(300);

/// The module's page adapter; every field is shared by mounted states.
#[derive(Clone)]
pub(crate) struct Pages {
    pub db: Db,
    pub settings: Arc<Settings>,
    pub contracts: Arc<PageContracts>,
    pub display: Arc<DisplayPolicy>,
    pub locales: Arc<Locales>,
    pub follow: Arc<Follow>,
    pub software: Arc<crate::Software>,
}

fn render(fragment: impl Template) -> Result<Html> {
    Html::render(&fragment).map_err(|_| Error::Internal)
}
fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}
fn event_i64(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(|item| {
        item.as_i64()
            .or_else(|| item.as_str().and_then(|text| text.trim().parse().ok()))
            .filter(|number| *number > 0)
    })
}
/// A read contract's answer, out of the door's envelope (`result`, `meta`).
async fn read(contracts: &PageContracts, scope: &Scope, key: &str) -> Result<Value> {
    let mut reply = contracts
        .read(scope, key, &json!({}))
        .await
        .map_err(backend)?;
    Ok(reply["result"].take())
}
/// A contract's refusal as the page's own error, for a read the route already
/// admitted: the owner's words never reach the page.
fn backend(error: ContractError) -> Error {
    match error {
        ContractError::Backend(error) => error,
        ContractError::Forbidden => Error::Forbidden,
        _ => Error::Internal,
    }
}
/// The first field message of a refused input, in the owner's words.
fn first_message(fields: &BTreeMap<String, Vec<String>>) -> String {
    fields
        .values()
        .flatten()
        .next()
        .cloned()
        .unwrap_or_else(|| "The request was not accepted.".into())
}
fn instant(display: &DisplayContext, at: &str) -> Option<Instant> {
    let at: Timestamp = at.parse().ok()?;
    let rendered = display.render(at, Format::DateTime, Precision::Minute);
    Some(Instant::new(
        &rendered.iso,
        &rendered.text,
        &rendered.company,
        &rendered.utc,
        rendered.mode.as_str(),
    ))
}
/// "2 min 5 s" between two recorded instants.
fn elapsed(from: &str, to: &str) -> Option<String> {
    let from: Timestamp = from.parse().ok()?;
    let to: Timestamp = to.parse().ok()?;
    let seconds = to.as_second() - from.as_second();
    Some(duration_text(seconds.max(0)))
}
fn minutes_text(minutes: u64) -> String {
    if minutes == 1 {
        "about 1 minute".into()
    } else {
        format!("about {minutes} minutes")
    }
}
fn duration_text(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds} s")
    } else if seconds % 60 == 0 {
        format!("{} min", seconds / 60)
    } else {
        format!("{} min {} s", seconds / 60, seconds % 60)
    }
}
/// A Domain or add-on id from the catalogue, safe inside a DOM id and a form
/// field name: the same rule the review applies to a request.
fn safe_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
fn short_commit(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Install,
    Undo,
    Go,
}
impl Kind {
    /// The page event the write is declared under (`PageWrites`).
    fn event(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Undo => "undo",
            Self::Go => "go",
        }
    }
    fn contract(self) -> &'static str {
        match self {
            Self::Install => "base.software.install",
            Self::Undo => "base.software.undo",
            Self::Go => "base.software.go",
        }
    }
    fn parse(event: &str) -> Option<Self> {
        match event {
            "install" => Some(Self::Install),
            "undo" => Some(Self::Undo),
            "go" => Some(Self::Go),
            _ => None,
        }
    }
}

/// A dry-run review the person has not yet confirmed: exactly what the owner
/// previewed and the unchanged input to recheck and commit.
struct Review {
    kind: Kind,
    input: Value,
    facts: Value,
    error: Option<String>,
}
enum Dialog {
    Review(Review),
    Cancel { job: i64, running: bool },
    Commands { kind: String, id: String },
}

struct View {
    pages: Pages,
    query: HashMap<String, String>,
    releases: crate::release_files::ReleaseFiles,
    backups: ListColumns,
    backup_list: ListState,
    modules: modules::Modules,
    propose: Vec<String>,
    bundle: String,
    dialog: Option<Dialog>,
    flash: Option<(StatusKind, String)>,
    install_error: Option<String>,
    columns: ListColumns,
    rows_changed: bool,
    catalogue: Option<(std::time::Instant, Value)>,
}

// The job panel and history use one schema.
bilibi_ui::record_fields! { "base/software", mod job_fields {
    ID: bilibi_ui::RecordField::new("id", "Job", bilibi_ui::RecordFieldType::Integer),
    KIND: bilibi_ui::RecordField::new("kind", "What", bilibi_ui::RecordFieldType::String),
    STATE: bilibi_ui::RecordField::new("state", "Outcome", bilibi_ui::RecordFieldType::String),
    SUMMARY: bilibi_ui::RecordField::new("summary", "Change", bilibi_ui::RecordFieldType::String),
    REQUESTED_AT: bilibi_ui::RecordField::new("requested_at", "Requested", bilibi_ui::RecordFieldType::DateTime),
    FINISHED_AT: bilibi_ui::RecordField::new("finished_at", "Finished", bilibi_ui::RecordFieldType::DateTime),
    WORKER: bilibi_ui::RecordField::new("worker", "Worker", bilibi_ui::RecordFieldType::String),
    STEPS: bilibi_ui::RecordField::new("steps", "Steps", bilibi_ui::RecordFieldType::Array).excluded("Nested progress and developer details stay on the job panel"),
    RESULT: bilibi_ui::RecordField::new("result", "Result", bilibi_ui::RecordFieldType::Object).excluded("Long outcome messages stay on the job panel"),
} }

fn history_columns() -> Vec<BuiltinColumn> {
    let mut requested = job_fields::get("requested_at")
        .builtin()
        .expect("job scalar column");
    requested.kind = ColumnType::Datetime;
    let mut finished = job_fields::get("finished_at")
        .builtin()
        .expect("job scalar column");
    finished.kind = ColumnType::Datetime;
    vec![
        job_fields::get("id")
            .builtin()
            .expect("job scalar column")
            .right(),
        job_fields::get("kind")
            .builtin()
            .expect("job scalar column"),
        job_fields::get("state")
            .builtin()
            .expect("job scalar column"),
        job_fields::get("summary")
            .builtin()
            .expect("job scalar column"),
        requested,
        finished,
    ]
}

impl View {
    fn new(pages: Pages, query: &HashMap<String, String>) -> Result<Self> {
        let propose = query
            .get("propose")
            .map(|list| {
                list.split(',')
                    .map(str::trim)
                    .filter(|id| safe_id(id))
                    .take(16)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let backup_spec = ListSpec {
            param_prefix: "backups_".into(),
            omit_defaults: true,
            ..ListSpec::default()
        }
        .shared()
        .map_err(|_| Error::Internal)?;
        Ok(Self {
            releases: crate::release_files::ReleaseFiles::new(pages.clone())?,
            backups: ListColumns::mount(
                "software-backups",
                vec![
                    BuiltinColumn::new("taken_for", "Taken for"),
                    BuiltinColumn::new("when", "When"),
                    BuiltinColumn::new("size", "Size").right(),
                ],
            )
            .map_err(|_| Error::Internal)?,
            backup_list: ListState::parse(query, backup_spec).map_err(|_| Error::Internal)?,
            modules: modules::Modules::new(query)?,
            pages,
            query: query.clone(),
            propose,
            bundle: String::new(),
            dialog: None,
            flash: None,
            install_error: None,
            columns: ListColumns::mount("software-jobs", history_columns())
                .map(|columns| columns.with_record_fields(job_fields::ALL.iter().copied()))
                .map_err(|_| Error::Internal)?,
            rows_changed: true,
            catalogue: None,
        })
    }
    fn flash(&mut self, kind: StatusKind, message: impl Into<String>) {
        self.flash = Some((kind, message.into()));
    }
    fn call<'a>(&self, scope: &'a Scope, credential: &'a str, kind: Kind) -> Call<'a> {
        Call {
            scope,
            credential,
            service: PAGES,
            event: kind.event(),
            contract: kind.contract(),
        }
    }

    /// The published catalogue, read through the contract and kept for a while.
    async fn catalogue(&mut self, scope: &Scope) -> Result<Value> {
        if let Some((at, value)) = &self.catalogue {
            if at.elapsed() < CATALOGUE_KEEP {
                return Ok(value.clone());
            }
        }
        let value = read(&self.pages.contracts, scope, "base.software.catalogue").await?;
        self.catalogue = Some((std::time::Instant::now(), value.clone()));
        Ok(value)
    }

    async fn event(
        &mut self,
        scope: &Scope,
        credential: &str,
        value: &Value,
        status: &Value,
    ) -> Result<()> {
        let values =
            params::string_values_checked(value, params::FIELD_LIMIT).map_err(Error::refused)?;
        let get = |key: &str| value.get(key).and_then(Value::as_str);
        let kind = get("type").ok_or(Error::Invalid)?;
        if self.modules.event(kind, value)? {
            self.modules.write_query(&mut self.query);
            return Ok(());
        }
        match kind {
            "commands" => {
                self.dialog = modules::commands_target(get("module").unwrap_or_default())
                    .map(|(kind, id)| Dialog::Commands { kind, id });
            }
            "request_install" => {
                self.bundle = values.get("bundle").cloned().unwrap_or_default();
                if value.get("asset").is_some() {
                    match (
                        event_i64(value, "asset"),
                        event_i64(value, "repository_id"),
                        event_i64(value, "release_id"),
                    ) {
                        (Some(asset), Some(repository_id), Some(release_id)) => {
                            self.install_error = None;
                            let input = json!({
                                "asset": asset,
                                "repository_id": repository_id,
                                "release_id": release_id,
                                "tag": value.get("tag").and_then(Value::as_str).unwrap_or(""),
                                "target": value.get("target").and_then(Value::as_str).unwrap_or(""),
                                "digest": value.get("digest").and_then(Value::as_str).unwrap_or(""),
                                "switch": "auto",
                            });
                            self.preview(scope, credential, Kind::Install, input)
                                .await?;
                            if let Some(message) = self.install_error.clone() {
                                self.flash(StatusKind::Error, message);
                            }
                        }
                        _ => {
                            let message = "Choose a GitHub file from the list again.".to_owned();
                            self.install_error = Some(message.clone());
                            self.flash(StatusKind::Error, message);
                        }
                    }
                } else if !self.bundle.trim().is_empty() {
                    let bundle = self.bundle.trim().to_owned();
                    self.install_error = None;
                    let input = json!({"bundle": bundle, "switch": "auto"});
                    self.preview(scope, credential, Kind::Install, input)
                        .await?;
                } else {
                    self.install_error =
                        Some("Choose a GitHub release or an uploaded file.".into());
                }
            }
            "request_undo" => {
                self.preview(scope, credential, Kind::Undo, json!({}))
                    .await?
            }
            "request_go" => {
                let job = &status["job"];
                if let Some(id) = job["id"].as_i64() {
                    if text(&job["kind"]) == "install" && job["switch"] == "ask" {
                        self.preview(scope, credential, Kind::Go, json!({"job": id}))
                            .await?
                    }
                }
            }
            "cancel_dialog" => self.dialog = None,
            "install" | "undo" | "go" => {
                let code = values.get("code").cloned().unwrap_or_default();
                self.commit(scope, credential, get("type").unwrap_or_default(), &code)
                    .await?
            }
            "request_cancel" => {
                let job = &status["job"];
                if let Some(id) = job["id"].as_i64() {
                    if job["cancel"] == "available" {
                        self.dialog = Some(Dialog::Cancel {
                            job: id,
                            running: job["state"] == "running",
                        });
                    }
                }
            }
            "cancel_job" => {
                let Some(Dialog::Cancel { job, .. }) = self.dialog.take() else {
                    return Ok(());
                };
                let call = Call {
                    scope,
                    credential,
                    service: PAGES,
                    event: "cancel_job",
                    contract: "base.software.cancel",
                };
                match self
                    .pages
                    .contracts
                    .submit(&call, json!({"job": job}))
                    .await
                {
                    Ok(reply) => {
                        let state = text(&reply["result"]["state"]);
                        let message = if state == "cancelled" {
                            format!("Job {job} was cancelled. Nothing has changed.")
                        } else {
                            format!("Job {job} was asked to stop. It stops before the pause, so nothing changes.")
                        };
                        self.flash(StatusKind::Success, message);
                    }
                    Err(ContractError::Backend(
                        error @ (Error::StepUpRequired { .. } | Error::Unauthorized),
                    )) => return Err(error),
                    Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                        self.flash(StatusKind::Error, DENIED)
                    }
                    Err(ContractError::InvalidInput(fields)) => {
                        self.flash(StatusKind::Error, first_message(&fields))
                    }
                    Err(_) => self.flash(StatusKind::Error, UNAVAILABLE),
                }
            }
            "grid_backups" => {
                if let ColumnsOutcome::Sort(_) = self.backups.handle(&values) {
                    return Err(Error::Invalid);
                }
            }
            "grid" => match self.columns.handle(&values) {
                ColumnsOutcome::Sort(_) => return Err(Error::Invalid),
                ColumnsOutcome::Update | ColumnsOutcome::Noop => self.rows_changed = false,
            },
            "clear-flash" => {
                if self
                    .flash
                    .as_ref()
                    .is_some_and(|(kind, _)| Some(kind.name()) == get("key"))
                {
                    self.flash = None;
                }
            }
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }

    /// The owner's dry-run review of a request, held until the
    /// person confirms it with their authenticator code.
    async fn preview(
        &mut self,
        scope: &Scope,
        credential: &str,
        kind: Kind,
        input: Value,
    ) -> Result<()> {
        let call = self.call(scope, credential, kind);
        let refused = |this: &mut Self, message: String| match kind {
            Kind::Install => this.install_error = Some(message),
            Kind::Undo | Kind::Go => this.flash(StatusKind::Error, message),
        };
        match self.pages.contracts.preview(&call, input.clone()).await {
            Ok(reply) => {
                let result = &reply["results"][0];
                if reply["valid"] == true && result["valid"] == true {
                    let preview = &result["preview"];
                    // The owner's facts, whichever envelope the door wraps them in.
                    let facts =
                        if preview.get("summary").is_none() && preview.get("result").is_some() {
                            preview["result"].clone()
                        } else {
                            preview.clone()
                        };
                    self.dialog = Some(Dialog::Review(Review {
                        kind,
                        input,
                        facts,
                        error: None,
                    }));
                } else {
                    let fields: BTreeMap<String, Vec<String>> =
                        serde_json::from_value(result["error"]["fields"].clone())
                            .unwrap_or_default();
                    let message = if fields.is_empty() {
                        match text(&result["error"]["code"]) {
                            "missing_capability" => DENIED.to_owned(),
                            _ => "The request was not accepted.".to_owned(),
                        }
                    } else {
                        first_message(&fields)
                    };
                    refused(self, message);
                }
            }
            Err(ContractError::Backend(
                error @ (Error::StepUpRequired { .. } | Error::Unauthorized),
            )) => return Err(error),
            Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                self.flash(StatusKind::Error, DENIED)
            }
            Err(ContractError::InvalidInput(fields)) => refused(self, first_message(&fields)),
            Err(_) => self.flash(StatusKind::Error, UNAVAILABLE),
        }
        Ok(())
    }

    /// Commits the reviewed write with the owner's fresh authenticator code
    /// presented on the request, as an agent's `step_up_code` is: the owner's
    /// facade spends it inside the commit (`.always()`), so a wrong code is
    /// an error on this dialog, not a trip through the browser prompt.
    async fn commit(
        &mut self,
        scope: &Scope,
        credential: &str,
        event: &str,
        code: &str,
    ) -> Result<()> {
        let Some(Dialog::Review(review)) = &mut self.dialog else {
            return Ok(());
        };
        if Kind::parse(event) != Some(review.kind) {
            return Err(Error::Invalid);
        }
        let code = code.trim();
        if code.is_empty() {
            review.error = Some("Enter the 6-digit code from your authenticator app.".into());
            return Ok(());
        }
        let presented = scope.clone().presenting_step_up(code);
        let kind = review.kind;
        let call = Call {
            scope: &presented,
            credential,
            service: PAGES,
            event: kind.event(),
            contract: kind.contract(),
        };
        let outcome = self
            .pages
            .contracts
            .commit(&call, review.input.clone())
            .await;
        let label = text(&review.facts["instance"]["label"]).to_owned();
        match outcome {
            Ok(reply) => {
                let job = reply["results"][0]["result"]["result"]["id"].as_i64();
                let what = match kind {
                    Kind::Install => "The install",
                    Kind::Undo => "The undo",
                    Kind::Go => "",
                };
                let message = match (kind, job) {
                    (Kind::Go, Some(id)) => format!("Job {id} has your go-ahead. {label} pauses for about a minute, then switches. Its progress shows below."),
                    (Kind::Go, None) => format!("The go-ahead is given on {label}."),
                    (_, Some(id)) => format!("{what} is queued as job {id} on {label}. Its progress shows below, and the result also reaches your notifications."),
                    (_, None) => format!("{what} is queued on {label}."),
                };
                self.dialog = None;
                self.flash(StatusKind::Success, message);
            }
            Err(ContractError::Backend(Error::StepUpRequired { reason, operation })) => {
                let message = match reason {
                    // Enrolment is the host's prompt: it enrols and comes back.
                    "not_enrolled" => {
                        return Err(Error::StepUpRequired { reason, operation });
                    }
                    "invalid_code" => "That code was not accepted. Enter the current code from your authenticator app.",
                    "locked" => "Too many wrong codes. Wait a while, then try again with a fresh code.",
                    "impersonating" => "The software cannot be changed while viewing as someone else.",
                    "unavailable" => "No authenticator is set up on this server, so the software cannot be changed.",
                    _ => "Enter the current 6-digit code from your authenticator app.",
                };
                review.error = Some(message.into());
            }
            Err(ContractError::Backend(Error::Unauthorized)) => return Err(Error::Unauthorized),
            Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                self.dialog = None;
                self.flash(StatusKind::Error, DENIED);
            }
            Err(ContractError::InvalidInput(fields)) => {
                review.error = Some(first_message(&fields));
            }
            Err(ContractError::PlanRejected(results)) => {
                let fields: BTreeMap<String, Vec<String>> =
                    serde_json::from_value(results[0]["error"]["fields"].clone())
                        .unwrap_or_default();
                review.error = Some(if fields.is_empty() {
                    "The request was not accepted. Close the dialog and review the page.".into()
                } else {
                    first_message(&fields)
                });
            }
            Err(ContractError::Backend(Error::Conflict)) => {
                review.error = Some(if kind == Kind::Go {
                    "The job changed; it may have been stopped or already approved. Close the dialog and review its status.".into()
                } else {
                    "Another job was queued. Close the dialog and review its status.".into()
                });
            }
            Err(_) => review.error = Some(UNAVAILABLE.into()),
        }
        Ok(())
    }

    async fn render(&mut self, scope: &Scope) -> Result<String> {
        if let Some(gate) = self.releases.github_gate(scope).await? {
            return Ok(gate.to_string());
        }
        let pages = self.pages.clone();
        let client = pages.db.get().await?;
        let locale = pages.locales.resolve(&**client, scope, None).await?.locale;
        let display = pages
            .display
            .display(&**client, scope, None, locale)
            .await?;
        let instance = Instance::resolve(&pages.settings, &**client).await?;
        let can_manage = effective_capabilities(&**client, scope)
            .await?
            .contains(MANAGE);
        drop(client);
        let lost = match &self.dialog {
            Some(Dialog::Review(review)) if review.kind == Kind::Undo => {
                match review.facts["job"].as_i64() {
                    Some(job) => pages.software.undo_loss(scope, job).await?["count"].as_u64(),
                    None => None,
                }
            }
            _ => None,
        };
        if let Some(Dialog::Review(review)) = &self.dialog {
            let methods = pages
                .software
                .step_up
                .enrolled_methods(scope)
                .await?
                .into_iter()
                .map(|m| bilibi_ui::ProofMethod::new(m.id, m.label, m.code_length, m.numeric))
                .collect::<Vec<_>>();
            return Ok(self
                .review_dialog(&instance, lost, review, &methods, &display)?
                .to_string());
        }
        let status = read(&pages.contracts, scope, "base.software.status").await?;
        let releases = self.query.get("tab").is_some_and(|tab| tab == "releases");
        let catalogue = if releases {
            Value::Null
        } else {
            self.catalogue(scope).await?
        };
        let content = if releases {
            let active = status["job"]["state"] == "queued" || status["job"]["state"] == "running";
            let mut details = if can_manage {
                self.install_card(&instance, &status, active)?
            } else {
                Html::EMPTY
            };
            details.push(&self.instance_card(&instance, &status)?);
            let state = text(&status["job"]["state"]);
            let mut job = Html::EMPTY;
            if !status["job"].is_null() {
                let card = self.job_card(&display, &instance, can_manage, &status)?;
                if active || matches!(state, "failed" | "interrupted") {
                    job = card;
                } else {
                    details.push(&card);
                }
            }
            details.push(&self.backups_card(&display, &status)?);
            details.push(&self.history_card(&display, &status)?);
            self.releases.body(scope, active, job, details).await?
        } else {
            self.modules
                .body(&status, &catalogue, &self.propose, &display)?
        };
        self.page(&instance, content)
    }

    fn page(&mut self, instance: &Instance, content: Html) -> Result<String> {
        let label = instance.label.clone();
        let subtitle = "See what this server runs and install updates from the Releases tab.";
        let header = render(Header {
            title: Html::text("Software"),
            subtitle,
            ..Header::default()
        })?;
        let banner = instance
            .banner("software-instance", "mb-4")
            .map_err(|_| Error::Internal)?;
        let releases = self.query.get("tab").is_some_and(|tab| tab == "releases");
        let mut domains_query = self.query.clone();
        domains_query.remove("tab");
        let domains_url = params::address(PATH, &domains_query);
        let mut releases_query = domains_query;
        releases_query.insert("tab".into(), "releases".into());
        let releases_url = params::address(PATH, &releases_query);
        let tabs = render(Tabs {
            id: "software-tabs",
            label: "Software",
            class: "mb-4",
            tabs: vec![
                Tab {
                    id: "software-tab-domains",
                    ..Tab::patch(&domains_url, "Domains and extensions", !releases)
                },
                Tab {
                    id: "software-tab-releases",
                    ..Tab::patch(&releases_url, "Releases", releases)
                },
            ],
        })?;
        let dialog = match &self.dialog {
            Some(Dialog::Review(_)) => Html::EMPTY,
            Some(Dialog::Cancel { job, running }) => {
                let consequence = if *running {
                    format!("Stop job {job}?")
                } else {
                    format!("Cancel job {job} before it starts?")
                };
                let detail = if *running {
                    format!("Nothing has changed yet, so stopping is safe: {label} keeps running as it is.")
                } else {
                    "It is removed from the queue. Nothing has changed.".to_owned()
                };
                render(ConfirmDialog {
                    id: "software-cancel-confirm",
                    consequence: &consequence,
                    detail: &detail,
                    confirm: if *running { "Stop" } else { "Cancel the job" },
                    working: "Stopping…",
                    confirm_event: "cancel_job",
                    cancel_event: "cancel_dialog",
                })?
            }
            Some(Dialog::Commands { kind, id }) => match &self.catalogue {
                Some((_, catalogue)) => modules::commands_dialog(catalogue, kind, id)?,
                None => Html::EMPTY,
            },
            None => Html::EMPTY,
        };
        let mut flash = NO_FLASH;
        if let Some((kind, message)) = &self.flash {
            match kind {
                StatusKind::Success => flash.success = message,
                StatusKind::Error => flash.error = message,
                StatusKind::Warning => flash.warning = message,
                StatusKind::Info => flash.info = message,
            }
        }
        PageView {
            flash,
            header,
            banner,
            tabs,
            list: !releases,
            content,
            dialog,
        }
        .render()
        .map_err(|_| Error::Internal)
    }

    /// What runs now, from the program itself.
    fn instance_card(&self, instance: &Instance, status: &Value) -> Result<Html> {
        let installed = &status["installed"];
        let role = instance.notice();
        let platform = &installed["platform"];
        let mut bilibi = format!("Bilibi {}", text(&platform["version"]));
        if let Some(release) = platform["release"].as_str() {
            bilibi.push_str(&format!(" · release {release}"));
        }
        if let Some(commit) = platform["commit"].as_str() {
            bilibi.push_str(&format!(" · build {}", short_commit(commit)));
        }
        let pin = match text(&platform["pin"]) {
            "pinned" => "The program records the exact versions it contains.".to_owned(),
            "stale" => "The saved version list does not match this build. Rebuild with an up-to-date version list.".to_owned(),
            _ => "This build has no exact version list. Record module versions when building the next update.".to_owned(),
        };
        let installed_components = |kind: &str| {
            let components: Vec<_> = installed[kind]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| match item["version"].as_str() {
                    Some(version) => format!(
                        "{} v{}",
                        text(&item["name"]),
                        version.trim_start_matches('v')
                    ),
                    None => text(&item["name"]).to_owned(),
                })
                .collect();
            if components.is_empty() {
                "None".to_owned()
            } else {
                components.join(", ")
            }
        };
        let domains = installed_components("domains");
        let plugins: Vec<String> = status["plugins"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["enabled"] == true)
            .map(|p| text(&p["name"]).to_owned())
            .collect();
        let plugins = if plugins.is_empty() {
            "None".to_owned()
        } else {
            plugins.join(", ")
        };
        let extensions = installed_components("extensions");
        let mut facts = render(Facts {
            id: "software-instance-facts",
            items: vec![
                Fact {
                    title: "Bilibi version",
                    id: "software-fact-platform",
                    value: Html::text(&bilibi),
                },
                Fact {
                    title: "Installed domains",
                    id: "software-fact-domains",
                    value: Html::text(&domains),
                },
                Fact {
                    title: "Installed extensions",
                    id: "software-fact-extensions",
                    value: Html::text(&extensions),
                },
            ],
        })?;
        facts.push(&render(Fold {
            id: "software-version-details",
            title: "Version details",
            body: render(Facts {
                items: vec![
                    Fact {
                        title: "Server",
                        id: "software-fact-role",
                        value: Html::text(&role),
                    },
                    Fact {
                        title: "Enabled add-ons",
                        id: "software-fact-plugins",
                        value: Html::text(&plugins),
                    },
                    Fact {
                        title: "Version record",
                        id: "software-fact-pin",
                        value: Html::text(&pin),
                    },
                ],
                ..Facts::default()
            })?,
            ..Fold::default()
        })?);
        render(Card {
            id: "software-running",
            title: "Installed version",
            body: facts,
            ..Card::default()
        })
    }

    /// The job waiting, running or last finished, with its steps as the
    /// worker reports them.
    fn job_card(
        &self,
        display: &DisplayContext,
        instance: &Instance,
        can_manage: bool,
        status: &Value,
    ) -> Result<Html> {
        let job = &status["job"];
        let id = job["id"].as_i64().unwrap_or_default();
        let kind = text(&job["kind"]);
        let state = text(&job["state"]);
        let steps_json = job["steps"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let first = "fetch";
        let part = steps_json
            .iter()
            .find(|s| s["key"] == first && s["state"] == "done");
        let built = part.and(job["request"]["release"].as_i64());
        let installer_started = steps_json
            .iter()
            .skip_while(|s| s["key"] != first)
            .skip(1)
            .any(|s| s["state"] != "pending");
        let parked =
            kind == "install" && state == "running" && built.is_some() && !installer_started;
        let awaiting_go = parked && job["switch"] == "ask";
        let handed_over = awaiting_go;
        let ready = "copied";
        let label = job["review"]["instance"]["label"]
            .as_str()
            .unwrap_or(&instance.label);
        let nothing_changed =
            job["result"]["migrated"] != true && job["result"]["release"].is_null();
        let unchanged = if nothing_changed {
            ". Nothing changed"
        } else {
            ""
        };
        let doing = match kind {
            "install" => "Installing",
            _ => "Undoing",
        };
        let heading = match state {
            "queued" => format!("Job {id} is waiting for the installer"),
            "running" if handed_over => format!(
                "Release {} is {ready} and waiting to be installed",
                built.unwrap_or_default()
            ),
            // The job's own review names the instance it was asked on.
            "running" => format!("{doing} on {label}…"),
            "done" => format!("Job {id} finished"),
            "failed" => format!("Job {id} failed{unchanged}"),
            "cancelled" => format!("Job {id} was stopped. Nothing changed"),
            "interrupted" => format!("Job {id} was interrupted{unchanged}"),
            other => format!("Job {id} is {other}"),
        };
        let summary = text(&job["review"]["summary"]).to_owned();
        let requested = instant(display, text(&job["requested_at"]));
        let finished = job["finished_at"].as_str();
        let active = state == "queued" || state == "running";
        let mut steps = Vec::with_capacity(steps_json.len());
        for step in steps_json {
            let step_state = text(&step["state"]);
            let detail = &step["detail"];
            let mut message = text(&detail["message"]).to_owned();
            if message.is_empty() && step_state == "skipped" {
                message = "Skipped".into();
            }
            let took = match (step["started_at"].as_str(), step["finished_at"].as_str()) {
                (Some(from), Some(to)) if step_state == "done" || step_state == "failed" => {
                    elapsed(from, to)
                }
                _ => None,
            };
            let developer = detail["details"]
                .as_str()
                .filter(|d| !d.trim().is_empty())
                .map(str::to_owned);
            steps.push(StepView {
                id: format!("software-step-{}", text(&step["key"])),
                label: text(&step["label"]).to_owned(),
                state: step_state.to_owned(),
                icon: match step_state {
                    "done" => "check-circle",
                    "failed" => "x-circle",
                    "running" => "arrow-path",
                    "skipped" => "minus-circle",
                    _ => "ellipsis-horizontal-circle",
                },
                message,
                took,
                developer,
            });
        }
        let result = &job["result"];
        let failed = matches!(state, "failed" | "interrupted");
        let failed_step = steps_json
            .iter()
            .find(|step| step["state"] == "failed")
            .or_else(|| {
                steps_json.iter().find(|step| {
                    !step["started_at"].is_null()
                        && matches!(text(&step["state"]), "running" | "skipped")
                })
            });
        let result_message = if failed {
            result["message"]
                .as_str()
                .filter(|message| !message.trim().is_empty())
                .or_else(|| {
                    failed_step
                        .and_then(|step| step["detail"]["message"].as_str())
                        .filter(|message| !message.trim().is_empty())
                })
                .unwrap_or("No failure reason was recorded.")
        } else {
            text(&result["message"])
        };
        let result_kind = match state {
            "done" => Some(StatusKind::Success),
            "failed" => Some(StatusKind::Error),
            "cancelled" | "interrupted" => Some(StatusKind::Warning),
            _ => None,
        };
        let mut notes = Vec::new();
        if active && !handed_over {
            notes.push("The job continues if you close this page. Its result appears here and in your notifications.".to_owned());
        }
        if awaiting_go {
            notes.push(format!(
                "Release {} is {ready} and waits for your go-ahead: Install now, or Stop to {}.",
                built.unwrap_or_default(),
                "cancel the install"
            ));
        }
        let undo = &status["undo"];
        let undo_offered =
            can_manage && undo["available"] == true && (failed || undo["job"].as_i64() == Some(id));
        if state == "done" && kind == "install" {
            if instance.role == bilibi_system::Role::Staging {
                notes.push("Next: try it here.".to_owned());
            }
            if undo_offered {
                notes.push("Done. Undo is available below if something is wrong.".to_owned());
            }
        }
        match text(&job["cancel"]) {
            "requested" => notes.push("Asked to stop: the worker stops before the pause.".to_owned()),
            "too_late" => notes.push(
                "Past the pause, so it can no longer be stopped. If the check fails it switches back by itself."
                    .to_owned(),
            ),
            _ => {}
        }
        let mut actions = Html::EMPTY;
        if can_manage && job["cancel"] == "available" {
            let label = if state == "running" { "Stop" } else { "Cancel" };
            actions.push(&render(Button {
                id: "software-cancel",
                button_type: "button",
                event: "request_cancel",
                body: Html::text(label),
                ..Button::default()
            })?);
        }
        if can_manage && awaiting_go {
            actions.push(&render(Button {
                id: "software-go",
                button_type: "button",
                event: "request_go",
                variant: ButtonVariant::Primary,
                body: Html::text("Install now (pauses about 1 minute)"),
                ..Button::default()
            })?);
        }
        if undo_offered {
            actions.push(&render(Button {
                id: "software-undo",
                button_type: "button",
                event: "request_undo",
                body: Html::text("Undo"),
                ..Button::default()
            })?);
        }
        let steps = render(Fold {
            id: "software-job-details",
            title: "Install steps",
            open: active || failed,
            body: render(JobSteps { steps })?,
            ..Fold::default()
        })?;
        let mut body = render(JobBody {
            id,
            heading,
            summary,
            requested,
            finished: finished.and_then(|at| instant(display, at)),
            worker: job["worker"].as_str().map(str::to_owned),
            steps,
            result_kind,
            result_message: result_message.to_owned(),
            notes,
            actions,
        })?;
        if failed && can_manage {
            let step = failed_step.map_or("No step was recorded", |step| text(&step["label"]));
            let message = result_message
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            body.push(&render(UpdatePrompt {
                id: "software-diagnosis-prompt",
                label: "Copy AI diagnosis prompt",
                prompt: &format!("Diagnose and help fix Bilibi software job {id} at {}. Step: {step}. Message: {message}", instance.origin),
            })?);
        }
        let title = match state {
            "queued" | "running" => "Current job",
            _ => "Last install",
        };
        render(Card {
            id: "software-job",
            title,
            body,
            ..Card::default()
        })
    }

    /// Recorded backups remain visible after another job becomes the latest.
    /// Uses the same status evidence as the card.
    fn backups_card(&mut self, display: &DisplayContext, status: &Value) -> Result<Html> {
        let backups = status["backups"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let page = self.backup_list.paginate(backups);
        let mut rows = Vec::new();
        for backup in page.entries {
            let id = backup["job"].as_i64().unwrap_or_default();
            let kind = if backup["kind"] == "undo" {
                "undo"
            } else {
                "install"
            };
            let when = match backup["at"].as_str().and_then(|at| instant(display, at)) {
                Some(i) => render(InstantCell {
                    id: format!("software-backup-{id}-at"),
                    i,
                })?,
                None => Html::text("Not recorded"),
            };
            let size = backup["bytes"]
                .as_u64()
                .map_or_else(|| "Not recorded".to_owned(), crate::transport::human);
            let mut taken_for = Html::text(&format!("Before {kind} {id}"));
            // The status contract already omits paths for readers without manage permission.
            if let Some(file) = backup["file"].as_str() {
                taken_for.push(&render(Fold {
                    id: &format!("software-backup-{id}-file"),
                    title: "File location",
                    body: Html::text(file),
                    ..Fold::default()
                })?);
            }
            rows.push(
                FlexRow::new(id.to_string())
                    .slot("taken_for", taken_for)
                    .slot("when", when)
                    .slot("size", Html::text(&size)),
            );
        }
        let mut table = FlexTable::new("software-backups-table", self.backups.flex_columns(), rows);
        table.event = "grid_backups";
        table.zoom = self.backups.zoom;
        table.suggestions = self.backups.flex_suggestions();
        table.add_query = &self.backups.add_query;
        table.caption = "Database backups";
        table.empty = Some(TableEmpty::Content(Html::text(
            "No backup has been recorded for the ten most recent jobs.",
        )));
        let mut body = render(Card {
            id: "software-backups",
            title: "Backups",
            inner_class: "p-0",
            body: render(table)?,
            ..Card::default()
        })?;
        body.push(&render(Fold {
            id: "software-backup-details",
            title: "Backup details",
            body: Html::text(status["backup_rule"].as_str().unwrap_or_default()),
            ..Fold::default()
        })?);
        Ok(body)
    }

    fn install_card(&self, instance: &Instance, status: &Value, busy: bool) -> Result<Html> {
        let uploads: Vec<&Value> = status["uploads"]
            .as_array()
            .map(|list| list.iter().collect())
            .unwrap_or_default();
        let usable = !uploads.is_empty();
        let file = if uploads.is_empty() {
            None
        } else {
            let mut options = vec![Choice::new("Choose an uploaded file", "")];
            options.extend(uploads.iter().map(|upload| {
                let bundle = text(&upload["bundle"]);
                let what = match upload["release"].as_u64() {
                    Some(n) => format!("release {n}"),
                    None => "a release".to_owned(),
                };
                let label = match upload["from"].as_str() {
                    Some(from) => format!("{bundle}: {what} from {from}"),
                    None => format!("{bundle}: {what}"),
                };
                Choice::new(label, bundle)
            }));
            let file = Field::named("bundle")
                .id("software-bundle")
                .label("Uploaded release file")
                .kind(InputKind::Select)
                .options(options)
                .value(self.bundle.clone())
                .hint("Review this file before installing it.")
                .wrapper_class("mb-0 w-full sm:w-72");
            let file = match &self.install_error {
                Some(message) => file.error(message.clone()),
                _ => file,
            };
            Some(if busy { file.disabled() } else { file })
        };
        let review = render(Button {
            id: "software-review-install",
            button_type: "submit",
            variant: ButtonVariant::Primary,
            disabled: busy || !usable,
            working: "Reviewing…",
            body: Html::text("Review the install"),
            ..Button::default()
        })?;
        let mut fields = Html::EMPTY;
        if let Some(file) = file {
            fields.push(&render(file)?);
        }
        let form = if usable {
            render(bilibi_ui::Form {
                id: "software-install-form",
                event: "request_install",
                body: fields,
                actions: review,
                ..bilibi_ui::Form::default()
            })?
        } else {
            Html::EMPTY
        };
        let mut body = Html::EMPTY;
        if !usable {
            body.push(&render(Alert {
                kind: StatusKind::Info,
                id: "software-no-upload",
                body: Html::text(&format!(
                    "Upload a release file to review and install it on {}.",
                    instance.label
                )),
                ..Alert::default()
            })?);
        }
        body.push(&form);
        if instance.role == bilibi_system::Role::Production {
            body.push(&render(Alert {
                kind: StatusKind::Warning,
                id: "software-upload-production",
                body: Html::text("Pick a quiet time: this server pauses for about a minute."),
                ..Alert::default()
            })?);
        }
        if busy {
            body.push(&render(Alert {
                kind: StatusKind::Info,
                id: "software-upload-busy",
                body: Html::text(
                    "Wait for the current job to finish, or cancel it, before installing.",
                ),
                ..Alert::default()
            })?);
        }
        render(Card {
            id: "software-install",
            title: "Install",
            body,
            ..Card::default()
        })
    }

    fn history_card(&mut self, display: &DisplayContext, status: &Value) -> Result<Html> {
        let jobs = status["jobs"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        if self.rows_changed {
            self.columns.load(jobs, |job| {
                job["id"].as_i64().unwrap_or_default().to_string()
            });
        }
        let mut rows = Vec::with_capacity(jobs.len());
        for job in jobs {
            let id = job["id"].as_i64().unwrap_or_default();
            let state = text(&job["state"]);
            let badge = match state {
                "done" => Badge::status(BadgeKind::Success, "Done"),
                "failed" => Badge::status(BadgeKind::Danger, "Failed"),
                "cancelled" => Badge::status(BadgeKind::Neutral, "Stopped"),
                "interrupted" => Badge::status(BadgeKind::Warning, "Interrupted"),
                "running" => Badge::status(BadgeKind::Warning, "Running"),
                "queued" => Badge::status(BadgeKind::Neutral, "Waiting"),
                other => Badge::status(BadgeKind::Neutral, other),
            };
            let kind = match text(&job["kind"]) {
                "change" => "Retired server build",
                "install" => "Install",
                "undo" => "Undo",
                other => other,
            };
            let when = |key: &str, cell: &str| -> Result<Html> {
                match job[key].as_str().and_then(|at| instant(display, at)) {
                    Some(i) => render(InstantCell {
                        id: format!("software-job-{id}-{cell}"),
                        i,
                    }),
                    None => Ok(Html::text("—")),
                }
            };
            let mut row = FlexRow::new(id.to_string())
                .slot("id", render(NumberCell { number: id })?)
                .slot("kind", Html::text(kind))
                .slot("state", render(badge)?)
                .slot("summary", Html::text(text(&job["summary"])))
                .slot("worker", Html::text(job["worker"].as_str().unwrap_or("—")))
                .slot("requested_at", when("requested_at", "requested")?)
                .slot("finished_at", when("finished_at", "finished")?);
            row.dom_id = Some(format!("software-history-{id}"));
            rows.push(row);
        }
        let mut table = FlexTable::new("software-jobs", self.columns.flex_columns(), rows);
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.caption = "Software jobs";
        table.framed = false;
        if jobs.is_empty() {
            table.empty = Some(TableEmpty::Content(Html::text(
                "No install or undo has been requested on this server yet.",
            )));
        }
        let body = render(table)?;
        render(Card {
            id: "software-history",
            title: "History",
            inner_class: "p-0",
            body,
            ..Card::default()
        })
    }

    /// The review the person confirms: the instance first, then what changes,
    /// the warnings, what will happen, and the code field.
    fn review_dialog(
        &self,
        instance: &Instance,
        lost: Option<u64>,
        review: &Review,
        methods: &[bilibi_ui::ProofMethod],
        display: &DisplayContext,
    ) -> Result<Html> {
        let facts = &review.facts;
        let label = instance.label.as_str();
        let banner = instance
            .banner("software-review-instance", "mb-3")
            .map_err(|_| Error::Internal)?;
        let mut lines: Vec<ChangeLine> = Vec::new();
        let mut checks: Vec<String> = Vec::new();
        let (title, lead, submit, variant) = match review.kind {
            Kind::Install => {
                // The operation already names every source, including an
                // uploaded file whose release number may be unknown.
                lines.push(ChangeLine::change(text(&facts["summary"]).to_owned()));
                let running = &facts["running"];
                let domains: Vec<String> = running["domains"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|d| format!("{} {}", text(&d["id"]), text(&d["version"])))
                    .collect();
                lines.push(ChangeLine::keep(format!(
                    "{label} runs Bilibi {} with {} now",
                    text(&running["platform"]),
                    if domains.is_empty() {
                        "no business modules".to_owned()
                    } else {
                        domains.join(", ")
                    }
                )));
                if let Some(changes) = facts["changes"].as_str() {
                    lines.last_mut().unwrap().note = Some(changes.to_owned());
                }
                checks = facts["checks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| text(c).to_owned())
                    .collect();
                let pause = facts["pause_minutes"].as_u64().unwrap_or(1);
                (
                    "Review the install".to_owned(),
                    format!(
                        "Bilibi checks and copies the update. Then {label} pauses for {} while it backs up and installs.",
                        minutes_text(pause)
                    ),
                    format!("Install on {label}"),
                    ButtonVariant::Primary,
                )
            }
            Kind::Go => {
                let release = facts["release"].as_i64().unwrap_or_default();
                lines.push(ChangeLine::change(format!(
                    "Put release {release} into service on {label}"
                )));
                let pause = facts["pause_minutes"].as_u64().unwrap_or(1);
                (
                    "Go ahead with the pause".to_owned(),
                    format!(
                        "Release {release} is ready. {label} pauses for {} once you go ahead.",
                        minutes_text(pause)
                    ),
                    format!("Install now on {label}"),
                    ButtonVariant::Primary,
                )
            }
            Kind::Undo => {
                lines.push(ChangeLine::warn(format!(
                    "Undo job {}: {}",
                    facts["job"].as_i64().unwrap_or_default(),
                    text(&facts["undone"])
                )));
                lines.push(ChangeLine::change(format!(
                    "Back to release {} from release {}",
                    facts["previous_release"].as_i64().unwrap_or_default(),
                    facts["release"].as_i64().unwrap_or_default()
                )));
                let pause = facts["pause_minutes"].as_u64().unwrap_or(1);
                (
                    "Undo the last change".to_owned(),
                    format!("{label} pauses for {} to switch back.", minutes_text(pause)),
                    format!("Undo on {label}"),
                    ButtonVariant::Danger,
                )
            }
        };
        let restores_backup = facts["restores_backup"] == true;
        let title = if review.kind == Kind::Undo && !restores_backup {
            text(&facts["loss"]).to_owned()
        } else {
            title
        };
        let mut warnings: Vec<(StatusKind, Html)> = facts["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|w| (StatusKind::Warning, Html::text(text(w))))
            .collect();
        if review.kind == Kind::Undo && restores_backup {
            let mut loss = Html::EMPTY;
            if let Some(i) = facts["backup_at"]
                .as_str()
                .and_then(|at| instant(display, at))
            {
                loss.push(&Html::text("Bilibi restores the backup from "));
                loss.push(&render(InstantCell {
                    id: "software-review-backup-at".into(),
                    i,
                })?);
                loss.push(&Html::text("; anything entered since then is lost."));
            } else {
                loss.push(&Html::text(text(&facts["loss"])));
            }
            warnings.insert(0, (StatusKind::Warning, loss));
            if let (Some(count), Some(i)) = (
                lost,
                facts["backup_at"]
                    .as_str()
                    .and_then(|at| instant(display, at)),
            ) {
                let mut message = Html::text(&format!(
                    "About {count} {} since ",
                    if count == 1 {
                        "change entered"
                    } else {
                        "changes entered"
                    }
                ));
                message.push(&render(InstantCell {
                    id: "software-review-loss-since".into(),
                    i,
                })?);
                message.push(&Html::text(if count == 1 {
                    " is lost (counted just now)."
                } else {
                    " are lost (counted just now)."
                }));
                warnings.insert(1, (StatusKind::Warning, message));
            }
        }
        if matches!(review.kind, Kind::Install | Kind::Go)
            && instance.role == bilibi_system::Role::Production
        {
            if let Some(first) = warnings.first_mut() {
                first.0 = StatusKind::Error;
            }
        }
        let warnings: Vec<Html> = warnings
            .into_iter()
            .enumerate()
            .map(|(index, (kind, body))| {
                render(Alert {
                    kind,
                    id: &format!("software-review-warning-{index}"),
                    class: "",
                    body,
                })
            })
            .collect::<Result<_>>()?;
        let steps: Vec<String> = facts["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| text(&s["label"]).to_owned())
            .collect();
        let mut what_happens = text(&facts["what_happens"]).to_owned();
        if review.kind == Kind::Undo && restores_backup {
            if let Some(file) = facts["backup"].as_str() {
                what_happens.push_str(&format!(" Undo restores this file on the server: {file}"));
            }
        }
        let code = render(bilibi_ui::ProofInput::new(
            "software-code",
            "code",
            methods.to_vec(),
        ))?;
        let error = match &review.error {
            Some(message) => render(Alert {
                kind: StatusKind::Error,
                id: "software-review-error",
                class: "mb-3",
                body: Html::text(message),
            })?,
            None => Html::EMPTY,
        };
        let cancel = render(Button {
            id: "software-review-cancel",
            button_type: "button",
            event: "cancel_dialog",
            body: Html::text("Cancel"),
            ..Button::default()
        })?;
        let confirm = render(Button {
            id: "software-review-confirm",
            button_type: "submit",
            variant,
            working: "Queuing…",
            body: Html::text(&submit),
            ..Button::default()
        })?;
        let mut step_up =
            bilibi_ui::StepUp::new(!methods.is_empty(), params::address(PATH, &self.query));
        step_up.code = code;
        step_up.actions = confirm;
        step_up.secondary_actions = cancel;
        let body = render(ReviewBody {
            banner,
            lead,
            lines,
            checks,
            warnings,
            what_happens,
            steps,
            event: review.kind.event(),
            step_up,
            error,
        })?;
        render(
            bilibi_ui::StepUpGate::action("Software", &title, body).map_err(|_| Error::Internal)?,
        )
    }
}

struct StepView {
    id: String,
    label: String,
    state: String,
    icon: &'static str,
    message: String,
    took: Option<String>,
    developer: Option<String>,
}
impl StepView {
    fn icon_class(&self) -> &'static str {
        match self.state.as_str() {
            "done" => "size-4 shrink-0 text-success-ink",
            "failed" => "size-4 shrink-0 text-danger-ink",
            "running" => "size-4 shrink-0 animate-spin text-action motion-reduce:animate-none",
            "skipped" => "size-4 shrink-0 text-ink-faint",
            _ => "size-4 shrink-0 text-ink-faint",
        }
    }
    fn label_class(&self) -> &'static str {
        match self.state.as_str() {
            "pending" | "skipped" => "text-ink-muted",
            _ => "text-ink",
        }
    }
}

struct ChangeLine {
    tone: &'static str,
    text: String,
    note: Option<String>,
}
impl ChangeLine {
    fn keep(text: String) -> Self {
        Self {
            tone: "text-ink-muted",
            text,
            note: None,
        }
    }
    fn change(text: String) -> Self {
        Self {
            tone: "text-ink font-medium",
            text,
            note: None,
        }
    }
    fn warn(text: String) -> Self {
        Self {
            tone: "text-warning-ink font-medium",
            text,
            note: None,
        }
    }
}

#[derive(Template)]
#[template(path = "software.html")]
struct PageView<'a> {
    flash: FlashMap<'a>,
    header: Html,
    banner: Html,
    tabs: Html,
    list: bool,
    content: Html,
    dialog: Html,
}

#[derive(Template)]
#[template(path = "software_job.html")]
struct JobBody {
    id: i64,
    heading: String,
    summary: String,
    requested: Option<Instant>,
    finished: Option<Instant>,
    worker: Option<String>,
    steps: Html,
    result_kind: Option<StatusKind>,
    result_message: String,
    notes: Vec<String>,
    actions: Html,
}

#[derive(Template)]
#[template(path = "software_job_steps.html")]
struct JobSteps {
    steps: Vec<StepView>,
}

#[derive(Template)]
#[template(path = "software_review.html")]
struct ReviewBody {
    banner: Html,
    lead: String,
    lines: Vec<ChangeLine>,
    checks: Vec<String>,
    warnings: Vec<Html>,
    what_happens: String,
    steps: Vec<String>,
    event: &'static str,
    step_up: bilibi_ui::StepUp,
    error: Html,
}

#[derive(Template)]
#[template(
    source = r#"{% import "flex_table.html" as flex %}{% call flex::instant(id=id, i=i) %}{% endcall %}"#,
    ext = "html"
)]
struct InstantCell {
    id: String,
    i: Instant,
}

#[derive(Template)]
#[template(
    source = r#"<span class="tabular-nums text-ink">{{ number }}</span>"#,
    ext = "html"
)]
struct NumberCell {
    number: i64,
}

impl LiveState<Scope, Error> for View {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            self.rows_changed = true;
            match request.input {
                LiveInput::Params(query) => {
                    self.releases.cancel_github();
                    self.query = query.clone();
                    self.modules.params(query)?;
                    self.backup_list = ListState::parse(
                        query,
                        self.backup_list
                            .spec()
                            .clone()
                            .shared()
                            .map_err(|_| Error::Internal)?,
                    )
                    .map_err(|_| Error::Internal)?;
                }
                LiveInput::Event(value) => {
                    if self.releases.event(&request).await? {
                        if let Some(flash) = self.releases.flash.take() {
                            self.flash = Some(flash);
                        }
                    } else {
                        let status =
                            read(&self.pages.contracts, request.scope, "base.software.status")
                                .await?;
                        self.event(request.scope, request.credential, value, &status)
                            .await?;
                    }
                }
                LiveInput::Refresh => {}
            }
            let html = self.render(request.scope).await?;
            Ok(LiveBody {
                html,
                query: self.query.clone(),
                ..LiveBody::default()
            })
        })
    }
    fn uploads(&mut self) -> Option<&mut bilibi_ui::Uploads> {
        Some(&mut self.releases.uploads)
    }
    fn subscriptions(&mut self) -> Vec<Subscription> {
        vec![Subscription {
            receiver: self.pages.follow.subscribe(),
        }]
    }
}

impl Page<Scope, Error> for Pages {
    fn redirect(&self, operation: &str) -> Option<&'static str> {
        (operation == "legacy_releases").then_some("/system/software?tab=releases")
    }
    // The old releases address only redirects, so it has no name of its own:
    // a title would list it in the top-bar search beside Software.
    fn title(&self, operation: &str) -> Option<&'static str> {
        (operation == "index").then_some("Software")
    }
    fn render<'a>(&'a self, request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            if request.operation == "legacy_releases" {
                return Ok(PageBody {
                    html: String::new(),
                    live: false,
                    title: None,
                });
            }
            if request.operation != "index" {
                return Err(Error::NotFound);
            }
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            let mut view = View::new(self.clone(), request.query)?;
            let html = view.render(scope).await?;
            Ok(PageBody {
                html,
                live: true,
                title: None,
            })
        })
    }
    fn mount<'a>(&'a self, request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            if request.operation != "index" {
                return Ok(None);
            }
            let state: Box<dyn LiveState<Scope, Error>> =
                Box::new(View::new(self.clone(), request.query)?);
            Ok(Some(state))
        })
    }
}
