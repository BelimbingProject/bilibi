//! The account's editable arrangement and its owner-side write model.
use crate::{Dashboard, Item, Placement, LAYOUT, SECTIONS};
use bilibi_authz::authorize_key;
use bilibi_database::{Error, MutationTransaction, Result};
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};
use tokio_postgres::Client;

pub const ARRANGEMENTS: &str = "base/dashboard.arrangements";
pub(crate) const MAX_PANELS: usize = 32;
const OWN: Target = Target::User { company: None };

pub struct Arrangements {
    dashboard: Arc<Dashboard>,
    settings: Arc<Settings>,
}
impl Arrangements {
    pub(crate) fn new(dashboard: Arc<Dashboard>, settings: Arc<Settings>) -> Self {
        Self {
            dashboard,
            settings,
        }
    }
}

/// What one account sees right now: the arrangement of the entries it may see
/// and the authorized entries it has not placed.
pub(crate) struct Arrangement {
    pub(crate) grid: Vec<Item>,
    pub(crate) sections: Vec<Item>,
    pub(crate) spare_grid: Vec<Item>,
    pub(crate) spare_sections: Vec<Item>,
    /// The capabilities that withheld every grid entry, for the empty wording.
    pub(crate) withheld: Vec<&'static str>,
    pub(crate) catalogue_empty: bool,
    /// An authorized provider can offer entries when Customize is opened.
    pub(crate) provider_offers: bool,
    /// Hidden stored pins still reserve space, so restoring access cannot
    /// overflow the host's panel limit after adding another entry.
    pub(crate) hidden: usize,
    defaults: BTreeMap<&'static str, Vec<Item>>,
    stored: BTreeMap<&'static str, Option<Vec<String>>>,
}
impl Arrangements {
    pub(crate) async fn arrangement(
        &self,
        client: &tokio_postgres::Client,
        scope: &Scope,
        offers: bool,
        requested: &[String],
    ) -> Result<Arrangement> {
        let mut decided: HashMap<&str, bool> = HashMap::new();
        let capabilities = self
            .dashboard
            .entries()
            .iter()
            .filter_map(|widget| widget.capability)
            .chain(
                self.dashboard
                    .providers()
                    .iter()
                    .filter_map(|p| p.capability),
            );
        for key in capabilities {
            if !decided.contains_key(key) {
                let allowed = match authorize_key(client, scope, key).await {
                    Ok(()) => true,
                    Err(Error::Forbidden) => false,
                    Err(error) => return Err(error),
                };
                decided.insert(key, allowed);
            }
        }
        let offered = |capability: Option<&str>| capability.is_none_or(|key| decided[key]);
        let contributed: Vec<Item> = self
            .dashboard
            .entries()
            .iter()
            .filter(|widget| offered(widget.capability))
            .map(|widget| Item::widget(widget))
            .collect();
        let mut stored = self
            .settings
            .resolve_many(
                client,
                scope,
                Target::User { company: None },
                &[LAYOUT, SECTIONS],
            )
            .await?;
        // Placed ids, so a provider answers those before choosing which of
        // its other entries to offer: a pin never falls past the offer cap.
        let placed_ids: Vec<String> = stored
            .values()
            .filter(|s| s.overridden)
            .filter_map(|s| s.value.as_array())
            .flatten()
            .filter_map(|id| id.as_str().map(str::to_owned))
            .chain(requested.iter().cloned())
            .collect();
        let mut provided: Vec<Item> = Vec::new();
        for provider in self.dashboard.providers() {
            if !offered(provider.capability) {
                continue;
            }
            let prefix = format!("{}-", provider.prefix);
            let pinned: Vec<String> = placed_ids
                .iter()
                .filter_map(|id| id.strip_prefix(&prefix).map(str::to_owned))
                .collect();
            if pinned.is_empty() && !offers {
                continue;
            }
            let entries = match (provider.entries)(client, scope, &pinned, offers).await {
                Ok(entries) => entries,
                Err(Error::Forbidden) => continue,
                Err(error) => return Err(error),
            };
            let (kept, others): (Vec<_>, Vec<_>) =
                entries.into_iter().partition(|e| pinned.contains(&e.key));
            provided.extend(
                kept.into_iter()
                    .chain(others.into_iter().take(crate::MAX_PROVIDED))
                    .filter_map(|entry| Item::provided(provider, entry)),
            );
        }
        let mut hidden = 0;
        let mut defaults = BTreeMap::new();
        let mut saved = BTreeMap::new();
        let mut arrange = |key: &'static str, placement: Placement| -> Result<_> {
            let stored = stored.remove(key).ok_or(Error::Internal)?;
            let own: Vec<_> = contributed
                .iter()
                .filter(|i| i.placement == placement)
                .cloned()
                .collect();
            defaults.insert(key, own.clone());
            let ids: Option<Vec<String>> = stored
                .overridden
                .then(|| serde_json::from_value(stored.value))
                .transpose()
                .map_err(|_| Error::Invalid)?;
            let available: Vec<_> = own
                .iter()
                .chain(provided.iter().filter(|i| i.placement == placement))
                .cloned()
                .collect();
            // No stored row shows the whole authorized catalogue of contributed
            // entries (never a provided one); a stored row keeps its order and
            // withholds ids the account can no longer see without forgetting them.
            let placed: Vec<Item> = if let Some(ids) = &ids {
                ids.iter()
                    .filter_map(|id| available.iter().find(|item| item.id == *id))
                    .cloned()
                    .collect()
            } else {
                own
            };
            if let Some(ids) = &ids {
                hidden += ids.len() - placed.len();
            }
            saved.insert(key, ids);
            let spare = available
                .into_iter()
                .filter(|widget| !placed.iter().any(|p| p.id == widget.id))
                .collect();
            Ok((placed, spare))
        };
        let (grid, spare_grid) = arrange(LAYOUT, Placement::Grid)?;
        let (sections, spare_sections) = arrange(SECTIONS, Placement::Section)?;
        let mut withheld: Vec<_> = self
            .dashboard
            .widgets()
            .iter()
            .filter_map(|widget| widget.capability)
            .collect();
        withheld.sort_unstable();
        withheld.dedup();
        let provider_offers = !offers
            && self.dashboard.providers().iter().any(|provider| {
                provider.placement == Placement::Grid && offered(provider.capability)
            });
        let catalogue_empty = self.dashboard.widgets().is_empty()
            && !provider_offers
            && !provided.iter().any(|i| i.placement == Placement::Grid);
        Ok(Arrangement {
            grid,
            sections,
            spare_grid,
            spare_sections,
            withheld,
            catalogue_empty,
            provider_offers,
            hidden,
            defaults,
            stored: saved,
        })
    }
}

