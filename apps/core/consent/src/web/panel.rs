//! The consent panel on the Core person page: the purpose × channel matrix,
//! each cell edited in place (a new version, with where it was captured
//! and its evidence), the history folded under it, and the person's active
//! suppressions, each lifted after a confirmation.
use super::{event, lift, refusal, render, when, LiftDraft, Pages, Refusal};
use crate::{Capture, Channel, Current, Decision, NewAddress, Standing, MANAGE, RECORD};
use askama::Template;
use bilibi_database::{Error, Result};
use bilibi_datetime::DisplayContext;
use bilibi_party_ids::PersonId;
use bilibi_tenancy::Scope;
use bilibi_ui::{
    page::*, Button, ButtonVariant, Card, Choice, CommitStates, CommitStatus, Fact, Facts, Field,
    Fold, Form, Html, IconButton, InlineForm, InputKind, NoticeKind, PanelNotice, RadioGroup,
    SectionHeading,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const FORBIDDEN: &str = "You do not have permission to record consent.";

#[derive(Template)]
#[template(path = "consent_stack.html")]
struct Stack {
    parts: Vec<Html>,
}

#[derive(Template)]
#[template(path = "consent_panel.html")]
struct PanelTemplate<'a> {
    anchor: &'a str,
    card: Html,
    dialog: Html,
}

/// The cell being edited and what has been entered so far.
struct Draft {
    purpose: String,
    channel: Channel,
    granted: String,
    captured_on: String,
    evidence: String,
    errors: BTreeMap<String, Vec<String>>,
}

