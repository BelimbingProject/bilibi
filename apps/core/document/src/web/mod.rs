//! Pages and downloads: the document list, a document's record page,
//! a revision's file and the CSV
//! export. Reads use the `Documents` facade; every write calls an operation
//! contract through `PageContracts` (declared in `PageWrites`), except
//! receiving a file, which stores it with `store_file` (a listed page-write
//! gap, as Bulk data's receive). No Bilimbi counterpart; the shapes follow
//! `cx/activities` and Core Person's list and record pages.
mod dialog;
mod list;
mod panel;
mod record;

use crate::{Document, Documents, Filter, Sort};
use askama::Template;
use bilibi_agent_api::page::{Call, PageContracts};
use bilibi_database::{Db, Error, Result};
use bilibi_datetime::{DisplayContext, Format, Precision};
use bilibi_module_registry::Services;
use bilibi_picklists::PickLists;
use bilibi_principal_directory::{PrincipalDirectory, PrincipalKind, PrincipalRef, Query};
use bilibi_records::Records;
use bilibi_tenancy::Scope;
use bilibi_ui::{
    download::{Download, DownloadFuture, DownloadRequest, FileBody},
    page::*,
    Html,
};
use jiff::{civil::Date, Timestamp};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

pub const SERVICE: &str = "core/document.pages";
pub const FILES: &str = "core/document.files";
/// The list's path.
pub const INDEX: &str = "/documents";
/// The most rows one CSV export writes.
const EXPORT_ROWS: i64 = 5_000;

/// A document's record page.
pub fn path(id: i64) -> String {
    format!("{INDEX}/{id}")
}
/// Where a revision's file downloads.
pub fn file_path(id: i64, revision: i32) -> String {
    format!("{INDEX}/{id}/revisions/{revision}/file")
}

pub struct Pages {
    pub documents: Arc<Documents>,
    pub records: Arc<Records>,
    pub picklists: Arc<PickLists>,
    pub contracts: Arc<PageContracts>,
    pub directory: Arc<PrincipalDirectory>,
    pub display: Arc<bilibi_datetime::DisplayPolicy>,
    pub locales: Arc<bilibi_locale::Locales>,
    pub db: Db,
}
impl Pages {
    pub fn new(s: &Services) -> bilibi_module_registry::Result<Self> {
        Ok(Self {
            documents: s.get(crate::SERVICE)?,
            records: s.get(bilibi_records::SERVICE)?,
            picklists: s.get(bilibi_picklists::SERVICE)?,
            contracts: s.get(bilibi_agent_api::page::SERVICE)?,
            directory: s.get("base/principal_directory.directory")?,
            display: s.get("base/datetime.display")?,
            locales: s.get("base/locale.locales")?,
            db: s.get::<Db>("base/database.pool")?.as_ref().clone(),
        })
    }
    fn shared(&self) -> Self {
        Self {
            documents: self.documents.clone(),
            records: self.records.clone(),
            picklists: self.picklists.clone(),
            contracts: self.contracts.clone(),
            directory: self.directory.clone(),
            display: self.display.clone(),
            locales: self.locales.clone(),
            db: self.db.clone(),
        }
    }
    fn call<'a>(
        &self,
        scope: &'a Scope,
        credential: &'a str,
        event: &'a str,
        contract: &'static str,
    ) -> Call<'a> {
        Call {
            scope,
            credential,
            service: SERVICE,
            event,
            contract,
        }
    }
    /// Submits a contract for a page event; a refusal is what the page says.
    async fn submit(
        &self,
        scope: &Scope,
        credential: &str,
        event: &str,
        contract: &'static str,
        input: Value,
        failed: &str,
    ) -> Result<std::result::Result<Value, Refusal>> {
        let call = self.call(scope, credential, event, contract);
        match self.contracts.submit(&call, input).await {
            Ok(value) => Ok(Ok(value)),
            Err(error) => Ok(Err(refusal(error, failed)?)),
        }
    }
    /// The reader's display context: their clock mode, zone and language.
    async fn clock(&self, scope: &Scope) -> Result<Clock> {
        let client = self.db.get().await?;
        let locale = self.locales.resolve(&**client, scope, None).await?.locale;
        Ok(Clock(
            self.display.display(&**client, scope, None, locale).await?,
        ))
    }
    /// Account names for the ids given, in one directory read.
    async fn names(&self, scope: &Scope, users: &[i64]) -> Result<HashMap<i64, String>> {
        let principals: Vec<PrincipalRef> = users
            .iter()
            .filter_map(|id| PrincipalRef::new(PrincipalKind::User, *id).ok())
            .collect();
        if principals.is_empty() {
            return Ok(HashMap::new());
        }
        let client = self.db.get().await?;
        let ranked = self
            .directory
            .rank(&**client, scope, &principals, &Query::default())
            .await
            .map_err(|_| Error::Internal)?;
        Ok(principals
            .iter()
            .map(|p| (p.id(), ranked.label(*p)))
            .collect())
    }
    /// A pick-list's choices in the reader's language.
    async fn choices(
        &self,
        scope: &Scope,
        list: &str,
        parent: Option<&str>,
    ) -> Result<Vec<bilibi_ui::Choice>> {
        let client = self.db.get().await?;
        self.picklists.choices(&**client, scope, list, parent).await
    }
    /// How a stored code shows: its label, marked when retired.
    async fn label(&self, scope: &Scope, list: &str, code: Option<&str>) -> Result<String> {
        let Some(code) = code else {
            return Ok(String::new());
        };
        let client = self.db.get().await?;
        Ok(
            match self.picklists.label(&**client, scope, list, code).await? {
                Some(label) if label.retired => format!("{} (retired)", label.text),
                Some(label) => label.text,
                None => code.to_owned(),
            },
        )
    }
}

