//! The installed-principal section of the shared automation administration page.
use super::*;
use crate::principals::{LIST, SET};
use bilibi_ui::{Fact, Facts, Modal};

#[derive(Template)]
#[template(
    source = "{% import \"components.html\" as ui %}{% call ui::cluster(parts=parts) %}{% endcall %}",
    ext = "html"
)]
struct Actions {
    parts: Vec<Html>,
}

struct Review {
    input: Value,
    facts: Value,
    error: String,
}

pub(super) struct PrincipalState {
    columns: ListColumns,
    pending: Option<Review>,
}
impl PrincipalState {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            columns: ListColumns::mount(
                "automation-principals",
                vec![
                    BuiltinColumn::new("principal", "System principal"),
                    BuiltinColumn::new("capability", "Declared capability"),
                    BuiltinColumn::new("rule", "Stored rule"),
                    BuiltinColumn::new("effective", "Effective access"),
                    BuiltinColumn::new("actions", "Actions"),
                ],
            )
            .map_err(|_| Error::Internal)?,
            pending: None,
        })
    }
}

fn fact<'a>(title: &'a str, value: &str) -> Fact<'a> {
    Fact {
        title,
        id: "",
        value: Html::text(value),
    }
}

fn token(principal: &str, capability: &str, rule: &str, expected: &str) -> String {
    json!({"principal":principal,"capability":capability,"rule":rule,"expected_rule":expected})
        .to_string()
}

