//! `/settings/ownership`: the two-person installation ownership review.
use super::*;
use crate::{OwnershipError, OwnershipState, OFFER_MINUTES};
use bilibi_ui::{
    page::{LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState},
    Alert, Button, ButtonVariant, Card, Form, Header, ProofInput, ProofMethod, RecordPicker,
    StatusKind, StepUpGate,
};

const ACCOUNT: bilibi_ui::NamedKind<'static> = bilibi_ui::NamedKind {
    kind: "core/user",
    prefix: "account",
    label: "Account",
    page: Some("/users"),
};

#[derive(Template)]
#[template(path = "ownership.html")]
struct View {
    flash: Html,
    header: Html,
    current: bilibi_ui::SettingsTab,
    heading: &'static str,
    subheading: &'static str,
    card: Html,
}

enum Pending {
    Offer(i64, String),
    Accept(crate::OwnershipOffer),
}

pub(crate) struct Ownership {
    pages: Pages,
    query: String,
    selected: Option<(i64, String)>,
    pending: Option<Pending>,
    flash: Flash,
    proof_error: Option<String>,
}

fn review_id(value: &Value) -> Result<i64> {
    value
        .get("ownership_id")
        .and_then(Value::as_str)
        .and_then(|id| id.parse().ok())
        .ok_or(Error::Invalid)
}

impl Ownership {
    pub(crate) fn new(pages: Pages) -> Self {
        Self {
            pages,
            query: String::new(),
            selected: None,
            pending: None,
            flash: Flash::default(),
            proof_error: None,
        }
    }

    async fn gate(
        &self,
        scope: &Scope,
        event: &str,
        action: &str,
        subject: &str,
        id: i64,
    ) -> Result<LiveBody> {
        let methods = self
            .pages
            .users
            .step_up
            .enrolled_methods(scope)
            .await?
            .into_iter()
            .map(|method| {
                ProofMethod::new(method.id, method.label, method.code_length, method.numeric)
            })
            .collect::<Vec<_>>();
        let mut body = render(&Alert {
            kind: StatusKind::Info,
            body: Html::text(subject),
            ..Default::default()
        })?;
        if let Some(error) = &self.proof_error {
            body.push(&render(&Alert {
                kind: StatusKind::Error,
                body: Html::text(error),
                ..Default::default()
            })?);
        }
        if methods.is_empty() {
            body.push(&render(&Alert {
                kind: StatusKind::Warning,
                body: Html::text("Set up 2FA before transferring installation ownership."),
                ..Default::default()
            })?);
        } else {
            let mut fields = render(
                &bilibi_ui::Field::named("ownership_id")
                    .kind(bilibi_ui::InputKind::Hidden)
                    .value(id.to_string()),
            )?;
            fields.push(&render(&ProofInput::new(
                "ownership-proof",
                "proof",
                methods,
            ))?);
            body.push(&render(&Form {
                id: "ownership-proof-form",
                event,
                body: fields,
                actions: render(&Button {
                    id: "ownership-proof-submit",
                    button_type: "submit",
                    variant: ButtonVariant::Primary,
                    body: Html::text(action),
                    ..Default::default()
                })?,
                ..Default::default()
            })?);
        }
        Ok(LiveBody {
            html: render(
                &StepUpGate::action("Installation ownership", "Confirm access", body)
                    .map_err(|_| Error::Internal)?,
            )?
            .to_string(),
            ..Default::default()
        })
    }

