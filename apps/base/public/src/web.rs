//! The Public site page, [`crate::PAGE_PATH`] (`base/public.pages`,
//! operation `site`): the tenant's public site (its address, title, logo and
//! whether it is on) and the hostnames it answers at. The site card reads
//! first and opens its form in place; the hostnames are a list with an add
//! dialog, a primary action and a confirmed remove.
//!
//! The route asks for `VIEW`; the page asks for `MANAGE` before it offers a
//! write and again before each one. Every write goes through the operation
//! contracts agents call (`operations.rs`), so a page and an agent are
//! refused alike. A refusal is a flash or a field error, never a cleared page.
use crate::{
    operations::{HOSTNAME_ADD, HOSTNAME_REMOVE, SITE_SET},
    sites::{Site, MANAGE},
    Public, PAGES,
};
use askama::Template;
use bilibi_agent_api::{
    page::{Call, PageContracts},
    Error as ContractError,
};
use bilibi_database::{Db, Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::page::{
    LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
    PageFuture, PageRequest,
};
use bilibi_ui::{
    params, Badge, BadgeKind, BuiltinColumn, Button, CreateButton, ErrorSummary, Fact, Facts,
    Field, FlashMap, FlexRow, FlexTable, Html, IconButton, InputKind, ListColumns, ListSpec,
    ListState, StatusKind, TableEmpty, NO_FLASH,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, LazyLock},
};

const DENIED: &str = "You do not have permission to manage the public site.";
/// The largest logo. Media's own limit still applies.
const LOGO_MAX_BYTES: u64 = 1_048_576;

/// The site's hostnames, primary first; a site has few, so one page.
static SPEC: LazyLock<Arc<ListSpec>> = LazyLock::new(|| {
    ListSpec {
        page_sizes: vec![100],
        default_page_size: 100,
        omit_defaults: true,
        ..ListSpec::default()
    }
    .shared()
    .expect("public site hostnames list spec")
});

bilibi_ui::record_fields! { "base/public", pub(crate) mod fields {
    ADDRESS: bilibi_ui::RecordField::new("address", "Site address", bilibi_ui::RecordFieldType::String).excluded("A fact of the site, not of a hostname"),
    TITLE: bilibi_ui::RecordField::new("title", "Site title", bilibi_ui::RecordFieldType::String).excluded("A fact of the site, not of a hostname"),
    ENABLED: bilibi_ui::RecordField::new("enabled", "Public site is on", bilibi_ui::RecordFieldType::Boolean).excluded("A fact of the site, not of a hostname"),
    HOSTNAME: bilibi_ui::RecordField::new("hostname", "Hostname", bilibi_ui::RecordFieldType::String),
    PRIMARY: bilibi_ui::RecordField::new("primary", "Primary", bilibi_ui::RecordFieldType::Boolean),
} }

fn builtins() -> Vec<BuiltinColumn> {
    let column = |field: bilibi_ui::RecordField| field.builtin().expect("hostname column");
    vec![column(fields::HOSTNAME), column(fields::PRIMARY)]
}

/// The module's page adapter; every field is shared by mounted states.
#[derive(Clone)]
pub struct Pages {
    pub db: Db,
    pub public: Arc<Public>,
    pub contracts: Arc<PageContracts>,
}

fn render(fragment: impl Template) -> Result<Html> {
    Html::render(&fragment).map_err(|_| Error::Internal)
}

#[derive(Clone, Debug, Default)]
struct SiteDraft {
    address: String,
    title: String,
    enabled: bool,
    /// The site has a logo now; the form offers to remove it.
    has_logo: bool,
    remove_logo: bool,
    /// An image stored in Media by `store_logo`, used by the next save.
    staged_logo: Option<i64>,
    staged_name: String,
    version: Option<i64>,
    errors: BTreeMap<String, Vec<String>>,
}
#[derive(Clone, Debug, Default)]
struct HostnameDraft {
    hostname: String,
    primary: bool,
    errors: BTreeMap<String, Vec<String>>,
}

struct SiteForm {
    summary: ErrorSummary,
    address: Field,
    title: Field,
    enabled: Field,
    logo: Html,
    logo_note: Option<String>,
    logo_errors: Vec<String>,
    remove_logo: Option<Field>,
}
struct HostnameForm {
    summary: ErrorSummary,
    hostname: Field,
    primary: Field,
}