impl Arrangement {
    fn placed(&self, key: &str) -> &[Item] {
        if key == LAYOUT {
            &self.grid
        } else {
            &self.sections
        }
    }

    fn spare(&self, key: &str) -> &[Item] {
        if key == LAYOUT {
            &self.spare_grid
        } else {
            &self.spare_sections
        }
    }

    pub(crate) fn version(&self, scope: &Scope, key: &str) -> String {
        let ids = |items: &[Item]| items.iter().map(|item| item.id.clone()).collect::<Vec<_>>();
        let identity = json!([
            scope.tenant(),
            scope.user(),
            scope.current_company().map(|c| c.get()),
            key,
            self.stored[key],
            ids(self.placed(key)),
            ids(&self.defaults[key])
        ]);
        Sha256::digest(identity.to_string().as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn describe(&self, scope: &Scope, key: &str) -> Value {
        let describe = |items: &[Item]| {
            items
                .iter()
                .map(|item| json!({"id":item.id,"label":item.label}))
                .collect::<Vec<_>>()
        };
        json!({
            "version":self.version(scope, key),
            "placed":describe(self.placed(key)),
            "available":describe(self.spare(key)),
            "default":describe(&self.defaults[key]),
            "customised":self.stored[key].is_some()
        })
    }

    fn plan(&self, key: &str, next: Option<&[String]>) -> Result<Option<Vec<String>>> {
        let other = if key == LAYOUT { SECTIONS } else { LAYOUT };
        let other_count = self.stored[other]
            .as_ref()
            .map_or_else(|| self.defaults[other].len(), Vec::len);
        let Some(next) = next else {
            if self.defaults[key].len() + other_count > MAX_PANELS {
                return Err(Error::refused("Remove entries from the other dashboard region before restoring these defaults."));
            }
            return Ok(None);
        };
        let before = self
            .placed(key)
            .iter()
            .map(|item| item.id.as_str())
            .collect::<HashSet<_>>();
        let allowed = self
            .placed(key)
            .iter()
            .chain(self.spare(key))
            .map(|item| item.id.as_str())
            .collect::<HashSet<_>>();
        let wanted = next.iter().map(String::as_str).collect::<HashSet<_>>();
        if wanted.len() != next.len() || !wanted.is_subset(&allowed) {
            return Err(Error::Invalid);
        }
        // Keep hidden entries and their relative positions when arranging
        // the entries the current workplace allows.
        let mut pending = next.iter().cloned();
        let mut ids = Vec::new();
        for id in self.stored[key].iter().flatten() {
            if !before.contains(id.as_str()) {
                ids.push(id.clone());
            } else if wanted.contains(id.as_str()) {
                ids.extend(pending.next());
            }
        }
        ids.extend(pending);
        if ids.len() + other_count > MAX_PANELS {
            return Err(Error::Invalid);
        }
        Ok(Some(ids))
    }
}

impl Placement {
    fn key(self) -> &'static str {
        match self {
            Self::Grid => LAYOUT,
            Self::Section => SECTIONS,
        }
    }
}

impl Arrangements {
    pub async fn read(&self, client: &Client, scope: &Scope) -> Result<Value> {
        let current = self.arrangement(client, scope, true, &[]).await?;
        Ok(
            json!({"grid":current.describe(scope, LAYOUT),"sections":current.describe(scope, SECTIONS)}),
        )
    }

