//! The saved views of a list page: the strip of views a reader opens, the
//! form that saves the current list as one, and the delete that asks first.
//! A view is a link to the page's own address ([`SharedView::query`]); the
//! store and its rules are [`crate::shared_views`].
use super::{Outcome, PageColumns, ViewCaller};
use crate::shared_views::{Audience, SharedView};
use bilibi_agent_api::page::Call;
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::flex_table::{ViewLink, ViewsBar};
use serde_json::json;
use std::collections::HashMap;

/// What the page knows of its saved views, read afresh by every `load`.
#[derive(Default)]
pub(super) struct Saved {
    shared: Vec<SharedView>,
    /// The roles a view may be shared with; `None` when the reader may not share.
    roles: Option<Vec<(i64, String)>>,
    /// The view whose delete awaits confirmation.
    confirming: Option<i64>,
    /// What the last save or delete said, when it was refused.
    notice: Option<String>,
}

impl PageColumns {
    pub async fn refresh_views(&mut self, scope: &Scope) -> Result<()> {
        if !self.named_views {
            return Ok(());
        }
        self.saved.shared = self.views.shared(scope, &self.table).await?;
        self.saved.roles = self.views.share_roles(scope).await?;
        let confirming = self.saved.confirming;
        if !self
            .saved
            .shared
            .iter()
            .any(|view| Some(view.id) == confirming)
        {
            self.saved.confirming = None;
        }
        Ok(())
    }

    /// The views strip and save form for the flexible table.
    pub(super) fn views_bar(&self) -> ViewsBar {
        let roles = self.saved.roles.clone().unwrap_or_default();
        ViewsBar {
            links: self
                .saved
                .shared
                .iter()
                .map(|view| ViewLink {
                    id: view.id,
                    name: view.name.clone(),
                    query: view.query(),
                    audience: audience_text(view, &roles),
                    deletable: view.owned || self.saved.roles.is_some(),
                })
                .collect(),
            can_share: self.saved.roles.is_some(),
            roles,
            confirming: self.saved.confirming,
            notice: self.saved.notice.clone(),
        }
    }

    /// Applies the views ops; `None` when `op` is not one of them.
    pub(super) async fn handle_views(
        &mut self,
        scope: &Scope,
        params: &HashMap<String, String>,
        caller: ViewCaller<'_>,
    ) -> Result<Option<Outcome>> {
        let get = |key: &str| params.get(key).map(String::as_str).unwrap_or("");
        let id = get("id").parse::<i64>().unwrap_or(0);
        match get("op") {
            "save_view" => {
                self.saved.notice = None;
                if !self.named_views {
                    return Err(Error::Invalid);
                }
                let audience = match get("audience") {
                    "everyone" => json!({"everyone": true}),
                    role if role.starts_with("role:") => {
                        json!({"roles": [role[5..].parse::<i64>().map_err(|_| Error::Invalid)?]})
                    }
                    _ => return Err(Error::Invalid),
                };
                let query = crate::shared_views::clean_params(&caller.query.into_iter().collect())?;
                let result = caller.contracts.submit(&Call { scope, credential: caller.credential, service: caller.service, event: caller.event, contract: "base.grid.view.save" },
                    json!({"page": self.table, "view": self.view.to_json().to_string(), "params": serde_json::to_string(&query).map_err(|_| Error::Invalid)?, "name": get("name"), "audience": audience})).await;
                self.saved.notice = refusal(result)?;
                self.refresh_views(scope).await?;
            }
            "ask_delete_view" => {
                self.saved.notice = None;
                self.saved.confirming = Some(id).filter(|id| *id > 0);
            }
            "keep_view" => {
                self.saved.notice = None;
                self.saved.confirming = None;
            }
            "delete_view" => {
                self.saved.confirming = None;
                if !self.named_views {
                    return Err(Error::Invalid);
                }
                self.saved.notice = refusal(
                    caller
                        .contracts
                        .submit(
                            &Call {
                                scope,
                                credential: caller.credential,
                                service: caller.service,
                                event: caller.event,
                                contract: "base.grid.view.delete",
                            },
                            json!({"id": id}),
                        )
                        .await,
                )?;
                self.refresh_views(scope).await?;
            }
            _ => return Ok(None),
        }
        Ok(Some(Outcome::Update))
    }
}

/// What a refused write tells the reader; any other failure is the page's.
fn refusal<T>(result: bilibi_agent_api::Result<T>) -> Result<Option<String>> {
    use bilibi_agent_api::Error as ContractError;
    match result {
        Ok(_) => Ok(None),
        Err(ContractError::Backend(Error::Refused(message))) => Ok(Some(message)),
        Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
            Ok(Some("You may not share views.".into()))
        }
        Err(ContractError::Backend(Error::NotFound)) => {
            Ok(Some("That view is already gone.".into()))
        }
        Err(ContractError::InvalidInput(fields)) => Ok(Some(
            fields
                .into_values()
                .flatten()
                .collect::<Vec<_>>()
                .join("; "),
        )),
        Err(ContractError::Backend(error)) => Err(error),
        Err(_) => Err(Error::Internal),
    }
}

fn audience_text(view: &SharedView, roles: &[(i64, String)]) -> String {
    match &view.audience {
        Audience::Everyone => "Everyone".into(),
        Audience::Roles(ids) => {
            let names: Vec<&str> = ids
                .iter()
                .filter_map(|id| roles.iter().find(|(role, _)| role == id))
                .map(|(_, name)| name.as_str())
                .collect();
            if names.is_empty() {
                "People with a role it is shared with".into()
            } else {
                format!("Role: {}", names.join(", "))
            }
        }
    }
}
