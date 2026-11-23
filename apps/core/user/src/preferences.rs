//! The signed-in account's appearance (theme, time display, regional locale)
//! and sidebar pins.
//!
//! Every call takes the sealed `Scope` of the session, so a request can never
//! name another account. Theme, time-display mode and locale are Settings
//! values at the account's own scope (`ui.theme`, `ui.timezone.mode`,
//! `ui.locale`); pins are rows of `user_pins` keyed by the current tenant and
//! account IDs. Neither touches account membership.
use crate::UserService;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_datetime::Mode;
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_security::Scope;
use bilibi_settings::{Definition, ScopeKind, Setting, Target, Validator, ValueType, DEFINITIONS};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_postgres::GenericClient;

pub const THEME_KEY: &str = "ui.theme";
pub const THEMES: [&str; 3] = ["light", "dark", "system"];
/// Pins per account: storage stays bounded by the account count.
pub const MAX_PINS: i64 = 100;

fn theme_accepts(value: &Value) -> bool {
    value.as_str().is_some_and(|theme| THEMES.contains(&theme))
}
// `system` is the absence of an override, so the stored values are the two
// explicit choices; an editor is not contributed (the appearance page owns it).
inventory::submit! {
    Contribution::new(DEFINITIONS, THEME_KEY,
        Provenance { module: "core/user", source: file!(), line: line!() },
        &Setting::Definition(Definition {
            value_type: ValueType::String,
            scopes: &[ScopeKind::User],
            default: || json!("system"),
            nullable: false, encrypted: false, minimum: None, maximum: None,
            validator: Some(Validator { accepts: theme_accepts, message: "must be light, dark or system" }),
            editor: None,
        }))
}

/// Why an appearance or pin write did not happen, beyond infrastructure
/// failures: a page maps `Invalid` to "Choose a supported value.".
#[derive(Debug, thiserror::Error)]
pub enum PreferenceError {
    #[error("unsupported value")]
    Invalid,
    /// The session is an operator viewing as this account: its appearance
    /// and pins belong to the account, so they are read but never written.
    #[error("impersonating")]
    Impersonating,
    #[error(transparent)]
    Other(#[from] Error),
}
impl From<tokio_postgres::Error> for PreferenceError {
    fn from(error: tokio_postgres::Error) -> Self {
        Error::from(error).into()
    }
}

/// One resolved snapshot for every surface that shows the preferences: the
/// shell's top-bar controls, the appearance page and the preference endpoint.
#[derive(Clone, Debug)]
pub struct Presentation {
    pub theme: &'static str,
    pub mode: Mode,
    pub timezone: String,
    pub locale: &'static str,
    pub language: &'static str,
    /// The account's own locale choice; empty follows the installation.
    pub own_locale: &'static str,
}

/// The account's resolved theme, time display and locale. The locale is
/// resolved once and handed to the display policy.
pub async fn presentation(
    service: &UserService,
    client: &(impl GenericClient + Sync),
    scope: &Scope,
) -> Result<Presentation> {
    let own = Target::User { company: None };
    let theme = service.settings.get(client, scope, own, THEME_KEY).await?;
    let theme = match theme.value.as_str() {
        Some("light") if theme.overridden => "light",
        Some("dark") if theme.overridden => "dark",
        _ => "system",
    };
    let locale = service.locales.resolve(client, scope, None).await?;
    let own_locale = service
        .settings
        .get(client, scope, own, bilibi_locale::LOCALE_KEY)
        .await?;
    let own_locale = if own_locale.overridden {
        own_locale
            .value
            .as_str()
            .and_then(bilibi_locale::normalize)
            .unwrap_or("")
    } else {
        ""
    };
    let display = service
        .display
        .display(client, scope, None, locale.locale)
        .await?;
    Ok(Presentation {
        theme,
        mode: display.mode,
        timezone: display.timezone,
        locale: locale.locale,
        language: locale.language,
        own_locale,
    })
}

impl UserService {
    pub async fn presentation(&self, scope: &Scope) -> Result<Presentation> {
        let client = self.db.get().await?;
        presentation(self, &**client, scope).await
    }

