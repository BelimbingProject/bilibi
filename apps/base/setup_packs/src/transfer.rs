use crate::{
    pack::{distinct, Difference, Issue, Pack},
    SetupPacks,
};
use bilibi_agent_api::{invalid, AudiencePreview, Result};
use bilibi_database::Error;
use bilibi_grid::{Audience, CustomTable, ViewPreview, ViewSetup, ViewTransfer};
use bilibi_tenancy::Scope;
use bilibi_watchers::{WatcherSetup, WatcherTransfer};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tokio_postgres::Transaction;

#[derive(Deserialize)]
pub(crate) struct ViewSelection {
    pub page: String,
    pub id: i64,
}

#[derive(Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct ViewKey {
    page: String,
    name: String,
    role_ids: Vec<i64>,
}

#[derive(Deserialize)]
pub(crate) struct RoleMap {
    from: String,
    to: i64,
}

#[derive(Serialize)]
pub(crate) struct SavedView {
    #[serde(flatten)]
    pub setup: ViewSetup,
    #[serde(skip)]
    pub audience: Audience,
    audience_preview: serde_json::Value,
}

impl SavedView {
    pub fn key(&self) -> ViewKey {
        ViewKey {
            page: self.setup.page.clone(),
            name: self.setup.name.clone(),
            role_ids: match &self.audience {
                Audience::Everyone => Vec::new(),
                Audience::Roles(roles) => roles.clone(),
            },
        }
    }
}

#[derive(Serialize)]
pub(crate) struct SavedWatcher {
    #[serde(flatten)]
    pub setup: WatcherSetup,
    #[serde(skip)]
    pub examples: serde_json::Value,
}

pub(crate) fn sort_pack(pack: &mut Pack) -> Result<()> {
    distinct(
        pack.views
            .iter()
            .map(|view| (&view.page, view.name.trim().to_lowercase())),
        "views",
    )?;
    distinct(
        pack.watchers.iter().map(|watcher| &watcher.name),
        "watchers",
    )?;
    pack.views
        .sort_by(|left, right| (&left.page, &left.name).cmp(&(&right.page, &right.name)));
    pack.watchers
        .sort_by(|left, right| left.name.cmp(&right.name));
    Ok(())
}

impl SetupPacks {
    pub(crate) async fn compare_views(
        &self,
        tx: &Transaction<'_>,
        scope: &Scope,
        views: Vec<ViewSetup>,
        mappings: &[RoleMap],
        preview: ViewPreview<'_>,
        diff: &mut Difference,
    ) -> Result<()> {
        distinct(mappings.iter().map(|mapping| &mapping.from), "role_map")?;
        let mut role_map = BTreeMap::new();
        for mapping in mappings {
            if mapping.to <= 0 || !views.iter().any(|view| view.roles.contains(&mapping.from)) {
                return Err(invalid(
                    "role_map",
                    "Map a role used by the pack to an existing destination role ID.",
                ));
            }
            role_map.insert(mapping.from.as_str(), mapping.to);
        }
        let mut pending = BTreeMap::new();
        for setup in views {
            let entry = format!("shared view {} on {}", setup.name, setup.page);
            let earlier = pending.entry(setup.page.clone()).or_default();
            match self
                .views
                .compare_view(
                    tx,
                    scope,
                    &setup,
                    &role_map,
                    ViewPreview {
                        pending: *earlier,
                        ..preview
                    },
                )
                .await
            {
                Ok(ViewTransfer::Same) => {}
                Ok(ViewTransfer::Add(audience)) => {
                    *earlier += 1;
                    let reviewed = match &audience {
                        Audience::Everyone => AudiencePreview::everyone(),
                        Audience::Roles(roles) => {
                            AudiencePreview::roles(tx, scope, roles.clone()).await?
                        }
                    };
                    let audience_preview = reviewed.review(setup.view["columns"].clone());
                    diff.additions.views.push(SavedView {
                        setup,
                        audience,
                        audience_preview,
                    });
                }
                Ok(ViewTransfer::Missing(reason)) => diff.missing.push(Issue { entry, reason }),
                Ok(ViewTransfer::Collision(reason)) => {
                    diff.collisions.push(Issue { entry, reason })
                }
                Err(Error::Refused(reason)) => diff.invalid.push(Issue { entry, reason }),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub(crate) async fn compare_watchers(
        &self,
        tx: &Transaction<'_>,
        scope: &Scope,
        watchers: Vec<WatcherSetup>,
        mut examples: BTreeMap<String, serde_json::Value>,
        (fields, lock): (&[CustomTable], bool),
        diff: &mut Difference,
    ) -> Result<()> {
        for mut watcher in watchers {
            let entry = format!("watcher {}", watcher.name);
            let examples = examples.remove(&watcher.name).unwrap_or_default();
            let transfer = self
                .watchers
                .compare_draft(tx, scope, &watcher, &examples, fields, lock)
                .await?;
            let warnings = match &transfer {
                WatcherTransfer::Add(warnings) | WatcherTransfer::Invalid(_, warnings) => {
                    warnings.as_slice()
                }
                _ => &[],
            };
            diff.warnings.extend(warnings.iter().map(|reason| Issue {
                entry: entry.clone(),
                reason: reason.clone(),
            }));
            match transfer {
                WatcherTransfer::Same => {}
                WatcherTransfer::Add(_) => {
                    watcher.warnings.clear();
                    diff.additions.watchers.push(SavedWatcher {
                        setup: watcher,
                        examples,
                    });
                }
                WatcherTransfer::Collision => diff.collisions.push(Issue {
                    entry,
                    reason: "The destination has a different or ambiguous watcher with this name."
                        .into(),
                }),
                WatcherTransfer::Missing(reasons) => {
                    diff.missing.extend(reasons.into_iter().map(|reason| Issue {
                        entry: entry.clone(),
                        reason,
                    }))
                }
                WatcherTransfer::Invalid(problems, _) => {
                    diff.invalid
                        .extend(problems.into_iter().map(|(field, problem)| Issue {
                            entry: entry.clone(),
                            reason: format!("{field}: {problem}"),
                        }))
                }
            }
        }
        Ok(())
    }
}