/// How the reader sees dates and instants.
pub(crate) struct Clock(DisplayContext);
impl Clock {
    pub fn date(&self, date: Date) -> String {
        self.0.render_date(date)
    }
    pub fn text(&self, at: Timestamp) -> String {
        self.0.render(at, Format::DateTime, Precision::Minute).text
    }
    /// The shared typed instant for a table cell.
    pub fn instant(&self, at: Timestamp) -> bilibi_ui::flex_table::Instant {
        let r = self.0.render(at, Format::DateTime, Precision::Minute);
        bilibi_ui::flex_table::Instant::new(&r.iso, &r.text, &r.company, &r.utc, r.mode.as_str())
    }
}

/// A byte count as people read it.
pub(crate) fn size(bytes: i64) -> String {
    const KIB: f64 = 1024.0;
    let bytes = bytes.max(0) as f64;
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < KIB * KIB {
        format!("{:.0} KB", bytes / KIB)
    } else {
        format!("{:.1} MB", bytes / (KIB * KIB))
    }
}

#[derive(Template)]
#[template(path = "documents_region.html")]
pub(crate) struct Region<'a> {
    pub id: &'a str,
    pub target: bool,
    pub body: Html,
}

pub(crate) fn render(template: impl Template) -> Result<Html> {
    Html::render(&template).map_err(|_| Error::Internal)
}

pub(crate) fn event(value: &Value) -> Result<&str> {
    value
        .get("type")
        .and_then(Value::as_str)
        .ok_or(Error::Invalid)
}

/// What a refused contract call says on the page. `Unauthorized` and a
/// step-up request go back to the host unchanged.
pub(crate) enum Refusal {
    Fields(std::collections::BTreeMap<String, Vec<String>>),
    Message(String),
}
impl Refusal {
    /// Every reason as one sentence, for a flash or notice.
    pub fn text(self) -> String {
        match self {
            Self::Message(message) => message,
            Self::Fields(fields) => fields.into_values().flatten().collect::<Vec<_>>().join(" "),
        }
    }
}
pub(crate) fn refusal(error: bilibi_agent_api::Error, failed: &str) -> Result<Refusal> {
    use bilibi_agent_api::Error as E;
    Ok(match error {
        E::Backend(error @ (Error::Unauthorized | Error::StepUpRequired { .. })) => {
            return Err(error)
        }
        E::Forbidden | E::Backend(Error::Forbidden) => {
            Refusal::Message("You do not have permission to change this document.".into())
        }
        E::Backend(Error::Conflict) | E::PlanRejected(_) => Refusal::Message(
            "The document changed since this page loaded. Reload before trying again.".into(),
        ),
        E::NotFound => Refusal::Message("The document could not be found.".into()),
        E::InvalidInput(fields) => Refusal::Fields(fields),
        _ => Refusal::Message(failed.into()),
    })
}

/// The flash text for a facade refusal while receiving a file.
pub(crate) fn store_refusal(error: crate::WriteError) -> Result<String> {
    use crate::WriteError as W;
    Ok(match error {
        W::Invalid(errors) => errors
            .into_iter()
            .map(|e| bilibi_ui::error_sentence("File", &e.message))
            .collect::<Vec<_>>()
            .join(" "),
        W::Forbidden | W::Access(Error::Forbidden) => {
            "You do not have permission to upload documents.".into()
        }
        W::Access(error @ (Error::Unauthorized | Error::StepUpRequired { .. })) => {
            return Err(error)
        }
        W::Access(Error::Refused(message)) => message,
        W::Access(error) => return Err(error),
        error => error.to_string(),
    })
}

fn record_id(request: &PageRequest<'_, Scope>) -> Result<i64> {
    request
        .path
        .get("id")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|id| *id > 0)
        .ok_or(Error::NotFound)
}

