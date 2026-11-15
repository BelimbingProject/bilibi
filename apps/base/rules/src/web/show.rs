//! `/rules/{id}`: one rule, the version that awaits approval or runs (with
//! the approval review and Approve through a confirmation), the on/off
//! switch and the latest receipts in words.
use super::*;

#[derive(Template)]
#[template(path = "rule.html")]
struct ShowView<'a> {
    flash: FlashMap<'a>,
    rule_id: String,
    header: Html,
    summary: Html,
    versions: Vec<Html>,
    runs: Html,
    dialog: Html,
}

fn run_columns() -> Vec<BuiltinColumn> {
    let mut at = BuiltinColumn::new("at", "When");
    at.kind = ColumnType::Datetime;
    vec![
        at,
        BuiltinColumn::new("record", "Record"),
        BuiltinColumn::new("version", "Version"),
        BuiltinColumn::new("outcome", "Outcome"),
        BuiltinColumn::new("why", "Why"),
    ]
}

/// A review message as a sentence: a capital first letter and a full stop.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    let mut out: String = chars
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default();
    out.push_str(chars.as_str());
    if !out.ends_with('.') {
        out.push('.');
    }
    out
}

/// Why a run ended as it did, in words.
fn reason(code: &str) -> &'static str {
    match code {
        "catalog_changed" => "Not run: the installed fields or actions changed. Draft a new version for review.",
        "record_changed" => "Skipped: the record changed after the rule matched.",
        "chain_budget" => "Stopped: another rule started it, and the chain reached the limit of rules in a row.",
        "rule_off" => "Not run: the rule was switched off.",
        "stale_version" => "Not run: a newer version was approved after this event, so this one no longer runs.",
        "action_limit" => "Not run: it has more actions than allowed per run.",
        "read_refused" => "Not run: automation may not read these records yet. An administrator allows it in Automation access.",
        "restricted_field" => "Not run: a condition reads a field that is hidden from automation.",
        "subject_unavailable" => "Not run: the record is gone or out of reach.",
        "action_refused" => "An action was refused; see its outcome.",
        "actor_refused" => "Not run: automation was stopped here (the workspace is suspended or Automation access was withdrawn).",
        _ => "Not run.",
    }
}
fn effect_reason(code: &str) -> &'static str {
    match code {
        "forbidden" => "refused: automation is not allowed to do this",
        "invalid" => "refused by the record's owner",
        "refused" => "refused by the record's owner",
        "not_found" => "refused: the record is gone",
        "record_changed" => "skipped: the record changed after the rule matched",
        "conflict" => "refused: the record changed",
        "actor_refused" => "refused: automation was stopped",
        _ => "refused",
    }
}
fn why(run: &RunView, described: &crate::review::Described) -> Vec<String> {
    let mut lines = Vec::new();
    match run.state.as_str() {
        _ if run.outcomes.get("restricted").is_some() => {
            lines.push("Which conditions held is hidden: you may not read these records.".into());
        }
        "no_match" | "done" | "partial" | "matched" => {
            if let Some(outcomes) = run.outcomes.as_array() {
                for (i, held) in outcomes.iter().enumerate() {
                    let condition = described.conditions.get(i).cloned().unwrap_or_default();
                    lines.push(format!(
                        "{} {condition}",
                        if held.as_bool() == Some(true) {
                            "Held:"
                        } else {
                            "Did not hold:"
                        }
                    ));
                }
            }
            if outcomes_empty(run) && run.state != "no_match" {
                lines.push("No conditions: every event matches.".into());
            }
        }
        _ => {}
    }
    if let Some(code) = &run.reason {
        if code != "action_refused" || run.effects.is_empty() {
            lines.push(reason(code).into());
        }
    }
    for effect in &run.effects {
        let action = described
            .actions
            .get(effect.position as usize)
            .cloned()
            .unwrap_or_else(|| effect.operation.clone());
        lines.push(match effect.state.as_str() {
            "done" => format!("Did: {action}"),
            _ => {
                let words = effect
                    .result
                    .get("refusal")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| {
                        effect
                            .result
                            .get("fields")
                            .and_then(Value::as_object)
                            .map(|fields| {
                                fields
                                    .values()
                                    .flat_map(|v| v.as_array().into_iter().flatten())
                                    .filter_map(Value::as_str)
                                    .collect::<Vec<_>>()
                                    .join("; ")
                            })
                    });
                let code = effect.reason.as_deref().unwrap_or("refused");
                match words.filter(|w| !w.is_empty()) {
                    Some(words) => format!("Not done: {action}, {} ({words})", effect_reason(code)),
                    None => format!("Not done: {action}, {}", effect_reason(code)),
                }
            }
        });
    }
    lines
}
fn outcomes_empty(run: &RunView) -> bool {
    run.outcomes.as_array().is_none_or(Vec::is_empty)
}
fn outcome_badge(run: &RunView) -> Result<Html> {
    render(match run.state.as_str() {
        "done" => Badge::status(BadgeKind::Success, "Acted"),
        "no_match" => Badge::status(BadgeKind::Neutral, "No match"),
        "partial" => Badge::status(BadgeKind::Warning, "Partly done"),
        "refused" => Badge::status(BadgeKind::Danger, "Refused"),
        _ => Badge::status(BadgeKind::Plain, "Waiting"),
    })
}

