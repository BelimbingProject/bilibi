//! `/settings/calendar-feed`: the signed-in person's calendar feed link
//! (`feed.rs`). They see whether they have one, when it was made and last
//! read; make, replace or revoke it; and see a new link once. An
//! administrator who holds `core.activity.feed.manage` also sees every
//! person's link and revokes any. Every write calls the operation contract
//! agents use (`core.activity.create_feed_key`, `.revoke_feed_key`).
use super::{event, refusal, render, sentences, BrowserZone, Clock, Pages, Refusal};
use crate::{FeedKey, FEED_PATH};
use askama::Template;
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    page::*, params, Badge, BadgeKind, BuiltinColumn, Button, ButtonVariant, Card, ConfirmDialog,
    Fact, Facts, FlashGroup, FlexRow, FlexTable, Header, Html, IconButton, IconKind, ListColumns,
    SectionHeading, StatusKind, NO_FLASH,
};
use serde_json::{json, Value};

#[derive(Template)]
#[template(path = "feed_settings.html")]
struct View {
    flash: Html,
    header: Html,
    own: Html,
    link: Html,
    everyone: Html,
    dialog: Html,
}

/// The link just made, as one copy box (the key is shown only here).
#[derive(Template)]
#[template(
    source = r#"{% import "components.html" as ui %}<div class="space-y-3" data-hook="ClipboardCopy">{% call ui::copy_box(id="feed-link", label="Your calendar feed link", text=link, copy_label="Copy link") %}{% endcall %}</div>"#,
    ext = "html"
)]
struct LinkBox<'a> {
    link: &'a str,
}

/// What a confirmation is about.
enum Confirm {
    Replace,
    Revoke,
    /// An administrator revoking another person's link.
    RevokeUser(i64, String),
}

pub(crate) struct Feed {
    pages: Pages,
    zone: BrowserZone,
    columns: ListColumns,
    /// The link made by this visit, shown until the page is left.
    link: Option<String>,
    confirm: Option<Confirm>,
    flash: Option<(StatusKind, String)>,
}

impl Feed {
    pub fn new(pages: Pages) -> Result<Self> {
        Ok(Self {
            pages,
            zone: BrowserZone::new(),
            columns: ListColumns::mount(
                "feed-keys",
                vec![
                    BuiltinColumn::new("person", "Person"),
                    BuiltinColumn::new("made", "Made"),
                    BuiltinColumn::new("used", "Last read"),
                ],
            )
            .map_err(|_| Error::Internal)?,
            link: None,
            confirm: None,
            flash: None,
        })
    }

    fn url(key: &str) -> String {
        let origin = std::env::var("APP_ORIGIN")
            .unwrap_or_else(|_| "http://localhost:3000".into())
            .trim_end_matches('/')
            .to_owned();
        format!("{origin}{}", FEED_PATH.replace("{token}", key))
    }

    async fn write(
        &mut self,
        scope: &Scope,
        credential: &str,
        event: &str,
        contract: &'static str,
        input: Value,
    ) -> Result<Option<Value>> {
        let call = self.pages.call(scope, credential, event, contract);
        match self.pages.contracts.submit(&call, input).await {
            Ok(result) => Ok(Some(result)),
            Err(error) => {
                let message = match refusal(
                    error,
                    "You do not have permission to change calendar feed links.",
                    "The link was not changed. Try again.",
                )? {
                    Refusal::Message(message) => message,
                    Refusal::Fields(fields) => sentences(&fields),
                };
                self.flash = Some((StatusKind::Error, message));
                Ok(None)
            }
        }
    }

