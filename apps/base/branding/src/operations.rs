//! The brand's operation contracts, its row on the Customisations page and
//! its entry in "what can I change here?". The platform operator's agent, the
//! Customisations page's restore action and System Settings all end in the
//! same three installation settings.
use crate::{valid_name, Branding, Stored, ACCENT_KEY, LOGO_KEY, MANAGE, NAME_KEY, NAME_MAX_CHARS};
use bilibi_agent_api::{
    json::Output, Channels, Definitions, Guide, Handler, Operation, OperationClient,
    OperationFuture, ReadHandler, WriteHandler, DEFINITIONS, SIGNED_IN,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance, RegistryError};
use bilibi_settings::Target;
use bilibi_tenancy::Scope;
use bilibi_ui::{Accent, PRODUCT};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use tokio_postgres::GenericClient;

static BOUND: OnceLock<Arc<Branding>> = OnceLock::new();
pub(crate) fn bind(branding: Arc<Branding>) -> bilibi_module_registry::Result<()> {
    BOUND
        .set(branding)
        .map_err(|_| RegistryError::Service("duplicate branding binding".into()))
}
fn bound() -> bilibi_agent_api::Result<&'static Arc<Branding>> {
    BOUND.get().ok_or(Error::Internal.into())
}

/// One part of the brand, by the name the operations call it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Item {
    Name,
    Logo,
    Accent,
}
impl Item {
    const ALL: [Self; 3] = [Self::Name, Self::Logo, Self::Accent];
    fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Logo => "logo",
            Self::Accent => "accent",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|item| item.as_str() == value)
    }
    fn key(self) -> &'static str {
        match self {
            Self::Name => NAME_KEY,
            Self::Logo => LOGO_KEY,
            Self::Accent => ACCENT_KEY,
        }
    }
    fn title(self) -> &'static str {
        match self {
            Self::Name => "Display name",
            Self::Logo => "Logo",
            Self::Accent => "Accent colour",
        }
    }
    /// What a restore returns to, as people read it.
    fn default_text(self) -> &'static str {
        match self {
            Self::Name => PRODUCT,
            Self::Logo => "the Bilibi mark",
            Self::Accent => "Lime",
        }
    }
    fn customised(self, stored: &Stored) -> bool {
        match self {
            Self::Name => !stored.name.is_empty(),
            Self::Logo => stored.logo.is_some(),
            Self::Accent => stored.accent != Accent::default(),
        }
    }
    /// The value in effect, as people read it.
    fn shown(self, stored: &Stored) -> String {
        match self {
            Self::Name if stored.name.is_empty() => PRODUCT.into(),
            Self::Name => stored.name.clone(),
            Self::Logo if stored.logo.is_some() => "Uploaded image".into(),
            Self::Logo => "The Bilibi mark".into(),
            Self::Accent => stored.accent.label().into(),
        }
    }
}

fn accents() -> Vec<&'static str> {
    Accent::ALL.into_iter().map(Accent::as_str).collect()
}

/// Whether the caller may change the brand, without logging a decision: the
/// read is open to every signed-in person, and most of them may not.
async fn may_manage(
    client: &tokio_postgres::Client,
    scope: &Scope,
) -> bilibi_database::Result<bool> {
    if !bilibi_authz::effective_capabilities(client, scope)
        .await?
        .contains(MANAGE)
    {
        return Ok(false);
    }
    match bilibi_tenancy::prove_operator(client, scope).await {
        Ok(_) => Ok(true),
        Err(Error::Forbidden) => Ok(false),
        Err(error) => Err(error),
    }
}

fn brand_json(stored: &Stored, can_manage: bool) -> Value {
    let items: Vec<Value> = Item::ALL
        .into_iter()
        .map(|item| {
            json!({
                "item": item.as_str(),
                "title": item.title(),
                "value": match item {
                    Item::Name => json!(if stored.name.is_empty() { PRODUCT } else { &stored.name }),
                    Item::Logo => json!(stored.logo.is_some()),
                    Item::Accent => json!(stored.accent.as_str()),
                },
                "shown": item.shown(stored),
                "default": item.default_text(),
                "customised": item.customised(stored),
            })
        })
        .collect();
    json!({
        "items": items,
        "accents": accents(),
        "can_manage": can_manage,
        "powered_by": "The account menu always says \"Powered by Bilibi\".",
    })
}

struct Get;
impl ReadHandler for Get {
    fn owner(&self) -> &'static str {
        "base/branding"
    }
    fn call<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        _input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let sql = client.sql_client().await?;
            let stored = bound()?.stored(sql).await?;
            Ok(Output::from(brand_json(
                &stored,
                may_manage(sql, scope).await?,
            )))
        })
    }
}