    async fn body(&self, scope: &Scope, state: &OwnershipState) -> Result<Html> {
        let mut body = Html::EMPTY;
        if let Some(offer) = &state.offer {
            if offer.to_user == scope.user() {
                body.push(&render(&Alert {
                    kind: StatusKind::Warning,
                    body: Html::text(&format!(
                        "{} offered you installation ownership. The offer expires {} minutes after it was made.",
                        offer.from_name, OFFER_MINUTES
                    )),
                    ..Default::default()
                })?);
                body.push(&render(&Form {
                    id: "ownership-response-form",
                    event: "request_accept_ownership",
                    body: render(
                        &bilibi_ui::Field::named("ownership_id")
                            .kind(bilibi_ui::InputKind::Hidden)
                            .value(offer.id.to_string()),
                    )?,
                    actions: render(&Button {
                        id: "ownership-accept",
                        button_type: "submit",
                        variant: ButtonVariant::Primary,
                        body: Html::text("Accept ownership"),
                        ..Default::default()
                    })?,
                    ..Default::default()
                })?);
            } else {
                body.push(&render(&Alert {
                    kind: StatusKind::Info,
                    body: Html::text(&format!(
                        "Waiting for {} to accept. The offer expires {} minutes after it was made.",
                        offer.to_name, OFFER_MINUTES
                    )),
                    ..Default::default()
                })?);
            }
        } else if state.is_owner {
            let hits = if self.selected.is_none() && !self.query.trim().is_empty() {
                self.pages
                    .users
                    .ownership_candidates(scope, &self.query)
                    .await
                    .map_err(|error| match error {
                        OwnershipError::Access(error) => error,
                        _ => Error::Forbidden,
                    })?
                    .into_iter()
                    .map(|choice| (choice.id, choice.name))
                    .collect()
            } else {
                Vec::new()
            };
            let picker = RecordPicker::named(
                ACCOUNT,
                "ownership-target",
                "ownership",
                "target",
                "New owner",
                &self.query,
                &hits,
                self.selected
                    .as_ref()
                    .map(|(id, name)| (*id, name.as_str())),
                ("pick_ownership_target", "change_ownership_target"),
                Vec::new(),
            );
            let form = Form {
                id: "ownership-offer-form",
                event: "search_ownership_target",
                change: true,
                body: render(&picker)?,
                actions: render(&Button {
                    id: "ownership-offer",
                    button_type: "submit",
                    variant: ButtonVariant::Primary,
                    disabled: self.selected.is_none(),
                    body: Html::text("Offer ownership"),
                    ..Default::default()
                })?,
                ..Default::default()
            }
            .submit_event("request_offer_ownership")
            .map_err(|_| Error::Internal)?;
            body.push(&form);
            body.push(&render(&Alert {
                kind: StatusKind::Warning,
                body: Html::text("The new owner will control releases, release trust and every owner-only setting. Your account and roles stay unchanged."),
                ..Default::default()
            })?);
        }
        Ok(body)
    }

    fn failure(&mut self, error: OwnershipError) -> Result<()> {
        if !matches!(error, OwnershipError::Access(Error::StepUpRequired { .. })) {
            self.pending = None;
        }
        match error {
            OwnershipError::Access(Error::Unauthorized) => return Err(Error::Unauthorized),
            OwnershipError::Access(Error::StepUpRequired { reason, .. }) => {
                self.proof_error = Some(match reason {
                    "invalid_code" => "That 2FA code was not accepted.",
                    "locked" => "Too many wrong codes. Wait, then use a fresh code.",
                    "not_enrolled" => "Set up 2FA before transferring ownership.",
                    _ => "Enter the current code from your 2FA method.",
                }.into());
            }
            OwnershipError::Expired => self.flash.error("That ownership offer expired. The installation owner did not change."),
            OwnershipError::Unavailable => self.flash.error("That ownership offer is no longer waiting. The installation owner did not change."),
            OwnershipError::Pending => self.flash.error("Another ownership offer is still waiting."),
            OwnershipError::Target => self.flash.error("Choose another active staff account in the operator tenant."),
            OwnershipError::BrowserOnly | OwnershipError::Access(Error::Forbidden) => self.flash.error("Only a direct browser session of the current owner or named recipient can do that."),
            OwnershipError::Access(error) => return Err(error),
        }
        Ok(())
    }

    async fn view(&self, scope: &Scope) -> Result<LiveBody> {
        let state = self.pages.users.ownership_state(scope).await?;
        let card = render(&Card {
            id: "ownership-card",
            title: "Installation owner",
            body: self.body(scope, &state).await?,
            ..Default::default()
        })?;
        Ok(LiveBody {
            html: View {
                flash: render(&bilibi_ui::FlashGroup {
                    id: "flash-group",
                    flash: self.flash.map(),
                })?,
                header: render(&Header {
                    title: Html::text("My settings"),
                    ..Default::default()
                })?,
                current: bilibi_ui::SettingsTab::Ownership,
                heading: "Installation ownership",
                subheading: "One person holds the installation's highest authority.",
                card,
            }
            .render()
            .map_err(|_| Error::Internal)?,
            ..Default::default()
        })
    }
}

