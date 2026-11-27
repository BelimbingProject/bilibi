//! The account administration screens: `/users/{id}` and the
//! signed-in account's `/settings/profile`. Each composes the
//! shared Base UI primitives over `UserService`'s account commands; the
//! tenant-wide `/users` index belongs to User Administration (K13).
mod detail;
mod invite;
mod ownership;
mod profile;

pub(crate) use detail::Detail;
pub(crate) use invite::Invite;
pub(crate) use ownership::Ownership;
pub(crate) use profile::{Profile, SERVICE as PROFILE_SERVICE};

use crate::web::Pages;
use askama::Template;
use bilibi_database::{Error, Result};
use bilibi_datetime::{DisplayContext, Format, Precision};
use bilibi_security::Scope;
use bilibi_ui::{FlashMap, Html, Instant, StatusKind};
use jiff::Timestamp;
use serde_json::Value;
use std::{collections::HashMap, time::SystemTime};

pub(crate) struct PersonChoice {
    pub(crate) id: bilibi_party_ids::PersonId,
    pub(crate) label: String,
}

/// The page's one flash message, by kind,
/// rendered by the page's own flash group.
#[derive(Default)]
pub(crate) struct Flash(Option<(StatusKind, String)>);
impl Flash {
    pub fn put(&mut self, kind: StatusKind, message: impl Into<String>) {
        self.0 = Some((kind, message.into()));
    }
    pub fn success(&mut self, message: impl Into<String>) {
        self.put(StatusKind::Success, message);
    }
    pub fn error(&mut self, message: impl Into<String>) {
        self.put(StatusKind::Error, message);
    }
    pub fn clear(&mut self) {
        self.0 = None;
    }
    /// The message for the page a redirect lands on (`LiveBody.flash`).
    pub fn take(&mut self) -> Option<(StatusKind, String)> {
        self.0.take()
    }
    /// The browser dismissed the message of this kind.
    pub fn dismiss(&mut self, key: Option<&str>) {
        if self
            .0
            .as_ref()
            .is_some_and(|(kind, _)| Some(kind.name()) == key)
        {
            self.0 = None;
        }
    }
    pub fn map(&self) -> FlashMap<'_> {
        let mut flash = FlashMap::default();
        if let Some((kind, message)) = &self.0 {
            match kind {
                StatusKind::Success => flash.success = message,
                StatusKind::Error => flash.error = message,
                StatusKind::Warning => flash.warning = message,
                StatusKind::Info => flash.info = message,
            }
        }
        flash
    }
}

/// Companies as a picker names them (`RecordPicker::named`).
pub(crate) const COMPANY: bilibi_ui::NamedKind<'static> = bilibi_ui::NamedKind {
    kind: "core/company",
    prefix: "company",
    label: "Company",
    page: Some("/companies"),
};
/// A type-ahead over the companies a page may offer, instead of a list of
/// every company. The owner still proves the chosen company on save.
pub(crate) fn company_picker(
    id: &str,
    form: &str,
    key: &str,
    label: &str,
    query: &str,
    hits: &[bilibi_tenancy::NamedOrganisation],
    chosen: Option<&bilibi_tenancy::NamedOrganisation>,
    events: (&str, &str),
    errors: Vec<String>,
) -> Result<Html> {
    let hits: Vec<(i64, String)> = hits
        .iter()
        .map(|company| (company.id.get(), company.name.clone()))
        .collect();
    render(&bilibi_ui::RecordPicker::named(
        COMPANY,
        id,
        form,
        key,
        label,
        query,
        &hits,
        chosen.map(|company| (company.id.get(), company.name.as_str())),
        events,
        errors,
    ))
}
/// The company a picker hit raised (`record=company:<id>`).
pub(crate) fn picked_company(event: &Value) -> Result<bilibi_party_ids::OrganisationId> {
    event
        .get("record")
        .and_then(Value::as_str)
        .and_then(|record| COMPANY.picked(record))
        .and_then(|id| bilibi_party_ids::OrganisationId::new(id).ok())
        .ok_or(Error::Invalid)
}

pub(crate) fn render(template: &impl Template) -> Result<Html> {
    Html::render(template).map_err(|_| Error::Internal)
}
pub(crate) fn event(event: &Value) -> Result<&str> {
    event
        .get("type")
        .and_then(Value::as_str)
        .ok_or(Error::Invalid)
}
pub(crate) fn text(event: &Value, key: &str) -> String {
    event
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}
/// The checked values of a `name[]` control, as the transport posts them.
pub(crate) fn list(event: &Value, key: &str) -> Vec<String> {
    event
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}
pub(crate) fn record_id(values: &HashMap<String, String>) -> Result<i64> {
    values
        .get("id")
        .and_then(|v| v.parse().ok())
        .filter(|id: &i64| *id > 0)
        .ok_or(Error::Invalid)
}

impl Pages {
    /// The live, non-system Persons the actor may list, by name; `None` when
    /// the actor may not list people at all.
    pub(crate) async fn person_choices(&self, scope: &Scope) -> Result<Option<Vec<PersonChoice>>> {
        let query = bilibi_person::Search {
            page_size: 300,
            ..bilibi_person::Search::default()
        };
        match self.users.persons.search(scope, &query).await {
            Ok(page) => Ok(Some(
                page.entries
                    .into_iter()
                    .filter(|person| person.system_key.is_none())
                    .map(|person| PersonChoice {
                        id: person.id,
                        label: match person.kind {
                            bilibi_person::PersonKind::Agent => format!("{} (agent)", person.name),
                            bilibi_person::PersonKind::Human => person.name,
                        },
                    })
                    .collect(),
            )),
            Err(Error::Forbidden) => Ok(None),
            Err(error) => Err(error),
        }
    }
    pub(crate) async fn allowed(&self, scope: &Scope, key: &str) -> Result<bool> {
        let client = self.db.get().await?;
        match bilibi_authz::authorize_key(&**client, scope, key).await {
            Ok(()) => Ok(true),
            Err(Error::Forbidden) => Ok(false),
            Err(error) => Err(error),
        }
    }
    pub(crate) async fn display(&self, scope: &Scope) -> Result<DisplayContext> {
        let client = self.db.get().await?;
        let locale = self.locales.resolve(&**client, scope, None).await?.locale;
        self.display.display(&**client, scope, None, locale).await
    }
}

/// The datetime tag through the shared instant markup.
#[derive(Template)]
#[template(
    source = r#"{% import "flex_table.html" as flex %}{% call flex::instant(id=id, i=i) %}{% endcall %}"#,
    ext = "html"
)]
struct DateTime<'a> {
    id: &'a str,
    i: Instant,
}
pub(crate) fn instant(display: &DisplayContext, at: SystemTime) -> Result<Instant> {
    let at = Timestamp::try_from(at).map_err(|_| Error::Internal)?;
    let rendered = display.render(at, Format::DateTime, Precision::Minute);
    Ok(Instant::new(
        &rendered.iso,
        &rendered.text,
        &rendered.company,
        &rendered.utc,
        rendered.mode.as_str(),
    ))
}
pub(crate) fn datetime(display: &DisplayContext, id: &str, at: SystemTime) -> Result<Html> {
    render(&DateTime {
        id,
        i: instant(display, at)?,
    })
}