/// What a Set asks for, checked.
struct Change {
    name: Option<String>,
    logo: Option<i64>,
    accent: Option<Accent>,
}

fn change(input: &Value) -> bilibi_agent_api::Result<Change> {
    let name = match input.get("name").and_then(Value::as_str) {
        Some(name) => {
            let name = name.trim();
            if name.is_empty() {
                return Err(bilibi_agent_api::invalid(
                    "name",
                    "must not be blank; use base.branding.restore to return to Bilibi",
                ));
            }
            if !valid_name(name) {
                return Err(bilibi_agent_api::invalid(
                    "name",
                    "must be at most 40 characters without @, quotes or angle brackets",
                ));
            }
            Some(name.to_owned())
        }
        None => None,
    };
    let accent = match input.get("accent").and_then(Value::as_str) {
        Some(accent) => Some(Accent::parse(accent).ok_or_else(|| {
            bilibi_agent_api::invalid(
                "accent",
                "must be one of lime, teal, cyan, indigo, violet, fuchsia",
            )
        })?),
        None => None,
    };
    let logo = input.get("logo_asset_id").and_then(Value::as_i64);
    if name.is_none() && accent.is_none() && logo.is_none() {
        return Err(bilibi_agent_api::invalid(
            "name",
            "give at least one of name, accent or logo_asset_id",
        ));
    }
    Ok(Change { name, logo, accent })
}

/// A new logo must be the operator tenant's own raster image that Media will
/// serve (an agent may name any asset id).
async fn check_logo(
    client: &(impl GenericClient + Sync),
    scope: &Scope,
    id: i64,
) -> bilibi_agent_api::Result<()> {
    let refused = || {
        bilibi_agent_api::invalid(
            "logo_asset_id",
            "must be a PNG, JPEG or WebP image of this workspace that passed its file check",
        )
    };
    let media = &bound()?.media;
    let asset = match media.get_asset(client, scope, id).await {
        Ok(asset) => asset,
        Err(bilibi_media::Error::Database(error)) => return Err(error.into()),
        Err(_) => return Err(refused()),
    };
    let image = asset
        .mime_type
        .as_deref()
        .is_some_and(|mime| ["image/png", "image/jpeg", "image/webp"].contains(&mime));
    let servable = match media.tenant_stream_url(client, scope.tenant(), id).await {
        Ok(_) => true,
        Err(bilibi_media::Error::Database(error)) => return Err(error.into()),
        Err(_) => false,
    };
    if image && servable {
        Ok(())
    } else {
        Err(refused())
    }
}

/// The review of a write: a summary line, then each changed part as
/// "before → after" under `plan`, the shape the Customisations page's confirm
/// dialog and the agent review card both read as plain lines.
fn review(before: &Stored, after: &Stored, summary: &str) -> Value {
    let mut plan = serde_json::Map::new();
    for item in Item::ALL {
        let (was, now) = (item.shown(before), item.shown(after));
        let replaced = item == Item::Logo && before.logo != after.logo;
        if was != now {
            plan.insert(item.as_str().into(), json!(format!("{was} → {now}")));
        } else if replaced {
            plan.insert(item.as_str().into(), json!("A new image replaces the logo"));
        }
    }
    let unchanged = plan.is_empty();
    if !unchanged {
        plan.insert(
            "shown_on".into(),
            json!("staff sign-in, the top bar, tab titles and the email sender name"),
        );
    }
    json!({
        "summary": if unchanged { "No change to the brand" } else { summary },
        "plan": plan,
    })
}

enum Write {
    Set,
    Restore,
}

impl Write {
    /// The brand this write leaves, from what is stored now.
    fn next(
        &self,
        input: &Value,
        stored: &Stored,
    ) -> bilibi_agent_api::Result<(Stored, Vec<Item>)> {
        let mut next = Stored {
            name: stored.name.clone(),
            logo: stored.logo,
            accent: stored.accent,
        };
        let mut items = Vec::new();
        match self {
            Self::Set => {
                let change = change(input)?;
                if let Some(name) = change.name {
                    next.name = name;
                    items.push(Item::Name);
                }
                if let Some(logo) = change.logo {
                    next.logo = Some(logo);
                    items.push(Item::Logo);
                }
                if let Some(accent) = change.accent {
                    next.accent = accent;
                    items.push(Item::Accent);
                }
            }
            Self::Restore => {
                let item = input
                    .get("item")
                    .and_then(Value::as_str)
                    .and_then(Item::parse)
                    .ok_or_else(|| {
                        bilibi_agent_api::invalid("item", "must be name, logo or accent")
                    })?;
                match item {
                    Item::Name => next.name.clear(),
                    Item::Logo => next.logo = None,
                    Item::Accent => next.accent = Accent::default(),
                }
                items.push(item);
            }
        }
        Ok((next, items))
    }
    fn summary(&self) -> &'static str {
        match self {
            Self::Set => "Set this installation's brand",
            Self::Restore => "Restore Bilibi's own brand",
        }
    }
}