impl State {
    pub(super) async fn principal_event(
        &mut self,
        request: &LiveRequest<'_, Scope>,
        value: &Value,
    ) -> Result<bool> {
        let event = text(&value["type"]);
        match event {
            "clear-flash" => {
                if let Some(review) = self.principals.pending.as_mut() {
                    review.error.clear();
                }
                return Ok(false);
            }
            "principals-grid" => {
                if matches!(
                    self.principals.columns.handle(
                        &params::string_values_checked(value, params::FIELD_LIMIT)
                            .map_err(Error::refused)?
                    ),
                    ColumnsOutcome::Sort(_)
                ) {
                    return Err(Error::Invalid);
                }
            }
            "request-principal" => {
                let input: Value =
                    serde_json::from_str(text(&value["change"])).map_err(|_| Error::Invalid)?;
                let call = Call {
                    scope: request.scope,
                    credential: request.credential,
                    service: PAGES,
                    event,
                    contract: SET,
                };
                match self.pages.contracts.preview(&call, input.clone()).await {
                    Ok(reply) if reply["valid"] == true => {
                        let facts = reply["results"][0]["preview"]["result"].clone();
                        self.pending = None;
                        self.principals.pending = Some(Review { input, facts, error:String::new() });
                    }
                    Err(ContractError::Backend(error @ (Error::Unauthorized | Error::StepUpRequired { .. }))) => return Err(error),
                    _ => self.flash(StatusKind::Error, "Access changed or you do not have permission. Refresh and review it again."),
                }
            }
            "cancel-principal" => self.principals.pending = None,
            "confirm-principal" => {
                let Some(review) = self.principals.pending.as_ref() else {
                    return Ok(true);
                };
                let call = Call {
                    scope: request.scope,
                    credential: request.credential,
                    service: PAGES,
                    event,
                    contract: SET,
                };
                match self
                    .pages
                    .contracts
                    .commit(&call, review.input.clone())
                    .await
                {
                    Ok(_) => {
                        self.principals.pending = None;
                        self.flash(StatusKind::Success, "System principal access changed.");
                    }
                    Err(ContractError::Backend(
                        error @ (Error::Unauthorized | Error::StepUpRequired { .. }),
                    )) => return Err(error),
                    Err(_) => {
                        if let Some(review) = self.principals.pending.as_mut() {
                            review.error = "Access changed or you do not have permission. Cancel and review the current access again.".into();
                        }
                    }
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(super) async fn principal_card(&mut self, scope: &Scope, search: &str) -> Result<Html> {
        let reply = self
            .pages
            .contracts
            .read(scope, LIST, &json!({}))
            .await
            .map_err(backend)?;
        let mut rows = Vec::new();
        for item in reply["result"]["entries"].as_array().into_iter().flatten() {
            let principal = text(&item["principal"]);
            let capability = text(&item["capability"]);
            if !search.is_empty()
                && !principal.to_lowercase().contains(search)
                && !capability.to_lowercase().contains(search)
            {
                continue;
            }
            let current = text(&item["rule"]);
            let mut actions = Vec::new();
            for (rule, label, permission) in [
                ("allow", "Grant", "can_allow"),
                ("deny", "Deny", "can_deny"),
                ("clear", "Clear rule", "can_clear"),
            ] {
                if rule == current || item[permission] != true {
                    continue;
                }
                let label = if rule == "clear" && current == "allow" && item["standing"] != true {
                    "Withdraw"
                } else {
                    label
                };
                let value = token(principal, capability, rule, current);
                actions.push(render(Button {
                    event: "request-principal",
                    name: "change",
                    value: &value,
                    variant: if rule == "allow" {
                        ButtonVariant::Secondary
                    } else {
                        ButtonVariant::Danger
                    },
                    body: Html::text(label),
                    ..Button::default()
                })?);
            }
            let actions = if actions.is_empty() {
                Html::text("No permitted changes.")
            } else {
                render(Actions { parts: actions })?
            };
            let effective = if item["granted"] == true {
                "Granted"
            } else {
                "Not granted"
            };
            let source = if item["standing"] == true {
                "Granted by default"
            } else {
                "No default access"
            };
            rows.push(
                FlexRow::new(format!("{principal}:{capability}"))
                    .slot("principal", Html::text(principal))
                    .slot("capability", Html::text(capability))
                    .slot("rule", Html::text(current))
                    .slot("effective", Html::text(&format!("{effective} · {source}")))
                    .slot("actions", actions),
            );
        }
        self.principals.columns.load(&rows, |row| row.key.clone());
        let mut table = FlexTable::new(
            "automation-principals",
            self.principals.columns.flex_columns(),
            rows,
        );
        table.event = "principals-grid";
        table.zoom = self.principals.columns.zoom;
        table.suggestions = self.principals.columns.flex_suggestions();
        table.add_query = &self.principals.columns.add_query;
        table.caption = "Installed system principal access";
        table.framed = false;
        table.empty = Some(TableEmpty::Content(Html::text(
            "No installed system principals match this search.",
        )));
        let mut body = render(SectionHeading {
            id: "automation-principals-heading",
            title: "System principals",
            description:
                "Deny blocks access; clear a saved rule to use the installation's default.",
            ..SectionHeading::default()
        })?;
        body.push(&render(table)?);
        render(Card {
            id: "automation-principals-card",
            body,
            ..Card::default()
        })
    }

    pub(super) async fn principal_dialog(&self) -> Result<Html> {
        let Some(review) = &self.principals.pending else {
            return Ok(Html::EMPTY);
        };
        let client = self.pages.db.get().await?;
        let instance = bilibi_system::Instance::resolve(&self.pages.settings, &**client).await?;
        let mut body = instance
            .banner("principal-review-instance", "")
            .map_err(|_| Error::Internal)?;
        body.push(&render(Facts {
            id: "principal-review-facts",
            items: vec![
                fact("Principal", text(&review.facts["principal"])),
                fact("Capability", text(&review.facts["capability"])),
                fact("Stored rule", text(&review.facts["current_rule"])),
                fact("New rule", text(&review.facts["rule"])),
                fact(
                    "Effective access now",
                    if review.facts["currently_granted"] == true {
                        "Granted"
                    } else {
                        "Not granted"
                    },
                ),
                fact(
                    "Granted by default",
                    if review.facts["standing"] == true {
                        "Yes; clearing a deny restores access"
                    } else {
                        "No"
                    },
                ),
            ],
        })?);
        body.push(&render(Actions {
            parts: vec![
                render(Button {
                    event: "cancel-principal",
                    body: Html::text("Cancel"),
                    ..Button::default()
                })?,
                render(Button {
                    event: "confirm-principal",
                    body: Html::text("Confirm"),
                    working: "Saving…",
                    variant: ButtonVariant::Primary,
                    ..Button::default()
                })?,
            ],
        })?);
        render(Modal { id:"principal-review", title:"Change system principal access?", cancel_event:"cancel-principal",
            description:"This changes access for your organisation; clearing a rule restores the installation's default.",
            flash:Some(FlashMap { error:&review.error, ..NO_FLASH }), body, ..Modal::default()
        })
    }
}