    pub async fn preview(
        &self,
        client: &Client,
        scope: &Scope,
        placement: Placement,
        version: &str,
        next: Option<&[String]>,
    ) -> Result<Value> {
        let (_, review) = self
            .plan(client, scope, placement.key(), version, next)
            .await?;
        Ok(review)
    }

    async fn plan(
        &self,
        client: &Client,
        scope: &Scope,
        key: &str,
        version: &str,
        next: Option<&[String]>,
    ) -> Result<(Option<Vec<String>>, Value)> {
        if scope.impersonator().is_some() {
            return Err(Error::Forbidden);
        }
        if next.is_some_and(|ids| ids.len() > MAX_PANELS) {
            return Err(Error::Invalid);
        }
        let current = self
            .arrangement(client, scope, false, next.unwrap_or_default())
            .await?;
        if version != current.version(scope, key) {
            return Err(Error::Conflict);
        }
        let planned = current.plan(key, next)?;
        let describe =
            |items: &[Item]| items.iter().map(|item| item.id.clone()).collect::<Vec<_>>();
        let shown = next.map_or_else(|| describe(&current.defaults[key]), <[String]>::to_vec);
        Ok((
            planned,
            json!({
                "region": if key == LAYOUT { "grid" } else { "sections" },
                "before": describe(current.placed(key)),
                "after": shown,
                "uses_default": next.is_none(),
            }),
        ))
    }

    pub async fn apply(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        placement: Placement,
        version: &str,
        next: Option<&[String]>,
    ) -> Result<Value> {
        Self::lock(tx, scope).await?;
        let key = placement.key();
        let (planned, review) = self
            .plan(tx.sql().client(), scope, key, version, next)
            .await?;
        self.save(tx, scope, key, planned).await?;
        Ok(review)
    }

    async fn lock(tx: &MutationTransaction<'_>, scope: &Scope) -> Result<()> {
        if scope.impersonator().is_some() {
            return Err(Error::Forbidden);
        }
        // Lock both regions as one arrangement. They share the host's panel cap.
        let lock = format!("base/dashboard:{}:{}", scope.tenant(), scope.user());
        tx.sql()
            .execute(
                "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
                &[&lock],
            )
            .await?;
        Ok(())
    }

    async fn save(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        key: &str,
        planned: Option<Vec<String>>,
    ) -> Result<()> {
        match planned {
            Some(ids) => {
                let value = json!(ids);
                let current = self.settings.get(tx.sql(), scope, OWN, key).await?;
                if !current.overridden || current.value != value {
                    self.settings.put(tx, scope, OWN, key, value).await?;
                }
            }
            None => self.settings.delete(tx, scope, OWN, key).await?,
        }
        Ok(())
    }
}