/// A pending approval the person asked for and has not yet confirmed.
struct Request {
    number: i32,
}

pub(super) struct Show {
    pages: Pages,
    pub(super) id: i64,
    columns: ListColumns,
    request: Option<Request>,
    flash: Option<(StatusKind, String)>,
}
impl Show {
    pub(super) fn new(pages: Pages, id: i64) -> Result<Self> {
        Ok(Self {
            pages,
            id,
            columns: ListColumns::mount("rule-runs", run_columns()).map_err(|_| Error::Internal)?,
            request: None,
            flash: None,
        })
    }

    async fn event(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        let get = |key: &str| value.get(key).and_then(Value::as_str);
        match get("type").ok_or(Error::Invalid)? {
            "clear-flash" => self.flash = None,
            "grid" => {
                self.columns.handle(
                    &params::string_values_checked(value, params::FIELD_LIMIT)
                        .map_err(Error::refused)?,
                );
            }
            "request_approve" => {
                let number = get("value")
                    .and_then(|v| v.parse::<i32>().ok())
                    .ok_or(Error::Invalid)?;
                self.flash = None;
                self.request = Some(Request { number });
            }
            "cancel_approve" => self.request = None,
            "approve" => {
                let Some(request) = self.request.take() else {
                    return Ok(());
                };
                let call = Call {
                    scope,
                    credential,
                    service: crate::PAGES,
                    event: "approve",
                    contract: "base.rules.approve",
                };
                let outcome = self
                    .pages
                    .contracts
                    .submit(
                        &call,
                        json!({"rule_id": self.id, "version": request.number}),
                    )
                    .await;
                self.answer(
                    outcome,
                    format!(
                        "Version {} approved. It runs on every new event from now on.",
                        request.number
                    ),
                )?;
            }
            "switch" => {
                let enabled = get("value") == Some("on");
                let call = Call {
                    scope,
                    credential,
                    service: crate::PAGES,
                    event: "switch",
                    contract: "base.rules.switch",
                };
                let outcome = self
                    .pages
                    .contracts
                    .submit(&call, json!({"rule_id": self.id, "enabled": enabled}))
                    .await;
                self.answer(
                    outcome,
                    if enabled {
                        "Rule switched on. Its approved version runs on new events again.".into()
                    } else {
                        "Rule switched off. It no longer runs.".into()
                    },
                )?;
            }
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }

    fn answer(
        &mut self,
        outcome: std::result::Result<Value, ContractError>,
        done: String,
    ) -> Result<()> {
        match outcome {
            Ok(_) => self.flash = Some((StatusKind::Success, done)),
            Err(ContractError::Backend(
                error @ (Error::StepUpRequired { .. } | Error::Unauthorized),
            )) => return Err(error),
            Err(ContractError::Forbidden | ContractError::Backend(Error::Forbidden)) => {
                self.flash = Some((StatusKind::Error, DENIED.into()))
            }
            Err(ContractError::InvalidInput(fields)) => {
                let problems: Vec<String> = fields.into_values().flatten().collect();
                self.flash = Some((
                    StatusKind::Error,
                    format!("Nothing changed: {}.", problems.join("; ")),
                ))
            }
            Err(_) => {
                self.flash = Some((
                    StatusKind::Error,
                    "The rule could not be changed; nothing was saved.".into(),
                ))
            }
        }
        Ok(())
    }

    fn version_card(
        &self,
        view: &RuleView,
        version: &VersionView,
        display: &DisplayContext,
        review: Option<&crate::Review>,
        running: bool,
        can_approve: bool,
    ) -> Result<Html> {
        let rules = &self.pages.rules;
        let described = rules.describe(
            &view.summary.trigger,
            &version.definition,
            &version.examples,
        );
        let dom = format!("rule-version-{}", version.number);
        let description = if running {
            "This is the approved version used when the rule is on."
        } else if version.approved_at.is_none() {
            "This version will not run until an administrator approves it."
        } else {
            "Approved earlier; another version runs now."
        };
        let actions = if can_approve && version.approved_at.is_none() {
            let ready = review.is_some_and(crate::Review::ok);
            render(Button {
                variant: ButtonVariant::Primary,
                id: &format!("{dom}-approve"),
                button_type: "button",
                event: "request_approve",
                name: "value",
                value: &version.number.to_string(),
                disabled: !ready,
                body: Html::text("Approve…"),
                ..Button::default()
            })?
        } else {
            Html::EMPTY
        };
        let heading = render(SectionHeading {
            title: &format!("Version {}", version.number),
            id: &format!("{dom}-heading"),
            description,
            actions,
            ..SectionHeading::default()
        })?;
        let matching = if described.matching == "any" {
            "Any of these holds"
        } else {
            "All of these hold"
        };
        let conditions = if described.conditions.is_empty() {
            Html::text("None: every event matches")
        } else {
            let mut html = Html::text(matching);
            html.push(&render(Bullets {
                lines: &described.conditions,
            })?);
            html
        };
        let drafted = format!(
            "{}{}, {}",
            version.drafted_by,
            if version.drafted_via == "agent" {
                "'s AI agent"
            } else {
                ""
            },
            display
                .render(version.drafted_at, Format::DateTime, Precision::Minute)
                .text
        );
        let mut items = vec![
            Fact {
                title: "When",
                id: "",
                value: Html::text(&view.summary.trigger_title),
            },
            Fact {
                title: "If",
                id: "",
                value: conditions,
            },
            Fact {
                title: "Then",
                id: "",
                value: render(Bullets {
                    lines: &described.actions,
                })?,
            },
            Fact {
                title: "Examples",
                id: "",
                value: render(Lines {
                    lines: &described.examples,
                })?,
            },
        ];
        if let Some(words) = &version.explanation {
            items.push(Fact {
                title: "Asked for",
                id: "",
                value: Html::text(words),
            });
        }
        items.push(Fact {
            title: "Drafted",
            id: "",
            value: Html::text(&drafted),
        });
        if let (Some(by), Some(at)) = (&version.approved_by, version.approved_at) {
            let approved = format!(
                "{by}, {}",
                display.render(at, Format::DateTime, Precision::Minute).text
            );
            items.push(Fact {
                title: "Approved",
                id: "",
                value: Html::text(&approved),
            });
        }
        let mut body = heading;
        body.push(&render(Facts {
            id: &format!("{dom}-facts"),
            items,
        })?);
        if let Some(review) = review.filter(|_| version.approved_at.is_none()) {
            if !review.problems.is_empty() {
                let mut lines: Vec<String> = review
                    .problems
                    .iter()
                    .map(|(_, why)| sentence(why))
                    .collect();
                lines.push("Draft a new version that fixes this; your AI agent can draft it from plain words.".into());
                body.push(&render(Alert {
                    kind: StatusKind::Error,
                    id: &format!("{dom}-problems"),
                    class: "",
                    body: render(Bullets { lines: &lines })?,
                })?);
            }
            if !review.warnings.is_empty() {
                body.push(&render(Alert {
                    kind: StatusKind::Warning,
                    id: &format!("{dom}-warnings"),
                    class: "",
                    body: render(Bullets {
                        lines: &review.warnings,
                    })?,
                })?);
            }
        }
        render(Card {
            id: &dom,
            inner_class: "p-5 sm:p-6",
            role: "region",
            labelledby: &format!("{dom}-heading"),
            body,
            ..Card::default()
        })
    }

    pub(super) async fn render(&mut self, scope: &Scope) -> Result<String> {
        let pages = self.pages.clone();
        let client = pages.db.get().await?;
        let display = pages.display(&client, scope).await?;
        let view = pages.rules.get(&**client, scope, self.id).await?;
        let can_approve = bilibi_authz::decide(&**client, scope, APPROVE)
            .await?
            .allowed;
        let latest = view.versions.first();
        let review = match latest {
            Some(version) if can_approve && version.approved_at.is_none() => Some(
                pages
                    .rules
                    .review_approval(&**client, scope, self.id, version.number)
                    .await?,
            ),
            _ => None,
        };
        drop(client);
        let header = render(Header {
            title: Html::text(&view.summary.name),
            subtitle: &format!("When {}", view.summary.trigger_title.to_lowercase()),
            actions: render(Button {
                variant: ButtonVariant::Secondary,
                id: "rule-all",
                href: INDEX,
                body: Html::text("All rules"),
                ..Button::default()
            })?,
            ..Header::default()
        })?;
        let runs_now = match view.summary.state {
            RuleState::NeedsReview => Html::text("Needs review: draft and approve a new version"),
            RuleState::Draft => Html::text("Not yet: approve a version first"),
            state if can_approve => render(Toggle {
                on: state == RuleState::On,
                label: "Rule runs",
                id: "rule-switch",
                event: "switch",
                name: "value",
                value: if state == RuleState::On { "off" } else { "on" },
                disabled: false,
            })?,
            RuleState::On => Html::text("On"),
            RuleState::Off => Html::text("Off"),
        };
        let last = match view.summary.last_run {
            Some(at) => Html::text(&display.render(at, Format::DateTime, Precision::Minute).text),
            None => Html::text("Not yet"),
        };
        let mut summary_body = render(SectionHeading {
            title: "Rule",
            id: "rule-summary-heading",
            ..SectionHeading::default()
        })?;
        summary_body.push(&render(Facts {
            id: "rule-facts",
            items: vec![
                Fact {
                    title: "Status",
                    id: "",
                    value: state_badge(view.summary.state, view.summary.pending)?,
                },
                Fact {
                    title: "Runs",
                    id: "",
                    value: runs_now,
                },
                Fact {
                    title: "Running version",
                    id: "",
                    value: Html::text(
                        &view
                            .summary
                            .running
                            .map_or_else(|| "None".into(), |n| format!("Version {n}")),
                    ),
                },
                Fact {
                    title: "Last run",
                    id: "",
                    value: last,
                },
            ],
        })?);
        let summary = render(Card {
            id: "rule-summary",
            inner_class: "p-5 sm:p-6",
            role: "region",
            labelledby: "rule-summary-heading",
            body: summary_body,
            ..Card::default()
        })?;
        let mut versions = Vec::new();
        if let Some(version) = latest {
            let running = view.summary.running == Some(version.number);
            versions.push(self.version_card(
                &view,
                version,
                &display,
                review.as_ref(),
                running,
                can_approve,
            )?);
        }
        if let Some(running) = view
            .versions
            .iter()
            .skip(1)
            .find(|v| Some(v.number) == view.summary.running)
        {
            versions.push(self.version_card(&view, running, &display, None, true, can_approve)?);
        }
        let mut rows = Vec::new();
        for run in &view.runs {
            let described = latest
                .filter(|v| v.number == run.version)
                .or_else(|| view.versions.iter().find(|v| v.number == run.version))
                .map(|v| {
                    pages
                        .rules
                        .describe(&view.summary.trigger, &v.definition, &v.examples)
                });
            let lines = described.as_ref().map(|d| why(run, d)).unwrap_or_default();
            let id = run.id.to_string();
            let mut row = FlexRow::new(id.clone())
                .slot(
                    "at",
                    render(InstantCell {
                        id: &format!("rule-run-at-{id}"),
                        i: instant(&display, run.created_at),
                    })?,
                )
                .slot("record", Html::text(&run.subject))
                .slot("version", Html::text(&run.version.to_string()))
                .slot("outcome", outcome_badge(run)?)
                .slot("why", render(Lines { lines: &lines })?);
            row.dom_id = Some(format!("rule-run-{id}"));
            rows.push(row);
        }
        self.columns.load(&view.runs, |run| run.id.to_string());
        let mut table = FlexTable::new("rule-runs", self.columns.flex_columns(), rows);
        table.zoom = self.columns.zoom;
        table.caption = "Recent runs";
        table.framed = false;
        if view.runs.is_empty() {
            table.empty = Some(TableEmpty::Content(Html::text(
                "No runs yet. Each new event this rule's trigger records shows here with why it matched and what it did.",
            )));
        }
        let runs = render(Card {
            id: "rule-runs-card",
            title: "Recent runs",
            inner_class: "p-0",
            body: render(table)?,
            ..Card::default()
        })?;
        let dialog = match &self.request {
            Some(request) => {
                let consequence = format!(
                    "Approve version {} of \u{201c}{}\u{201d}?",
                    request.number, view.summary.name
                );
                let detail = format!(
                    "From now on it runs each time \u{201c}{}\u{201d}, without AI. A run still waiting for an earlier version will not act.",
                    view.summary.trigger_title
                );
                render(ConfirmDialog {
                    id: "rule-approve-confirm",
                    consequence: &consequence,
                    detail: &detail,
                    confirm: "Approve",
                    working: "Approving…",
                    confirm_event: "approve",
                    cancel_event: "cancel_approve",
                })?
            }
            None => Html::EMPTY,
        };
        ShowView {
            flash: flash_map(&self.flash),
            rule_id: self.id.to_string(),
            header,
            summary,
            versions,
            runs,
            dialog,
        }
        .render()
        .map_err(|_| Error::Internal)
    }
}
impl LiveState<Scope, Error> for Show {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            match request.input {
                LiveInput::Refresh | LiveInput::Params(_) => {}
                LiveInput::Event(value) => {
                    self.event(request.scope, request.credential, value).await?
                }
            }
            let html = self.render(request.scope).await?;
            Ok(LiveBody {
                html,
                ..LiveBody::default()
            })
        })
    }
}