impl LiveState<Scope, Error> for Ownership {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            if let LiveInput::Event(value) = request.input {
                match event(value)? {
                    "clear-flash" => self.flash.dismiss(value.get("key").and_then(Value::as_str)),
                    "search_ownership_target" => {
                        self.query = value
                            .get("ownership[target_search]")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                    }
                    "pick_ownership_target" => {
                        let id = value
                            .get("record")
                            .and_then(Value::as_str)
                            .and_then(|record| ACCOUNT.picked(record))
                            .ok_or(Error::Invalid)?;
                        let name = self
                            .pages
                            .users
                            .ownership_candidates(request.scope, &self.query)
                            .await
                            .map_err(|_| Error::Forbidden)?
                            .into_iter()
                            .find(|choice| choice.id == id)
                            .ok_or(Error::Invalid)?
                            .name;
                        self.selected = Some((id, name));
                        self.query.clear();
                    }
                    "change_ownership_target" => self.selected = None,
                    "request_offer_ownership" => {
                        let target = self.selected.as_ref().cloned().ok_or(Error::Invalid)?;
                        self.pending = Some(Pending::Offer(target.0, target.1));
                        self.proof_error = None;
                    }
                    "confirm_offer_ownership" => {
                        let Some(Pending::Offer(target, _)) = &self.pending else {
                            return Err(Error::Invalid);
                        };
                        if review_id(value)? != *target {
                            return Err(Error::Invalid);
                        }
                        let proof = value
                            .get("proof")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let scope = request.scope.clone().presenting_step_up(proof);
                        match self.pages.users.offer_ownership(&scope, *target).await {
                            Ok(_) => {
                                self.pending = None;
                                self.selected = None;
                                self.flash.success(
                                    "Ownership offered. The named person has 15 minutes to accept.",
                                );
                            }
                            Err(error) => self.failure(error)?,
                        }
                    }
                    "request_accept_ownership" => {
                        let id = review_id(value)?;
                        let state = self.pages.users.ownership_state(request.scope).await?;
                        if let Some(offer) = state
                            .offer
                            .filter(|offer| offer.id == id && offer.to_user == request.scope.user())
                        {
                            self.pending = Some(Pending::Accept(offer));
                            self.proof_error = None;
                        } else {
                            self.pending = None;
                            self.failure(OwnershipError::Unavailable)?;
                        }
                    }
                    "confirm_accept_ownership" => {
                        let Some(Pending::Accept(offer)) = &self.pending else {
                            return Err(Error::Invalid);
                        };
                        if review_id(value)? != offer.id {
                            return Err(Error::Invalid);
                        }
                        let proof = value
                            .get("proof")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let scope = request.scope.clone().presenting_step_up(proof);
                        match self.pages.users.accept_ownership(&scope, offer.id).await {
                            Ok(()) => {
                                self.pending = None;
                                self.flash.success("You are now the installation owner.");
                            }
                            Err(error) => self.failure(error)?,
                        }
                    }
                    _ => return Err(Error::Invalid),
                }
            }
            match &self.pending {
                Some(Pending::Offer(id, name)) => {
                    self.gate(
                        request.scope,
                        "confirm_offer_ownership",
                        "Confirm offer",
                        &format!("Offer installation ownership to {name}."),
                        *id,
                    )
                    .await
                }
                Some(Pending::Accept(offer)) => {
                    self.gate(
                        request.scope,
                        "confirm_accept_ownership",
                        "Accept ownership",
                        &format!(
                            "Accept installation ownership offer #{} from {}.",
                            offer.id, offer.from_name
                        ),
                        offer.id,
                    )
                    .await
                }
                None => self.view(request.scope).await,
            }
        })
    }
}