#[derive(Template)]
#[template(path = "public_site.html")]
struct View<'a> {
    flash: FlashMap<'a>,
    site_form: Option<SiteForm>,
    site_facts: Html,
    hostnames: Html,
    hostname_actions: Html,
    add: Option<HostnameForm>,
    remove: bool,
    remove_detail: &'a str,
}

struct SitePage {
    pages: Pages,
    list: ListState,
    columns: ListColumns,
    site: Option<SiteDraft>,
    uploads: bilibi_ui::Uploads,
    add: Option<HostnameDraft>,
    /// The hostname a remove waits to confirm.
    remove: Option<String>,
    flash: Option<(StatusKind, String)>,
}

fn summary(
    id: &str,
    errors: &BTreeMap<String, Vec<String>>,
    labels: &[(&str, &str, &str)],
) -> ErrorSummary {
    let mut summary = ErrorSummary::new(id);
    for (key, target, label) in labels {
        for error in errors.get(*key).into_iter().flatten() {
            summary.push(*target, bilibi_ui::error_sentence(label, error));
        }
    }
    summary
}

impl SitePage {
    fn new(pages: &Pages, query: &HashMap<String, String>) -> Result<Self> {
        Ok(Self {
            pages: pages.clone(),
            list: ListState::parse(query, SPEC.clone()).map_err(|_| Error::Internal)?,
            columns: ListColumns::mount("public-site-hostnames", builtins())
                .map_err(|_| Error::Internal)?,
            site: None,
            uploads: bilibi_ui::Uploads::new([bilibi_ui::UploadSpec::new("logo", LOGO_MAX_BYTES)
                .accept(["image/png", "image/jpeg", "image/gif", "image/webp"])
                .max_entries(1)]),
            add: None,
            remove: None,
            flash: None,
        })
    }
    fn flash(&mut self, kind: StatusKind, message: impl Into<String>) {
        self.flash = Some((kind, message.into()));
    }
    async fn can_manage(&self, scope: &Scope) -> Result<bool> {
        let client = self.pages.db.get().await?;
        match bilibi_authz::authorize_key(&**client, scope, MANAGE).await {
            Ok(()) => Ok(true),
            Err(Error::Forbidden) => Ok(false),
            Err(error) => Err(error),
        }
    }
    async fn site(&self, scope: &Scope) -> Result<(Option<Site>, Option<String>)> {
        let client = self.pages.db.get().await?;
        let site = self.pages.public.site_of(&**client, scope).await?;
        let origin = self.pages.public.origin(&**client, scope.tenant()).await?;
        Ok((site, origin))
    }

    fn site_form(&self) -> Option<SiteForm> {
        let draft = self.site.as_ref()?;
        let errors = |key: &str| draft.errors.get(key).cloned().unwrap_or_default();
        Some(SiteForm {
            summary: summary(
                "public-site-errors",
                &draft.errors,
                &[
                    ("address", "public-site-address", fields::ADDRESS.label),
                    ("title", "public-site-title", fields::TITLE.label),
                    ("logo", "public-site-logo", "Logo"),
                    ("version", "public-site-address", "Public site"),
                ],
            ),
            address: Field::in_form("site", "address")
                .id("public-site-address")
                .label(fields::ADDRESS.label)
                .placeholder("e.g. acme")
                .hint("A short name for this site, unique on this installation.")
                .required()
                .attr("maxlength", "63")
                .value(draft.address.clone())
                .errors(errors("address")),
            title: Field::in_form("site", "title")
                .id("public-site-title")
                .label(fields::TITLE.label)
                .placeholder("e.g. Acme")
                .hint("The name visitors and customers see on its pages and in its emails.")
                .required()
                .attr("maxlength", "120")
                .value(draft.title.clone())
                .errors(errors("title")),
            enabled: Field::in_form("site", "enabled")
                .id("public-site-enabled")
                .kind(InputKind::Switch)
                .label(fields::ENABLED.label)
                .value(if draft.enabled { "true" } else { "false" })
                .hint("While off, every page of the site answers \"not found\": forms, surveys, links in emails and the customer portal."),
            logo_note: (!draft.staged_name.is_empty())
                .then(|| format!("{} is ready. Save the site to use it.", draft.staged_name)),
            logo_errors: errors("logo"),
            logo: Html::render(
                &self
                    .uploads
                    .input("logo", "public-site-logo")
                    .label("Logo")
                    .hint("A PNG, JPEG, GIF or WebP image shown on the site's pages. Choose one, then use it; Save applies it. Leave empty to keep the current one."),
            )
            .unwrap_or(Html::EMPTY),
            remove_logo: draft.has_logo.then(|| {
                Field::in_form("site", "remove_logo")
                    .id("public-site-remove-logo")
                    .kind(InputKind::Checkbox)
                    .label("Remove the logo")
                    .value("true")
                    .checked(draft.remove_logo)
            }),
        })
    }