    async fn handle(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        match event(value)? {
            "browser_timezone" => self.zone.accept(value),
            "make_link" | "replace_link" => {
                let name = if event(value)? == "make_link" {
                    "make_link"
                } else {
                    "replace_link"
                };
                self.confirm = None;
                if let Some(result) = self
                    .write(
                        scope,
                        credential,
                        name,
                        "core.activity.create_feed_key",
                        json!({}),
                    )
                    .await?
                {
                    if let Some(key) = result["result"]["key"].as_str() {
                        self.link = Some(Self::url(key));
                    }
                    self.flash = Some((
                        StatusKind::Success,
                        "Link made. Copy it now: it is shown only once.".into(),
                    ));
                }
            }
            "request_replace" => self.confirm = Some(Confirm::Replace),
            "request_revoke" => self.confirm = Some(Confirm::Revoke),
            "request_revoke_user" => {
                let user = value["user"]
                    .as_str()
                    .and_then(|v| v.parse::<i64>().ok())
                    .filter(|id| *id > 0)
                    .ok_or(Error::Invalid)?;
                let name = self
                    .pages
                    .names(scope, &[user])
                    .await?
                    .remove(&user)
                    .unwrap_or_else(|| format!("Account {user}"));
                self.confirm = Some(Confirm::RevokeUser(user, name));
            }
            "cancel_confirm" => self.confirm = None,
            "revoke_link" => {
                let Some(confirm) = self.confirm.take() else {
                    return Ok(());
                };
                let (input, own) = match confirm {
                    Confirm::RevokeUser(user, _) if user != scope.user() => {
                        (json!({"user_id": user}), false)
                    }
                    _ => (json!({}), true),
                };
                if self
                    .write(
                        scope,
                        credential,
                        "revoke_link",
                        "core.activity.revoke_feed_key",
                        input,
                    )
                    .await?
                    .is_some()
                {
                    if own {
                        self.link = None;
                    }
                    self.flash = Some((
                        StatusKind::Success,
                        "Link revoked. It stopped working at once.".into(),
                    ));
                }
            }
            "grid" => {
                let values = params::string_values_checked(value, params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
                let _ = self.columns.handle(&values);
            }
            "clear-flash" => self.flash = None,
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }

    async fn view(&mut self, scope: &Scope) -> Result<LiveBody> {
        let clock = Clock::new(&self.pages, scope, self.zone.zone.as_deref()).await?;
        let mine = self.pages.activities.feed_key(scope).await?;
        let mut actions = Html::EMPTY;
        let facts = vec![
            Fact {
                title: "Link",
                id: "feed-status",
                value: render(match &mine {
                    Some(_) => Badge::status(BadgeKind::Success, "Active"),
                    None => Badge::status(BadgeKind::Neutral, "None"),
                })?,
            },
            Fact {
                title: "Made",
                id: "feed-made",
                value: match &mine {
                    Some(key) => clock.time("feed-made-at", key.created_at)?,
                    None => Html::text("—"),
                },
            },
            Fact {
                title: "Last read",
                id: "feed-used",
                value: match mine.as_ref().and_then(|key| key.last_used_at) {
                    Some(at) => clock.time("feed-used-at", at)?,
                    None => Html::text(if mine.is_some() { "Never" } else { "—" }),
                },
            },
        ];
        if mine.is_some() {
            actions.push(&render(Button {
                id: "feed-replace",
                button_type: "button",
                event: "request_replace",
                body: Html::text("Replace link"),
                ..Button::default()
            })?);
            actions.push(&render(Button {
                id: "feed-revoke",
                button_type: "button",
                event: "request_revoke",
                variant: ButtonVariant::Danger,
                body: Html::text("Revoke link"),
                ..Button::default()
            })?);
        } else {
            actions.push(&render(Button {
                id: "feed-make",
                button_type: "button",
                event: "make_link",
                variant: ButtonVariant::Primary,
                working: "Making…",
                body: Html::text("Make link"),
                ..Button::default()
            })?);
        }
        let mut body = render(Facts {
            id: "feed-facts",
            items: facts,
        })?;
        body.push(&actions);
        let own = render(Card {
            id: "feed-own",
            body,
            ..Card::default()
        })?;
        let link = match &self.link {
            Some(link) => {
                let mut body = render(SectionHeading {
                    id: "feed-link-heading",
                    title: "Your new link",
                    description: "Paste it into your calendar app's “subscribe from URL” (Google Calendar: Other calendars › From URL). Anyone who has it can read your calendar until you revoke it, and it is not shown again.",
                    ..SectionHeading::default()
                })?;
                body.push(&render(LinkBox { link })?);
                render(Card {
                    id: "feed-link-card",
                    body,
                    ..Card::default()
                })?
            }
            None => Html::EMPTY,
        };
        let everyone = self.everyone(scope, &clock).await?;
        let dialog = match &self.confirm {
            None => Html::EMPTY,
            Some(Confirm::Replace) => render(ConfirmDialog {
                id: "feed-confirm",
                consequence: "Replace your calendar link?",
                detail: "The old link stops working at once. Your calendar app needs the new one.",
                confirm: "Replace",
                working: "Replacing…",
                confirm_event: "replace_link",
                cancel_event: "cancel_confirm",
            })?,
            Some(Confirm::Revoke) => render(ConfirmDialog {
                id: "feed-confirm",
                consequence: "Revoke your calendar link?",
                detail: "It stops working at once. Make a new one any time.",
                confirm: "Revoke",
                working: "Revoking…",
                confirm_event: "revoke_link",
                cancel_event: "cancel_confirm",
            })?,
            Some(Confirm::RevokeUser(_, name)) => render(ConfirmDialog {
                id: "feed-confirm",
                consequence: &format!("Revoke {name}’s calendar link?"),
                detail: "It stops working at once. They can make a new one.",
                confirm: "Revoke",
                working: "Revoking…",
                confirm_event: "revoke_link",
                cancel_event: "cancel_confirm",
            })?,
        };
        let mut flash = NO_FLASH;
        if let Some((kind, message)) = &self.flash {
            match kind {
                StatusKind::Success => flash.success = message,
                _ => flash.error = message,
            }
        }
        let html = render(View {
            flash: render(FlashGroup {
                id: "feed-flash",
                flash,
            })?,
            header: render(Header {
                title: Html::text("My settings"),
                ..Header::default()
            })?,
            own,
            link,
            everyone,
            dialog,
        })?;
        let region = render(super::Region {
            id: "feed-region",
            target: false,
            timezone_request: self.zone.request(),
            body: html,
        })?;
        Ok(LiveBody {
            html: region.to_string(),
            ..LiveBody::default()
        })
    }

    /// Every person's link, for an administrator; nothing for anyone else.
    async fn everyone(&mut self, scope: &Scope, clock: &Clock) -> Result<Html> {
        let keys: Vec<FeedKey> = match self.pages.activities.feed_keys(scope).await {
            Ok(keys) => keys,
            Err(Error::Forbidden) => return Ok(Html::EMPTY),
            Err(error) => return Err(error),
        };
        let users: Vec<i64> = keys.iter().map(|key| key.user).collect();
        let names = self.pages.names(scope, &users).await?;
        let _ = self.columns.load(&keys, |key| key.user.to_string());
        let mut rows = Vec::with_capacity(keys.len());
        for key in &keys {
            let id = key.user.to_string();
            let mut row = FlexRow::new(&id).slot(
                "person",
                Html::text(names.get(&key.user).map_or("—", String::as_str)),
            );
            row.cells.insert(
                "made".into(),
                bilibi_ui::flex_table::Cell::instant(clock.instant(key.created_at)),
            );
            row.cells.insert(
                "used".into(),
                match key.last_used_at {
                    Some(at) => bilibi_ui::flex_table::Cell::instant(clock.instant(at)),
                    None => bilibi_ui::flex_table::Cell::text("Never"),
                },
            );
            row.actions.push(&render(IconButton {
                id: &format!("feed-revoke-user-{id}"),
                event: "request_revoke_user",
                name: "user",
                value: &id,
                kind: IconKind::Danger,
                title: "Revoke link",
                ..IconButton::new("delete", "Revoke link")
            })?);
            row.dom_id = Some(format!("feed-key-{id}"));
            rows.push(row);
        }
        let mut table = FlexTable::new("feed-keys", self.columns.flex_columns(), rows);
        table.event = "grid";
        table.zoom = self.columns.zoom;
        table.suggestions = self.columns.flex_suggestions();
        table.add_query = &self.columns.add_query;
        table.caption = "Calendar feed links";
        table.actions = true;
        table.framed = false;
        let mut body = render(SectionHeading {
            id: "feed-everyone-heading",
            title: "Everyone's links",
            description:
                "Revoke a link that should no longer work, such as one that was shared by mistake.",
            ..SectionHeading::default()
        })?;
        body.push(&render(table)?);
        render(Card {
            id: "feed-everyone",
            inner_class: "p-0",
            body,
            ..Card::default()
        })
    }
}

impl LiveState<Scope, Error> for Feed {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            if let LiveInput::Event(value) = request.input {
                self.handle(request.scope, request.credential, value)
                    .await?;
            }
            self.view(request.scope).await
        })
    }
}