impl Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        match operation {
            "list" | "detail" => Some("Documents"),
            _ => None,
        }
    }
    fn render<'a>(&'a self, request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            let mut state = self.state(&request, scope).await?;
            let body = state
                .update(LiveRequest {
                    scope,
                    credential: "",
                    input: LiveInput::Refresh,
                })
                .await?;
            Ok(PageBody {
                html: body.html,
                live: true,
                title: None,
            })
        })
    }
    fn mount<'a>(&'a self, request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            Ok(Some(self.state(&request, scope).await?))
        })
    }
}
impl Pages {
    async fn state(
        &self,
        request: &PageRequest<'_, Scope>,
        scope: &Scope,
    ) -> Result<Box<dyn LiveState<Scope, Error>>> {
        Ok(match request.operation {
            "list" => Box::new(
                list::Index::new(self.shared(), scope, request.query, request.framed).await?,
            ),
            "detail" => Box::new(
                record::Detail::new(self.shared(), scope, record_id(request)?, request.framed)
                    .await?,
            ),
            "company_panel" | "person_panel" | "record_panel" => {
                Box::new(panel::Panel::new(self.shared(), request)?)
            }
            _ => return Err(Error::NotFound),
        })
    }
}

/// The list's filters as the address carries them, for the list and its
/// export: `search`, `category`, `type`, `status` and `related=<kind>:<id>`.
pub(crate) fn filter_of(query: &HashMap<String, String>) -> Filter {
    let code = |key: &str| {
        query
            .get(key)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty() && *v != "all" && v.len() <= 64)
            .map(str::to_owned)
    };
    Filter {
        search: query
            .get("search")
            .map(|s| s.trim().chars().take(200).collect())
            .unwrap_or_default(),
        category: code("category"),
        document_type: code("type"),
        status: code("status"),
        related: related_of(query),
        ..Filter::default()
    }
}
/// `related=<kind>:<id>` narrows to one record's documents.
pub(crate) fn related_of(query: &HashMap<String, String>) -> Option<crate::Reference> {
    let (kind, id) = query.get("related")?.split_once(':')?;
    (!kind.is_empty() && !id.is_empty() && kind.len() <= 100 && id.len() <= 64)
        .then(|| crate::Reference::new(kind, id))
}

/// A CSV cell: quoted when needed, and a leading `=`, `+`, `-`, `@`, tab or
/// carriage return made inert so a spreadsheet never runs it.
fn csv_cell(value: &str) -> String {
    let value = if value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{value}")
    } else {
        value.to_owned()
    };
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value
    }
}
fn csv_row(cells: &[String]) -> String {
    let mut line = cells
        .iter()
        .map(|c| csv_cell(c))
        .collect::<Vec<_>>()
        .join(",");
    line.push_str("\r\n");
    line
}

/// The downloads: a revision's file (`/documents/{id}/revisions/{revision}/file`)
/// read through `base/artifacts` every time, and the list's metadata as CSV
/// (`/documents/export.csv`), never a file's content.
pub struct Files(pub Arc<Documents>);
impl Files {
    async fn export(&self, scope: &Scope, query: &HashMap<String, String>) -> Result<FileBody> {
        let filter = Filter {
            sort: Sort::Name,
            descending: false,
            page: 1,
            page_size: EXPORT_ROWS,
            ..filter_of(query)
        };
        let page = self.0.list(scope, &filter).await?;
        let mut csv = csv_row(
            &[
                "id",
                "name",
                "publish_date",
                "source",
                "external_url",
                "category",
                "sub_category",
                "type",
                "status",
                "revision",
                "last_revision_at",
                "filing_company_id",
                "created_at",
            ]
            .map(String::from),
        );
        let text = |v: &Option<String>| v.clone().unwrap_or_default();
        for d in &page.entries {
            csv.push_str(&csv_row(&row(d, text)));
        }
        Ok(FileBody::attachment(
            csv.into_bytes(),
            "text/csv",
            "documents.csv",
        ))
    }
}
fn row(d: &Document, text: impl Fn(&Option<String>) -> String) -> Vec<String> {
    vec![
        d.id.to_string(),
        d.name.clone(),
        d.publish_date.to_string(),
        d.source.code().into(),
        text(&d.external_url),
        text(&d.category),
        text(&d.sub_category),
        text(&d.document_type),
        d.status.clone(),
        d.revision.to_string(),
        d.last_revision_at.to_string(),
        d.company_id.to_string(),
        d.created_at.to_string(),
    ]
}
impl Download<Scope, Error> for Files {
    fn serve<'a>(&'a self, request: DownloadRequest<'a, Scope>) -> DownloadFuture<'a, Error> {
        Box::pin(async move {
            if request.operation == "export" {
                return self.export(request.scope, request.query).await;
            }
            let number = |key: &str| {
                request
                    .path
                    .get(key)
                    .and_then(|v| v.parse::<i64>().ok())
                    .filter(|v| *v > 0)
                    .ok_or(Error::NotFound)
            };
            let id = number("id")?;
            let revision = i32::try_from(number("revision")?).map_err(|_| Error::NotFound)?;
            match self.0.read_file(request.scope, id, revision).await {
                Ok(file) => Ok(FileBody::attachment(
                    file.bytes,
                    &file.content_type,
                    &file.file_name,
                )),
                Err(crate::WriteError::Access(error)) => Err(error),
                Err(crate::WriteError::Forbidden | crate::WriteError::Quarantined) => {
                    Err(Error::Forbidden)
                }
                Err(_) => Err(Error::NotFound),
            }
        })
    }
}