    fn site_facts(
        &self,
        site: Option<&Site>,
        origin: Option<&str>,
        can_manage: bool,
    ) -> Result<Html> {
        let Some(site) = site else {
            if !can_manage {
                return Ok(Html::text(
                    "No public site yet. Someone who manages the public site can set one.",
                ));
            }
            let mut html = Html::text(
                "No public site yet. Set its address and title, then add a hostname and turn it on.",
            );
            html.push(&render(Button {
                id: "public-site-edit",
                event: "edit_site",
                button_type: "button",
                class: "mt-3",
                body: Html::text("Set up"),
                ..Button::default()
            })?);
            return Ok(html);
        };
        let address = match (origin, site.enabled, site.primary()) {
            (Some(origin), _, _) => origin.to_owned(),
            (None, false, Some(_)) => "Turn the site on to use its primary hostname.".to_owned(),
            (None, _, Some(_)) => {
                "This installation serves no public site yet; ask its operator.".to_owned()
            }
            (None, _, None) => "Add a hostname below.".to_owned(),
        };
        let mut html = render(Facts {
            id: "public-site-facts",
            items: vec![
                Fact {
                    title: "Opens at",
                    id: "public-site-origin",
                    value: Html::text(&address),
                },
                Fact {
                    title: fields::TITLE.label,
                    id: "public-site-title-value",
                    value: Html::text(&site.title),
                },
                Fact {
                    title: fields::ADDRESS.label,
                    id: "public-site-address-value",
                    value: Html::text(&site.address),
                },
                Fact {
                    title: "Site",
                    id: "public-site-enabled-value",
                    value: render(Badge {
                        kind: if site.enabled {
                            BadgeKind::Success
                        } else {
                            BadgeKind::Neutral
                        },
                        dot: true,
                        class: "",
                        body: Html::text(if site.enabled { "On" } else { "Off" }),
                    })?,
                },
            ],
        })?;
        if can_manage {
            html.push(&render(Button {
                id: "public-site-edit",
                event: "edit_site",
                button_type: "button",
                class: "mt-3",
                body: Html::text("Change"),
                ..Button::default()
            })?);
        }
        Ok(html)
    }

    fn hostnames(&mut self, site: Option<&Site>, can_manage: bool) -> Result<Html> {
        let rows = site.map(|site| site.hostnames.clone()).unwrap_or_default();
        self.columns.load(&rows, |row| row.name.clone());
        let mut flex_rows = Vec::with_capacity(rows.len());
        for (index, row) in rows.iter().enumerate() {
            let mut flex = FlexRow::new(row.name.clone())
                .slot("hostname", Html::text(&row.name))
                .slot(
                    "primary",
                    if row.primary {
                        render(Badge {
                            kind: BadgeKind::Success,
                            dot: false,
                            class: "",
                            body: Html::text("Primary"),
                        })?
                    } else {
                        Html::EMPTY
                    },
                );
            flex.dom_id = Some(format!("public-site-hostname-{index}"));
            if can_manage {
                if !row.primary {
                    let id = format!("public-site-hostname-primary-{index}");
                    flex.actions = render(IconButton {
                        id: &id,
                        event: "make_primary",
                        name: "hostname",
                        value: &row.name,
                        ..IconButton::new("star", "Make primary: emailed links use it")
                    })?;
                }
                if !row.primary || rows.len() == 1 {
                    let id = format!("public-site-hostname-remove-{index}");
                    flex.actions.push(&render(IconButton {
                        id: &id,
                        event: "remove",
                        name: "hostname",
                        value: &row.name,
                        ..IconButton::new("trash", "Remove hostname")
                    })?);
                }
            }
            flex_rows.push(flex);
        }
        let mut table = FlexTable::new(
            "public-site-hostnames",
            self.columns.flex_columns(),
            flex_rows,
        );
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.framed = false;
        table.actions = can_manage;
        table.caption = "Hostnames";
        if rows.is_empty() {
            table.empty = Some(TableEmpty::Content(Html::text(if site.is_none() {
                "Set the site's address and title first."
            } else if can_manage {
                "No hostnames yet. Point a name such as shop.example.com at this installation's public address, then add it."
            } else {
                "No hostnames yet."
            })));
        }
        render(table)
    }