impl WriteHandler for Write {
    fn owner(&self) -> &'static str {
        "base/branding"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let sql = client.sql_client().await?;
            let stored = bound()?.stored(sql).await?;
            let (next, items) = self.next(input, &stored)?;
            if let (true, Some(logo)) = (items.contains(&Item::Logo), next.logo) {
                check_logo(sql, scope, logo).await?;
            }
            Ok(Output::from(review(&stored, &next, self.summary())))
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            bilibi_authz::authorize_operator(tx.sql(), scope, MANAGE).await?;
            let branding = bound()?;
            let stored = branding.stored(tx.sql()).await?;
            let (next, items) = self.next(input, &stored)?;
            for item in items {
                let value = match (self, item) {
                    (Self::Restore, _) => None,
                    (Self::Set, Item::Name) => Some(json!(next.name)),
                    (Self::Set, Item::Accent) => Some(json!(next.accent.as_str())),
                    (Self::Set, Item::Logo) => {
                        let logo = next.logo.ok_or(Error::Internal)?;
                        check_logo(tx.sql(), scope, logo).await?;
                        Some(json!(logo))
                    }
                };
                match value {
                    Some(value) => {
                        branding
                            .settings
                            .put(tx, scope, Target::Global, item.key(), value)
                            .await?
                    }
                    None => {
                        branding
                            .settings
                            .delete(tx, scope, Target::Global, item.key())
                            .await?
                    }
                }
            }
            Ok(Output::from(review(&stored, &next, self.summary())))
        })
    }
}

fn empty_input() -> Value {
    json!({"type":"object","properties":{}})
}
fn set_input() -> Value {
    json!({"type":"object","properties":{
        "name":{"type":"string","maxLength":NAME_MAX_CHARS * 4,"description":"The display name shown in place of Bilibi: up to 40 characters, no @, quotes or angle brackets."},
        "accent":{"type":"string","enum":accents(),"description":"The colour of primary buttons, the current menu item and selections."},
        "logo_asset_id":{"type":"integer","minimum":1,"description":"A PNG, JPEG or WebP image already stored in Media as this workspace's own. A person uploads one on System Settings, Branding, Logo. Leave it out to keep the logo."}
    }})
}
fn restore_input() -> Value {
    json!({"type":"object","required":["item"],"properties":{
        "item":{"type":"string","enum":["name","logo","accent"],"description":"The part of the brand to return to Bilibi's own."}
    }})
}
fn get_output() -> Value {
    json!({"type":"object","description":"items (name, logo, accent: the value in effect, what a restore returns to, and whether this installation changed it), the accents to choose from, and can_manage: whether the caller may change the brand."})
}
fn write_output() -> Value {
    json!({"type":"object","description":"A summary, and under plan each changed part (name, logo, accent) as before → after and where the brand shows. An empty plan means nothing changes."})
}

static GET: Get = Get;
static SET: Write = Write::Set;
static RESTORE: Write = Write::Restore;

const fn provenance(line: u32) -> Provenance {
    Provenance {
        module: "base/branding",
        source: file!(),
        line,
    }
}