pub struct Panel {
    pages: Pages,
    person: PersonId,
    anchor: String,
    draft: Option<Draft>,
    statuses: CommitStates,
    lift: Option<LiftDraft>,
    notice: Option<(NoticeKind, String)>,
}
impl Panel {
    pub fn new(pages: Pages, request: &PageRequest<'_, Scope>) -> Result<Self> {
        let person = request
            .path
            .get("person_id")
            .and_then(|v| v.parse::<i64>().ok())
            .and_then(|id| PersonId::new(id).ok())
            .ok_or(Error::Invalid)?;
        let anchor = request.path.get("panel").ok_or(Error::Invalid)?;
        if anchor.is_empty()
            || !anchor
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            pages,
            person,
            anchor: anchor.clone(),
            draft: None,
            statuses: CommitStates::default(),
            lift: None,
            notice: None,
        })
    }

    /// `purpose:channel`, the key a cell's trigger carries.
    fn key(purpose: &str, channel: Channel) -> String {
        format!("{purpose}:{}", channel.code())
    }
    fn parse_key(value: &Value) -> Result<(String, Channel)> {
        let (purpose, channel) = value["key"]
            .as_str()
            .and_then(|key| key.rsplit_once(':'))
            .ok_or(Error::Invalid)?;
        let channel = Channel::parse(channel).ok_or(Error::Invalid)?;
        if purpose.is_empty() || purpose.len() > 64 {
            return Err(Error::Invalid);
        }
        Ok((purpose.to_owned(), channel))
    }

    async fn handle(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        match event(value)? {
            "edit_decision" => {
                let (purpose, channel) = Self::parse_key(value)?;
                let key = Self::key(&purpose, channel);
                // The event checks its capability before any work; the
                // contract checks again inside its transaction.
                if !self.pages.consent.may(scope, RECORD).await? {
                    self.statuses.clear_saved();
                    self.statuses
                        .put(&key, CommitStatus::Error(FORBIDDEN.into()));
                    return Ok(());
                }
                // Nothing is preselected: a save records what the operator
                // chose, never a default that could override a decision the
                // cell inherits.
                self.draft = Some(Draft {
                    granted: String::new(),
                    captured_on: String::new(),
                    evidence: String::new(),
                    errors: BTreeMap::new(),
                    purpose,
                    channel,
                });
            }
            "cancel_decision" => self.draft = None,
            "record" => {
                let Some(draft) = &mut self.draft else {
                    return Ok(());
                };
                draft.granted = value["granted"].as_str().unwrap_or_default().to_owned();
                draft.captured_on = value["captured_on"].as_str().unwrap_or_default().to_owned();
                draft.evidence = value["evidence"].as_str().unwrap_or_default().to_owned();
                draft.errors.clear();
                let granted = match draft.granted.as_str() {
                    "granted" => Some(true),
                    "withdrawn" => Some(false),
                    _ => None,
                };
                if granted.is_none() {
                    draft
                        .errors
                        .entry("granted".into())
                        .or_default()
                        .push("Choose granted or withdrawn.".into());
                }
                if Capture::parse(&draft.captured_on).is_none() {
                    draft
                        .errors
                        .entry("captured_on".into())
                        .or_default()
                        .push("Choose where the person said so.".into());
                }
                let Some(granted) = granted.filter(|_| draft.errors.is_empty()) else {
                    return Ok(());
                };
                let key = Self::key(&draft.purpose, draft.channel);
                let input = json!({
                    "person_id": self.person.get(),
                    "purpose": draft.purpose,
                    "channel": draft.channel.code(),
                    "granted": granted,
                    "captured_on": draft.captured_on,
                    "evidence": draft.evidence,
                });
                let call = self
                    .pages
                    .call(scope, credential, "record", "core.consent.record");
                let result = self.pages.contracts.submit(&call, input).await;
                self.statuses.clear_saved();
                match result {
                    Ok(_) => {
                        self.draft = None;
                        self.statuses.put(&key, CommitStatus::Saved);
                    }
                    Err(error) => {
                        match refusal(error, FORBIDDEN, "The decision was not saved. Try again.")? {
                            Refusal::Fields(fields) => {
                                let mut other = Vec::new();
                                for (field, messages) in fields {
                                    if ["granted", "captured_on", "evidence"]
                                        .contains(&field.as_str())
                                    {
                                        draft.errors.insert(field, messages);
                                    } else {
                                        other.extend(
                                            messages.into_iter().map(|m| format!("{field} {m}")),
                                        );
                                    }
                                }
                                if !other.is_empty() {
                                    self.statuses
                                        .put(&key, CommitStatus::Error(other.join("; ")));
                                }
                            }
                            Refusal::Message(message) => {
                                self.draft = None;
                                self.statuses.put(&key, CommitStatus::Error(message));
                            }
                        }
                    }
                }
            }
            "request_lift" => {
                let id = value["id"]
                    .as_str()
                    .and_then(|v| v.parse::<i64>().ok())
                    .ok_or(Error::Invalid)?;
                if !self.pages.consent.may(scope, MANAGE).await? {
                    self.notice = Some((
                        NoticeKind::Error,
                        "You do not have permission to lift suppressions.".into(),
                    ));
                    return Ok(());
                }
                let company = scope.current_company().ok_or(Error::Forbidden)?.get();
                let suppression = self.pages.consent.suppression(scope, company, id).await?;
                if suppression.contact.person != Some(self.person) {
                    return Err(Error::Invalid);
                }
                self.lift = Some(LiftDraft::new(&suppression, "This person".into()));
            }
            "lift_reason" => {
                if let Some(draft) = &mut self.lift {
                    draft.accept(value);
                }
            }
            "cancel_lift" => self.lift = None,
            "confirm_lift" => {
                let Some(draft) = &mut self.lift else {
                    return Ok(());
                };
                let Some(reason) = draft.ready() else {
                    return Ok(());
                };
                let outcome = lift(&self.pages, scope, credential, draft, &reason).await?;
                self.lift = None;
                self.notice = Some(match outcome {
                    Ok(message) => (NoticeKind::Success, message),
                    Err(message) => (NoticeKind::Error, message),
                });
            }
            "dismiss_notice" => self.notice = None,
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }

    fn read_text(current: Option<&Current>, default: NewAddress) -> String {
        match current {
            Some(c) if c.granted => "Granted".into(),
            Some(_) => "Withdrawn".into(),
            None => format!("No decision ({})", default.label()),
        }
    }
    fn provenance(display: &DisplayContext, current: &Current) -> String {
        format!(
            "{} · {} · v{} · {}",
            current.source.label(),
            current.captured_on.label(),
            current.version,
            when(display, current.recorded_at)
        )
    }

    /// The cell's editor. `current` is what the cell reads now, its own
    /// decision or the one it inherits, shown beside the choice.
    fn editor(&self, dom: &str, draft: &Draft, current: &str) -> Result<Html> {
        let errors = |field: &str| draft.errors.get(field).cloned().unwrap_or_default();
        let mut granted = RadioGroup::new(
            format!("{dom}-granted"),
            "granted",
            bilibi_ui::choices(&[("Granted", "granted"), ("Withdrawn", "withdrawn")]),
        );
        granted.label = "Consent".into();
        granted.value = draft.granted.clone();
        granted.hint = format!("Current decision: {current}.");
        granted.errors = errors("granted");
        let mut options = vec![Choice::new("Choose…", "")];
        options.extend(
            Capture::ALL
                .iter()
                .map(|c| Choice::new(c.label(), c.code())),
        );
        let mut captured = Field::named("captured_on")
            .id(format!("{dom}-captured-on"))
            .label("Captured on")
            .kind(InputKind::Select)
            .value(&draft.captured_on)
            .options(options)
            .required();
        captured.errors = errors("captured_on");
        let mut evidence = Field::named("evidence")
            .id(format!("{dom}-evidence"))
            .label("Evidence")
            .kind(InputKind::Textarea)
            .value(&draft.evidence)
            .attr("maxlength", "2000")
            .hint("A reference, or the wording the person agreed to.");
        evidence.errors = errors("evidence");
        let mut body = render(granted)?;
        body.push(&render(captured)?);
        body.push(&render(evidence)?);
        let mut actions = render(Button {
            id: &format!("{dom}-save"),
            button_type: "submit",
            working: "Saving…",
            variant: ButtonVariant::Primary,
            body: Html::text("Save"),
            ..Button::default()
        })?;
        actions.push(&render(Button {
            id: &format!("{dom}-cancel"),
            button_type: "button",
            event: "cancel_decision",
            body: Html::text("Cancel"),
            ..Button::default()
        })?);
        render(Form {
            id: &format!("{dom}-form"),
            event: "record",
            body,
            actions,
            ..Form::default()
        })
    }

    fn matrix(
        &self,
        display: &DisplayContext,
        standing: &Standing,
        purposes: &[Choice],
        editable: bool,
    ) -> Result<Vec<Html>> {
        let mut parts = Vec::new();
        for (index, purpose) in purposes.iter().enumerate() {
            let heading_id = format!("{}-purpose-{index}", self.anchor);
            let mut html = render(SectionHeading {
                title: &purpose.label,
                id: &heading_id,
                ..SectionHeading::default()
            })?;
            let mut cells = Vec::new();
            for channel in Channel::ALL {
                let dom = format!("{}-p{index}-{}", self.anchor, channel.code());
                let key = Self::key(&purpose.value, *channel);
                let find = |channel: Channel| {
                    standing
                        .current
                        .iter()
                        .find(|c| c.purpose == purpose.value && c.channel == channel)
                };
                let current = find(*channel);
                // With no decision of its own, a channel follows the
                // purpose's any-channel decision, as `check` does.
                let inherited = current.is_none().then(|| find(Channel::Any)).flatten();
                let (text, hint) = match (current, inherited) {
                    (Some(c), _) => (
                        Self::read_text(Some(c), standing.default),
                        Self::provenance(display, c),
                    ),
                    (None, Some(any)) => (
                        format!(
                            "{} (any channel)",
                            Self::read_text(Some(any), standing.default)
                        ),
                        String::new(),
                    ),
                    (None, None) => (Self::read_text(None, standing.default), String::new()),
                };
                let value = if editable {
                    let draft = self
                        .draft
                        .as_ref()
                        .filter(|d| d.purpose == purpose.value && d.channel == *channel);
                    render(InlineForm {
                        compact: false,
                        id: dom.clone(),
                        key: key.clone(),
                        label: format!("{} consent by {}", purpose.label, channel.label()),
                        form: match draft {
                            Some(draft) => self.editor(&dom, draft, &text)?,
                            None => Html::default(),
                        },
                        value: text,
                        hint,
                        editing: draft.is_some(),
                        open_event: "edit_decision".into(),
                        cancel_event: "cancel_decision".into(),
                        status: self.statuses.get(&key),
                    })?
                } else {
                    let mut value = Html::text(&text);
                    if !hint.is_empty() {
                        value.push(&Html::text(&format!(" · {hint}")));
                    }
                    value
                };
                cells.push((format!("{dom}-fact"), channel.label(), value));
            }
            let facts_id = format!("{}-p{index}-facts", self.anchor);
            html.push(&render(Facts {
                id: &facts_id,
                items: cells
                    .iter()
                    .map(|(id, title, value)| Fact {
                        title,
                        id,
                        value: value.clone(),
                    })
                    .collect(),
            })?);
            parts.push(html);
        }
        Ok(parts)
    }

    fn history(
        &self,
        display: &DisplayContext,
        versions: &[Decision],
        labels: &BTreeMap<String, String>,
    ) -> Result<Html> {
        if versions.is_empty() {
            return Ok(Html::default());
        }
        let rows: Vec<(String, String, String)> = versions
            .iter()
            .map(|d| {
                let purpose = labels.get(&d.purpose).unwrap_or(&d.purpose);
                let mut text = format!(
                    "{} · {} on {} · {}",
                    if d.granted { "Granted" } else { "Withdrawn" },
                    d.source.label(),
                    d.captured_on.label(),
                    when(display, d.recorded_at)
                );
                if let Some(evidence) = &d.evidence {
                    text.push_str(&format!(" · {evidence}"));
                }
                (
                    format!("{}-history-{}", self.anchor, d.id),
                    format!("{purpose} · {} · v{}", d.channel.label(), d.version),
                    text,
                )
            })
            .collect();
        let body = render(Facts {
            id: &format!("{}-history-facts", self.anchor),
            items: rows
                .iter()
                .map(|(id, title, text)| Fact {
                    title,
                    id,
                    value: Html::text(text),
                })
                .collect(),
        })?;
        let summary = match versions.len() {
            1 => "1 version".to_owned(),
            n => format!("{n} versions"),
        };
        render(Fold {
            id: &format!("{}-history", self.anchor),
            title: "History",
            summary: &summary,
            open: false,
            body,
        })
    }

    fn suppressions(
        &self,
        display: &DisplayContext,
        standing: &Standing,
        manage: bool,
    ) -> Result<Html> {
        let heading_id = format!("{}-suppressions", self.anchor);
        let mut html = render(SectionHeading {
            title: "Suppressions",
            id: &heading_id,
            count: (!standing.suppressions.is_empty()).then_some(standing.suppressions.len()),
            description: if standing.suppressions.is_empty() {
                "None active."
            } else {
                ""
            },
            ..SectionHeading::default()
        })?;
        if standing.suppressions.is_empty() {
            return Ok(html);
        }
        let mut items = Vec::new();
        for s in &standing.suppressions {
            let id = s.id.to_string();
            let mut text = format!("Since {}", when(display, s.suppressed_at));
            if let Some(evidence) = &s.evidence {
                text.push_str(&format!(" · {evidence}"));
            }
            let mut value = Html::text(&text);
            if manage {
                value.push(&Html::text(" "));
                value.push(&render(IconButton {
                    id: &format!("{}-lift-{id}", self.anchor),
                    event: "request_lift",
                    name: "id",
                    value: &id,
                    ..IconButton::new("lock-open", "Lift")
                })?);
            }
            items.push((
                format!("{}-suppression-{id}", self.anchor),
                format!("{} · {}", s.channel.label(), s.reason.label()),
                value,
            ));
        }
        html.push(&render(Facts {
            id: &format!("{}-suppression-facts", self.anchor),
            items: items
                .iter()
                .map(|(id, title, value)| Fact {
                    title,
                    id,
                    value: value.clone(),
                })
                .collect(),
        })?);
        Ok(html)
    }

    async fn view(&mut self, scope: &Scope) -> Result<LiveBody> {
        let consent = &self.pages.consent;
        let company = scope.current_company().ok_or(Error::Forbidden)?.get();
        let standing = consent.for_person(scope, company, self.person).await?;
        let versions = consent
            .history(scope, company, self.person, None, None)
            .await?;
        let purposes = consent.purposes(scope).await?;
        let editable = consent.may(scope, RECORD).await?;
        let manage = consent.may(scope, MANAGE).await?;
        let display = self.pages.display(scope).await?;
        let labels: BTreeMap<String, String> = purposes
            .iter()
            .map(|p| (p.value.clone(), p.label.clone()))
            .collect();
        if self
            .draft
            .as_ref()
            .is_some_and(|d| !labels.contains_key(&d.purpose))
        {
            self.draft = None;
        }

        let heading_id = format!("{}-heading", self.anchor);
        let description = match standing.default {
            NewAddress::OptedOut => "Required business messages do not use Consent. With no decision, elective contact is opted out.",
            NewAddress::OptedIn => "Required business messages do not use Consent. With no decision, elective contact is opted in.",
        };
        let mut top = render(SectionHeading {
            title: "Consent",
            id: &heading_id,
            description,
            ..SectionHeading::default()
        })?;
        if let Some((kind, message)) = &self.notice {
            top.push(&render(PanelNotice {
                id: &format!("{}-notice", self.anchor),
                kind: *kind,
                dismiss_event: "dismiss_notice",
                body: Html::text(message),
                ..PanelNotice::default()
            })?);
        }
        let mut parts = vec![top];
        parts.extend(self.matrix(&display, &standing, &purposes, editable)?);
        parts.push(self.suppressions(&display, &standing, manage)?);
        parts.push(self.history(&display, &versions, &labels)?);
        let content = render(Stack { parts })?;
        let card = render(Card {
            id: &format!("{}-card", self.anchor),
            inner_class: "p-5 sm:p-6",
            role: "region",
            labelledby: &heading_id,
            body: content,
            ..Card::default()
        })?;
        let dialog = match &self.lift {
            Some(draft) => draft.render(&format!("{}-lift", self.anchor))?,
            None => Html::default(),
        };
        Ok(LiveBody {
            html: render(PanelTemplate {
                anchor: &self.anchor,
                card,
                dialog,
            })?
            .to_string(),
            ..LiveBody::default()
        })
    }
}
impl LiveState<Scope, Error> for Panel {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            match request.input {
                LiveInput::Refresh | LiveInput::Params(_) => {}
                LiveInput::Event(value) => {
                    self.handle(request.scope, request.credential, value)
                        .await?;
                }
            }
            self.view(request.scope).await
        })
    }
}