    /// Stores one appearance preference of the signed-in account. `kind` is
    /// `theme`, `timezone` (the time-display mode) or `locale` (empty follows
    /// the installation). An unsupported kind or value is refused, never
    /// guessed.
    pub async fn save_preference(
        &self,
        token: &str,
        csrf: &str,
        kind: &str,
        value: &str,
    ) -> std::result::Result<(), PreferenceError> {
        let mut client = self.db.get().await.map_err(Error::from)?;
        let scope = self.sessions.activity(&**client, token).await?;
        if scope.csrf != csrf {
            return Err(Error::Forbidden.into());
        }
        if scope.impersonator().is_some() {
            return Err(PreferenceError::Impersonating);
        }
        let own = Target::User { company: None };
        match kind {
            "theme" if THEMES.contains(&value) => {
                let mut tx = self
                    .mutations
                    .begin_view_state(&mut client, scope.mutation_context("user.preference.theme"))
                    .await?;
                if value == "system" {
                    self.settings
                        .delete(&mut tx, &scope, own, THEME_KEY)
                        .await?;
                } else {
                    self.settings
                        .put(&mut tx, &scope, own, THEME_KEY, json!(value))
                        .await?;
                }
                tx.commit().await?;
            }
            "timezone" => {
                let mut tx = self
                    .mutations
                    .begin_view_state(
                        &mut client,
                        scope.mutation_context("user.preference.timezone"),
                    )
                    .await?;
                match self.display.put_mode(&mut tx, &scope, value).await {
                    Ok(_) => {}
                    Err(Error::Invalid) => return Err(PreferenceError::Invalid),
                    Err(error) => return Err(error.into()),
                }
                tx.commit().await?;
            }
            "locale" if value.is_empty() || bilibi_locale::supports(value) => {
                let mut tx = self
                    .mutations
                    .begin_view_state(
                        &mut client,
                        scope.mutation_context("user.preference.locale"),
                    )
                    .await?;
                if value.is_empty() {
                    self.locales.delete_user(&mut tx, &scope).await?;
                } else {
                    self.locales.put_user(&mut tx, &scope, value).await?;
                }
                tx.commit().await?;
            }
            _ => return Err(PreferenceError::Invalid),
        }
        Ok(())
    }
}

// ---- Pins -----------------------------------------------------------------

/// One pinned page of the sidebar, in the shape the shell and `/api/pins`
/// exchange.
#[derive(Clone, Debug, Serialize)]
pub struct Pin {
    pub id: i64,
    pub label: String,
    pub url: String,
    pub icon: Option<String>,
    pub sort_order: i32,
}

/// The last non-empty `/` segment of a breadcrumb label.
pub fn normalize_label(label: &str) -> String {
    label
        .split('/')
        .map(str::trim)
        .rfind(|segment| !segment.is_empty())
        .unwrap_or_else(|| label.trim())
        .to_owned()
}

/// The canonical path plus the deterministically sorted query: a scheme and
/// host, the fragment, trailing slashes and parameter order never make a
/// second pin of the same page. `None` when the result would leave the site.
pub fn normalize_url(url: &str) -> Option<String> {
    let url = url.trim();
    let url = url.split('#').next().unwrap_or("");
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (url, None),
    };
    // `http://host/a` keeps only `/a`, as the source's URI parse does.
    let path = match path.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("", |at| &rest[at..]),
        None => path,
    };
    let path = path.trim();
    let mut path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    // `//host/x` is a link to another site, never a page of this one.
    if path.starts_with("//") || path.contains('\\') || path.chars().any(char::is_control) {
        return None;
    }
    let sorted = query
        .and_then(|query| serde_urlencoded::from_str::<Vec<(String, String)>>(query).ok())
        .map(|pairs| {
            // A repeated key keeps its last value, as a decoded map does.
            let mut kept: std::collections::BTreeMap<String, String> = Default::default();
            kept.extend(pairs);
            serde_urlencoded::to_string(kept).unwrap_or_default()
        })
        .unwrap_or_default();
    Some(if sorted.is_empty() {
        path
    } else {
        format!("{path}?{sorted}")
    })
}