inventory::submit! { Contribution::new(DEFINITIONS, "base/branding.agent", provenance(line!()), &Definitions {
    operations: &[
        Operation { key:"base.branding.get", title:"Read this installation's brand", summary:"Read the display name, logo and accent colour shown in place of Bilibi, and whether each was changed.", keywords:&["brand","branding","logo","name","accent","colour","white label"], capability:SIGNED_IN, input:empty_input, output:get_output, handler:Handler::Read(&GET), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.branding.set", title:"Set this installation's brand", summary:"Review and set the display name, accent colour or logo shown in place of Bilibi on staff sign-in, the top bar, tab titles and email.", keywords:&["brand","branding","logo","name","accent","colour","white label","rename"], capability:MANAGE, input:set_input, output:write_output, handler:Handler::SafeWrite(&SET), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.branding.restore", title:"Restore Bilibi's own brand", summary:"Review and return the display name, the logo or the accent colour to Bilibi's own.", keywords:&["brand","branding","logo","name","accent","restore","default","undo"], capability:MANAGE, input:restore_input, output:write_output, handler:Handler::SafeWrite(&RESTORE), channels:Channels::Every, step_up:&[] },
    ],
    guides: &[Guide { key:"base.branding", title:"Installation brand", summary:"Show this installation's own name, logo and accent colour in place of Bilibi.", keywords:&["brand","branding","logo","accent","white label"], capability:MANAGE,
        body:"Use base.branding.get to read the display name, logo and accent colour in effect; `can_manage` says whether this person may change them (the platform operator's administrators). Plan base.branding.set with any of `name` (up to 40 characters), `accent` (one of the listed accents) and `logo_asset_id`, show the returned before and after, then wait for approval. A logo is an image already stored in Media: a person uploads one on System Settings (/system/settings), group Branding, row Logo; you cannot send image bytes. Plan base.branding.restore with `item` (name, logo or accent) to return one part to Bilibi's own; Administration, Customisations offers the same restore without AI. The brand shows on staff sign-in, the top bar, tab titles and as the email sender name. A workspace's own email sender name and each customer portal's title and logo are separate and unchanged. The account menu always says Powered by Bilibi." }],
}) }

fn customised_items(value: &Value) -> impl Iterator<Item = (Item, &Value)> {
    let manageable = value["can_manage"] == true;
    value["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(move |_| manageable)
        .filter_map(|row| Some((Item::parse(row["item"].as_str()?)?, row)))
}

/// The Customisations page's rows: each part this installation changed, for
/// the people who may restore it.
fn rows(value: &Value) -> Vec<bilibi_labels::CustomisationRow> {
    customised_items(value)
        .filter(|(_, row)| row["customised"] == true)
        .map(|(item, row)| bilibi_labels::CustomisationRow {
            description: format!(
                "{}: {} → {}",
                item.title(),
                item.default_text(),
                row["shown"].as_str().unwrap_or_default()
            ),
            // The brand's settings carry no revision of their own, so the
            // page shows no change time for these rows.
            audit_kind: "base/branding",
            audit_id: item as i64 + 1,
            revision: 1,
            input: json!({"item": item.as_str()}),
        })
        .collect()
}

fn choices(reads: &[Value]) -> Vec<bilibi_labels::Choice> {
    customised_items(&reads[0])
        .map(|(item, row)| {
            let customised = row["customised"] == true;
            bilibi_labels::Choice {
                subject: item.as_str().to_owned(),
                title: format!("Brand: {}", item.title()),
                keywords: "brand branding logo name accent colour color white label installation sign-in top bar email sender restore",
                value: Some(row["value"].clone()),
                default: Some(json!(item.default_text())),
                source: Some(if customised { "global" } else { "default" }.into()),
                customised,
                input: json!({"item": item.as_str()}),
                verify_input: json!({}),
                limits: match item {
                    Item::Name => json!({"max_characters": NAME_MAX_CHARS}),
                    Item::Logo => json!({"types": ["image/png","image/jpeg","image/webp"], "max_bytes": crate::LOGO_MAX_BYTES}),
                    Item::Accent => json!({"choices": accents()}),
                },
                ..Default::default()
            }
        })
        .collect()
}

inventory::submit! { Contribution::new(bilibi_labels::SECTIONS, "base/branding.customisations", provenance(line!()), &bilibi_labels::CustomisationSection {
    title: "Brand",
    undo: Some(bilibi_labels::Undo { read: "base.branding.get", read_input: empty_input_value, operation: "base.branding.restore", event: "customisation.branding", action: "Restore default", detail: "Bilibi's own will be shown again.", rows }),
    changes: Some(bilibi_labels::Changes {
        purpose: "Show this installation's own name, logo and accent colour in place of Bilibi.",
        limits: "A name of up to 40 characters, one of the listed accent colours, and a PNG, JPEG or WebP logo of at most 1 MiB that a person uploads on System Settings. Only the platform operator's administrators may change it, for every workspace of the installation.",
        guide: "base.branding",
        reads: &[bilibi_labels::Read { operation: "base.branding.get", input: empty_input_value }],
        choices,
        change: &["base.branding.set"],
        preview: None,
        verify: "base.branding.get",
        restore: Some("base.branding.restore"),
    }),
}) }
fn empty_input_value() -> Value {
    json!({})
}
inventory::submit! { Contribution::new(bilibi_agent_api::page::PAGE_BINDINGS, "base.branding.restore", provenance(line!()), &bilibi_agent_api::page::PageBinding { service: "base/labels.customisations", event: "customisation.branding", contract: "base.branding.restore" }) }