    fn add_form(&self) -> Option<HostnameForm> {
        let draft = self.add.as_ref()?;
        Some(HostnameForm {
            summary: summary(
                "public-site-hostname-errors",
                &draft.errors,
                &[("hostname", "public-site-hostname", fields::HOSTNAME.label)],
            ),
            hostname: Field::in_form("hostname", "hostname")
                .id("public-site-hostname")
                .label(fields::HOSTNAME.label)
                .placeholder("e.g. shop.example.com")
                .hint("Without https:// or a port. Its DNS must already point at this installation's public address.")
                .required()
                .attr("maxlength", "253")
                .value(draft.hostname.clone())
                .errors(draft.errors.get("hostname").cloned().unwrap_or_default()),
            primary: Field::in_form("hostname", "primary")
                .id("public-site-hostname-primary")
                .kind(InputKind::Checkbox)
                .label("Make it primary")
                .value("true")
                .checked(draft.primary)
                .hint("Emailed links (invitations, password resets, calendar feeds) use the primary hostname. The first one is always primary."),
        })
    }

    async fn body(&mut self, scope: &Scope) -> Result<String> {
        let can_manage = self.can_manage(scope).await?;
        let (site, origin) = self.site(scope).await?;
        let hostnames = self.hostnames(site.as_ref(), can_manage)?;
        let hostname_actions = if can_manage && site.is_some() {
            render(CreateButton {
                noun: "Hostname",
                id: "public-site-hostname-open",
                event: "show_add",
                ..CreateButton::default()
            })?
        } else {
            Html::EMPTY
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
        let remove_detail = if site.as_ref().is_some_and(|site| site.hostnames.len() == 1) {
            "It is the site's last hostname: the site stops answering anywhere, and links already sent stop working."
        } else {
            "The site stops answering there, and links already sent to it stop working."
        };
        Ok(render(View {
            flash,
            site_form: self.site_form(),
            site_facts: self.site_facts(site.as_ref(), origin.as_deref(), can_manage)?,
            hostnames,
            hostname_actions,
            add: self.add_form(),
            remove: self.remove.is_some(),
            remove_detail,
        })?
        .to_string())
    }

    /// One submitted write through its contract. `Ok(Err(fields))` is a
    /// refusal to show on the form; a denial becomes the page's flash.
    async fn submit(
        &mut self,
        scope: &Scope,
        credential: &str,
        event: &'static str,
        contract: &'static str,
        input: Value,
    ) -> Result<Option<std::result::Result<(), BTreeMap<String, Vec<String>>>>> {
        let call = Call {
            scope,
            credential,
            service: PAGES,
            event,
            contract,
        };
        match self.pages.contracts.submit(&call, input).await {
            Ok(_) => Ok(Some(Ok(()))),
            Err(ContractError::InvalidInput(fields)) => Ok(Some(Err(fields))),
            Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                self.flash(StatusKind::Error, DENIED);
                Ok(None)
            }
            Err(error) => Err(contract_error(error)),
        }
    }

    async fn store_logo(&mut self, scope: &Scope, draft: &mut SiteDraft) -> Result<()> {
        if self.uploads.in_progress("logo") {
            draft
                .errors
                .insert("logo".into(), vec!["is still uploading".into()]);
            return Ok(());
        }
        let Some(file) = self.uploads.consume("logo").into_iter().next() else {
            draft
                .errors
                .insert("logo".into(), vec!["choose an image first".into()]);
            return Ok(());
        };
        let name = file.name.clone();
        let stored = {
            let mut client = self.pages.db.get().await?;
            self.pages
                .public
                .media
                .put_public_image(
                    &mut client,
                    scope,
                    "public-site",
                    bilibi_media::Upload {
                        filename: file.name,
                        bytes: file.bytes,
                    },
                    LOGO_MAX_BYTES,
                )
                .await
        };
        match stored {
            Ok(asset) => {
                draft.staged_logo = Some(asset.id);
                draft.staged_name = name;
                draft.remove_logo = false;
            }
            Err(bilibi_media::Error::Database(error)) => return Err(error),
            Err(
                error @ (bilibi_media::Error::InvalidSize
                | bilibi_media::Error::DisallowedType
                | bilibi_media::Error::InvalidUpload),
            ) => {
                draft.errors.insert("logo".into(), vec![error.to_string()]);
            }
            Err(_) => {
                draft.errors.insert(
                    "logo".into(),
                    vec![
                        "could not be stored; ask an administrator to check the media settings"
                            .into(),
                    ],
                );
            }
        }
        Ok(())
    }