/// The account's pins in display order.
pub async fn list_pins(client: &impl GenericClient, scope: &Scope) -> Result<Vec<Pin>> {
    Ok(client
        .query(
            "SELECT id,label,url,icon,sort_order FROM user_pins WHERE tenant_id=$1 AND user_id=$2 ORDER BY sort_order,id",
            &[&scope.tenant(), &scope.user()],
        )
        .await?
        .iter()
        .map(|row| Pin {
            id: row.get(0),
            label: row.get(1),
            url: row.get(2),
            icon: row.get(3),
            sort_order: row.get(4),
        })
        .collect())
}

/// Whether a normalized pin URL names a page this deployment serves; a route
/// pattern's `{param}` segments match any one segment. A pin to a page that is
/// gone stays stored but is not drawn.
pub fn pin_route_is_served(routes: &[&str], url: &str) -> bool {
    let path = url.split('?').next().unwrap_or("");
    let actual: Vec<_> = path.split('/').collect();
    if path != "/"
        && actual.iter().skip(1).any(|segment| {
            segment.is_empty()
                || matches!(
                    segment.to_ascii_lowercase().as_str(),
                    "." | ".." | "%2e" | ".%2e" | "%2e." | "%2e%2e"
                )
        })
    {
        return false;
    }
    routes.iter().any(|route| {
        let pattern: Vec<_> = route.split('/').collect();
        pattern.len() == actual.len()
            && pattern
                .iter()
                .zip(&actual)
                .all(|(p, a)| (p.starts_with('{') && !a.is_empty()) || p == a)
    })
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PinDraft {
    pub label: String,
    pub url: String,
    pub icon: Option<String>,
}

impl PinDraft {
    pub(crate) fn clean(label: &str, url: &str, icon: Option<&str>) -> Result<Self> {
        let label = normalize_label(label);
        let icon = icon.filter(|icon| !icon.is_empty());
        if url.len() > 8192
            || label.is_empty()
            || label.chars().count() > 150
            || icon
                .is_some_and(|icon| icon.chars().count() > 100 || !bilibi_ui::Icons::is_known(icon))
        {
            return Err(Error::Invalid);
        }
        let url = normalize_url(url)
            .filter(|url| url.len() <= 8192)
            .ok_or(Error::Invalid)?;
        Ok(Self {
            label,
            url,
            icon: icon.map(str::to_owned),
        })
    }
}

impl From<&Pin> for PinDraft {
    fn from(pin: &Pin) -> Self {
        Self {
            label: pin.label.clone(),
            url: pin.url.clone(),
            icon: pin.icon.clone(),
        }
    }
}

/// Every pin writer locks the account before reading the list it will edit.
pub(crate) async fn lock_pins(tx: &MutationTransaction<'_>, scope: &Scope) -> Result<()> {
    if scope.impersonator().is_some() {
        return Err(Error::Forbidden);
    }
    tx.sql()
        .query_one(
            "SELECT id FROM users WHERE tenant_id=$1 AND id=$2 FOR NO KEY UPDATE",
            &[&scope.tenant(), &scope.user()],
        )
        .await?;
    Ok(())
}

pub(crate) async fn apply_pins(
    tx: &mut MutationTransaction<'_>,
    scope: &Scope,
    before: Vec<Pin>,
    after: Vec<PinDraft>,
) -> Result<Vec<Pin>> {
    if after.len() > usize::try_from(MAX_PINS).map_err(|_| Error::Internal)? {
        return Err(Error::Invalid);
    }
    let (tenant, user) = (scope.tenant(), scope.user());
    tx.mutate(move |sql| Box::pin(async move {
        let mut changes = Vec::new();
        for pin in &before {
            if after.iter().any(|next| next.url == pin.url) {
                continue;
            }
            sql.execute("DELETE FROM user_pins WHERE tenant_id=$1 AND user_id=$2 AND id=$3",
                &[&tenant,&user,&pin.id]).await?;
            changes.push(Change { kind:"user_pin".into(), id:pin.id, old:json!(pin), new:Value::Null });
        }
        for (index, next) in after.into_iter().enumerate() {
            let order = i32::try_from(index).map_err(|_| Error::Invalid)?;
            let old = before.iter().find(|pin| pin.url == next.url);
            if old.is_some_and(|pin| pin.sort_order == order && PinDraft::from(pin) == next) {
                continue;
            }
            let id = match old {
                Some(pin) => {
                    sql.execute(
                        "UPDATE user_pins SET label=$4,icon=$5,sort_order=$6,updated_at=now() WHERE tenant_id=$1 AND user_id=$2 AND id=$3",
                        &[&tenant,&user,&pin.id,&next.label,&next.icon,&order],
                    ).await?;
                    pin.id
                }
                None => sql.query_one(
                    "INSERT INTO user_pins(tenant_id,user_id,label,url,url_hash,icon,sort_order) VALUES($1,$2,$3,$4,md5($4),$5,$6) RETURNING id",
                    &[&tenant,&user,&next.label,&next.url,&next.icon,&order],
                ).await?.get(0),
            };
            let new = Pin { id, label: next.label, url: next.url, icon: next.icon, sort_order: order };
            changes.push(Change {kind:"user_pin".into(),id,old:json!(old),new:json!(new)});
        }
        Ok(((), changes))
    })).await?;
    list_pins(tx.sql(), scope).await
}

fn check_pin_session(scope: &Scope, csrf: &str) -> std::result::Result<(), PreferenceError> {
    if scope.csrf != csrf {
        return Err(Error::Forbidden.into());
    }
    if scope.impersonator().is_some() {
        return Err(PreferenceError::Impersonating);
    }
    Ok(())
}

impl UserService {
    pub fn pin_routes(&self) -> &[&'static str] {
        &self.pin_routes
    }

    /// Toggle the signed-in account's pin and return the resulting list.
    pub async fn toggle_pin(
        &self,
        token: &str,
        csrf: &str,
        label: &str,
        url: &str,
        icon: Option<&str>,
    ) -> std::result::Result<(bool, Vec<Pin>), PreferenceError> {
        let draft = PinDraft::clean(label, url, icon).map_err(|_| PreferenceError::Invalid)?;
        let mut client = self.db.get().await.map_err(Error::from)?;
        let scope = self.sessions.activity(&**client, token).await?;
        check_pin_session(&scope, csrf)?;
        let mut tx = self
            .mutations
            .begin_view_state(&mut client, scope.mutation_context("user.pin.toggle"))
            .await?;
        lock_pins(&tx, &scope).await?;
        let before = list_pins(tx.sql(), &scope).await?;
        let mut after: Vec<_> = before.iter().map(PinDraft::from).collect();
        let pinned = if let Some(index) = after.iter().position(|pin| pin.url == draft.url) {
            after.remove(index);
            false
        } else {
            if after.len() as i64 >= MAX_PINS || !pin_route_is_served(self.pin_routes(), &draft.url)
            {
                return Err(PreferenceError::Invalid);
            }
            after.push(draft);
            true
        };
        let pins = apply_pins(&mut tx, &scope, before, after).await?;
        tx.commit().await?;
        Ok((pinned, pins))
    }

    /// Apply the account's requested order. Unlisted pins keep their relative order.
    pub async fn reorder_pins(
        &self,
        token: &str,
        csrf: &str,
        ordered: &[i64],
    ) -> std::result::Result<Vec<Pin>, PreferenceError> {
        if ordered.len() as i64 > MAX_PINS {
            return Err(PreferenceError::Invalid);
        }
        let mut client = self.db.get().await.map_err(Error::from)?;
        let scope = self.sessions.activity(&**client, token).await?;
        check_pin_session(&scope, csrf)?;
        let mut tx = self
            .mutations
            .begin_view_state(&mut client, scope.mutation_context("user.pin.reorder"))
            .await?;
        lock_pins(&tx, &scope).await?;
        let before = list_pins(tx.sql(), &scope).await?;
        let mut seen = std::collections::HashSet::new();
        let after = ordered
            .iter()
            .filter_map(|id| before.iter().find(|pin| pin.id == *id))
            .chain(before.iter())
            .filter(|pin| seen.insert(pin.id))
            .map(PinDraft::from)
            .collect();
        let pins = apply_pins(&mut tx, &scope, before, after).await?;
        tx.commit().await?;
        Ok(pins)
    }
}