    async fn event(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        let get = |key: &str| value.get(key).and_then(Value::as_str);
        match get("type").ok_or(Error::Invalid)? {
            "clear-flash" => self.flash = None,
            "grid" => {
                let params: HashMap<String, String> = value
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(key, value)| match value {
                        Value::String(text) => Some((key.clone(), text.clone())),
                        Value::Number(number) => Some((key.clone(), number.to_string())),
                        _ => None,
                    })
                    .collect();
                self.columns.handle(&params);
            }
            "edit_site" => {
                if !self.can_manage(scope).await? {
                    self.flash(StatusKind::Error, DENIED);
                    return Ok(());
                }
                let (site, _) = self.site(scope).await?;
                self.site = Some(SiteDraft {
                    address: site.as_ref().map(|s| s.address.clone()).unwrap_or_default(),
                    title: site.as_ref().map(|s| s.title.clone()).unwrap_or_default(),
                    enabled: site.as_ref().is_some_and(|s| s.enabled),
                    has_logo: site.as_ref().is_some_and(|s| s.logo_asset_id.is_some()),
                    version: site.map(|s| s.version),
                    ..SiteDraft::default()
                });
            }
            "cancel_site" => {
                self.site = None;
                self.uploads.clear("logo");
            }
            // The form keeps what was typed while the logo is chosen and stored.
            kind @ ("refresh_site" | "store_logo") => {
                let Some(mut draft) = self.site.clone() else {
                    return Ok(());
                };
                let posted = params::form_values_checked(value, "site", params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                if !posted.is_empty() {
                    let field = |key: &str| posted.get(key).cloned().unwrap_or_default();
                    draft.address = field("address");
                    draft.title = field("title");
                    draft.enabled = field("enabled") == "true";
                    draft.remove_logo = field("remove_logo") == "true";
                }
                draft.errors.remove("logo");
                if kind == "store_logo" {
                    if !self.can_manage(scope).await? {
                        self.flash(StatusKind::Error, DENIED);
                        return Ok(());
                    }
                    self.store_logo(scope, &mut draft).await?;
                }
                self.site = Some(draft);
            }
            "save_site" => {
                let posted = params::form_values_checked(value, "site", params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                let field = |key: &str| posted.get(key).cloned().unwrap_or_default();
                let before = self.site.clone().unwrap_or_default();
                let mut draft = SiteDraft {
                    address: field("address"),
                    title: field("title"),
                    enabled: field("enabled") == "true",
                    remove_logo: field("remove_logo") == "true",
                    errors: BTreeMap::new(),
                    ..before
                };
                let mut input = json!({"address": draft.address, "title": draft.title, "enabled": draft.enabled});
                if let Some(version) = draft.version {
                    input["version"] = json!(version);
                }
                if draft.remove_logo {
                    input["remove_logo"] = json!(true);
                } else if let Some(asset) = draft.staged_logo {
                    input["logo_asset_id"] = json!(asset);
                }
                match self
                    .submit(scope, credential, "save_site", SITE_SET, input)
                    .await?
                {
                    Some(Ok(())) => {
                        self.site = None;
                        self.flash(StatusKind::Success, "Public site saved.");
                    }
                    Some(Err(errors)) => {
                        draft.errors = errors;
                        self.site = Some(draft);
                    }
                    None => {}
                }
            }
            "show_add" => {
                if self.can_manage(scope).await? {
                    self.add = Some(HostnameDraft::default());
                } else {
                    self.flash(StatusKind::Error, DENIED);
                }
            }
            "cancel_add" => self.add = None,
            kind @ ("refresh_add" | "add") => {
                let Some(mut draft) = self.add.clone() else {
                    return Ok(());
                };
                let posted = params::form_values_checked(value, "hostname", params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                draft.hostname = posted.get("hostname").cloned().unwrap_or_default();
                draft.primary = posted.get("primary").is_some_and(|v| v == "true");
                if kind == "refresh_add" {
                    self.add = Some(draft);
                    return Ok(());
                }
                let input = json!({"hostname": draft.hostname, "primary": draft.primary});
                match self
                    .submit(scope, credential, "add", HOSTNAME_ADD, input)
                    .await?
                {
                    Some(Ok(())) => {
                        self.add = None;
                        self.flash(StatusKind::Success, "Hostname added.");
                    }
                    Some(Err(errors)) => {
                        draft.errors = errors;
                        self.add = Some(draft);
                    }
                    None => {}
                }
            }
            "make_primary" => {
                let hostname = get("hostname").ok_or(Error::Invalid)?.to_owned();
                let input = json!({"hostname": hostname, "primary": true});
                match self
                    .submit(scope, credential, "make_primary", HOSTNAME_ADD, input)
                    .await?
                {
                    Some(Ok(())) => self.flash(
                        StatusKind::Success,
                        format!("{hostname} is primary. Links emailed from now on use it."),
                    ),
                    Some(Err(_)) => self.flash(
                        StatusKind::Error,
                        "Nothing changed. The hostname may have changed already; reload to see it.",
                    ),
                    None => {}
                }
            }
            "remove" => {
                if !self.can_manage(scope).await? {
                    self.flash(StatusKind::Error, DENIED);
                    return Ok(());
                }
                self.remove = Some(get("hostname").ok_or(Error::Invalid)?.to_owned());
            }
            "cancel_remove" => self.remove = None,
            "confirm_remove" => {
                let Some(hostname) = self.remove.take() else {
                    return Ok(());
                };
                match self
                    .submit(
                        scope,
                        credential,
                        "confirm_remove",
                        HOSTNAME_REMOVE,
                        json!({"hostname": hostname}),
                    )
                    .await
                {
                    Ok(Some(Ok(()))) => self.flash(StatusKind::Success, "Hostname removed."),
                    Ok(Some(Err(errors))) => self.flash(
                        StatusKind::Error,
                        errors
                            .get("hostname")
                            .and_then(|errors| errors.first())
                            .map_or_else(
                                || "Nothing changed.".to_owned(),
                                |error| bilibi_ui::error_sentence("Hostname", error),
                            ),
                    ),
                    Ok(None) => {}
                    Err(Error::NotFound) => self.flash(
                        StatusKind::Error,
                        "Nothing changed. The hostname may be gone already; reload to see it.",
                    ),
                    Err(error) => return Err(error),
                }
            }
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }
}

impl Page<Scope, Error> for Pages {
    fn title(&self, operation: &str) -> Option<&'static str> {
        (operation == "site").then_some("Public site")
    }
    fn render<'a>(&'a self, request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            if request.operation != "site" {
                return Err(Error::NotFound);
            }
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            let mut page = SitePage::new(self, request.query)?;
            Ok(PageBody {
                html: page.body(scope).await?,
                live: true,
                title: None,
            })
        })
    }
    fn mount<'a>(&'a self, request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            if request.operation != "site" {
                return Err(Error::NotFound);
            }
            request.scope.ok_or(Error::Unauthorized)?;
            let state: Box<dyn LiveState<Scope, Error>> =
                Box::new(SitePage::new(self, request.query)?);
            Ok(Some(state))
        })
    }
}

impl LiveState<Scope, Error> for SitePage {
    fn uploads(&mut self) -> Option<&mut bilibi_ui::Uploads> {
        Some(&mut self.uploads)
    }
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(values) => {
                    self.list =
                        ListState::parse(values, SPEC.clone()).map_err(|_| Error::Internal)?;
                }
                LiveInput::Event(value) => {
                    self.event(request.scope, request.credential, value).await?
                }
            }
            let html = self.body(request.scope).await?;
            Ok(LiveBody {
                html,
                query: self.list.to_params(),
                ..LiveBody::default()
            })
        })
    }
}

fn contract_error(error: ContractError) -> Error {
    match error {
        ContractError::Backend(error) => error,
        ContractError::Forbidden => Error::Forbidden,
        ContractError::NotFound => Error::NotFound,
        ContractError::InvalidInput(_) => Error::Invalid,
        _ => Error::Internal,
    }
}
