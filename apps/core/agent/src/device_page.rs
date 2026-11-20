//! `/oauth/device`: connecting an app with a code, three steps on one
//! address. The code, the approval that names the app, the outcome. A complete
//! code in the address (the app's own link) opens the approval; nothing is
//! approved until the person submits it, and `AgentAccess` decides.
use crate::{
    web::{
        duplicate_views, duplicates, html, only, refusal, replacement, replacement_choice,
        DuplicateView, Pages,
    },
    ConnectionSummary, Decision, DeviceReview, OAuthError,
};
use askama::Template;
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    page::{LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState},
    Button, ButtonVariant, CodeInput, Html, RadioGroup,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

/// The page's name in the browser tab and the shell's search.
pub(crate) const DEVICE_TITLE: &str = "Connect an app with a code";

struct Completed {
    approved: bool,
    agent_id: String,
}
/// Three steps on one address: the code, the approval naming the app, the
/// outcome. A complete code in the address (the app's own link) opens the
/// approval; nothing is approved until the person submits it.
pub(crate) struct DevicePage {
    pages: Pages,
    code: String,
    device: Option<DeviceReview>,
    duplicates: Vec<ConnectionSummary>,
    completed: Option<Completed>,
    error: Option<&'static str>,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DeviceEvent {
    Lookup {
        #[serde(default)]
        user_code: String,
    },
    Approve {
        #[serde(flatten)]
        fields: BTreeMap<String, Value>,
    },
    Deny,
    Another,
    Dismiss,
}
pub struct CompletedView<'a> {
    pub approved: bool,
    pub agent_id: &'a str,
}
impl CompletedView<'_> {
    pub fn title(&self) -> &'static str {
        if self.approved {
            "Connection approved"
        } else {
            "Connection refused"
        }
    }
}
#[derive(Template)]
#[template(path = "device.html")]
struct DeviceView<'a> {
    title: &'static str,
    origin: &'a str,
    connections: Html,
    code_actions: Html,
    approval_actions: Html,
    error: Option<&'static str>,
    code: &'a str,
    code_input: CodeInput,
    device: Option<&'a DeviceReview>,
    duplicates: Vec<DuplicateView>,
    replacement: RadioGroup,
    completed: Option<CompletedView<'a>>,
}
fn device_message(error: &OAuthError) -> &'static str {
    match error {
        OAuthError::PersonRequired | OAuthError::AccessDenied => {
            "Connect from your own account. You cannot approve connections while viewing as someone else."
        }
        OAuthError::SlowDown => "Too many codes tried. Wait a minute, then try again.",
        OAuthError::ExpiredToken => "This code expired. Request a new code from your app.",
        _ => "No app is waiting with this code. Check it against your app, or request a new code there.",
    }
}
impl DevicePage {
    /// `?user_code=` only fills the field.
    fn new(pages: Pages, query: &HashMap<String, String>) -> Result<Self> {
        only(query, &["user_code"])?;
        let code = Self::code(query.get("user_code").map_or("", String::as_str))?;
        Ok(Self {
            pages,
            code,
            device: None,
            duplicates: Vec::new(),
            completed: None,
            error: None,
        })
    }
    /// The page as its address opens it: a complete code goes straight to
    /// the approval step, so the app's link costs the person no retyping.
    pub(crate) async fn open(
        pages: Pages,
        query: &HashMap<String, String>,
        scope: &Scope,
    ) -> Result<Self> {
        let mut page = Self::new(pages, query)?;
        page.lookup_complete(scope).await?;
        Ok(page)
    }
    fn code(raw: &str) -> Result<String> {
        if raw.len() > 64 {
            return Err(Error::Invalid);
        }
        Ok(crate::normalize_user_code(raw))
    }
    /// Looks the code up when it has the full `XXXX-XXXX` shape; a partial
    /// one stays in the field for the person to finish.
    async fn lookup_complete(&mut self, scope: &Scope) -> Result<()> {
        if self.code.len() == 9 {
            let code = self.code.clone();
            self.lookup(scope, &code).await?;
        }
        Ok(())
    }
    fn button(id: &'static str, event: &'static str, primary: bool, text: &str) -> Result<Html> {
        Html::render(&Button {
            variant: if primary {
                ButtonVariant::Primary
            } else {
                ButtonVariant::default()
            },
            id,
            button_type: if event.is_empty() { "submit" } else { "button" },
            event,
            body: Html::text(text),
            ..Button::default()
        })
        .map_err(|_| Error::Internal)
    }
    pub(crate) async fn view(&self, scope: &Scope) -> Result<String> {
        let client = self.pages.db.get().await?;
        let display = self.pages.display(&**client, scope).await?;
        drop(client);
        let completed = self.completed.as_ref().map(|done| CompletedView {
            approved: done.approved,
            agent_id: &done.agent_id,
        });
        let duplicates = duplicate_views(&self.pages, scope, &self.duplicates, &display).await?;
        let mut approval_actions = Html::render(&Button {
            variant: ButtonVariant::Primary,
            id: "device-approve",
            button_type: "submit",
            working: "Connecting…",
            body: Html::text("Approve connection"),
            ..Button::default()
        })
        .map_err(|_| Error::Internal)?;
        approval_actions.push(&Self::button("device-deny", "deny", false, "Refuse")?);
        approval_actions.push(&Self::button(
            "device-other-code",
            "another",
            false,
            "Use another code",
        )?);
        html(DeviceView {
            title: match (&self.device, &completed) {
                (Some(_), _) => "Approve this app?",
                (None, Some(done)) => done.title(),
                (None, None) => DEVICE_TITLE,
            },
            origin: &self.pages.origin,
            connections: Html::render(&Button {
                id: "device-connections",
                href: "/settings/ai-connections",
                body: Html::text("Your connections"),
                ..Button::default()
            })
            .map_err(|_| Error::Internal)?,
            code_actions: Self::button("device-lookup", "", true, "Continue")?,
            approval_actions,
            error: self.error,
            code: &self.code,
            code_input: CodeInput::new(
                "device_user_code",
                "user_code",
                "Code shown by your app",
                8,
            )
            .value(&self.code)
            .group_size(4),
            device: self.device.as_ref(),
            replacement: replacement_choice(&duplicates),
            duplicates,
            completed,
        })
    }
    async fn lookup(&mut self, scope: &Scope, raw: &str) -> Result<()> {
        self.code = Self::code(raw)?;
        self.completed = None;
        match self.pages.access.device(scope, &self.code).await {
            Ok(device) => {
                self.duplicates = duplicates(&self.pages, scope, &device.agent_id).await?;
                self.device = Some(device);
                self.error = None;
            }
            Err(error) => {
                self.device = None;
                self.error = Some(device_message(&refusal(error)?));
            }
        }
        Ok(())
    }
    async fn decide(&mut self, scope: &Scope, decision: Decision, replace: bool) -> Result<()> {
        let Some(device) = &self.device else {
            return Ok(());
        };
        match self
            .pages
            .access
            .decide_replacing(
                scope,
                &self.code,
                decision,
                &if replace {
                    self.duplicates.iter().map(|r| r.id).collect::<Vec<_>>()
                } else {
                    Vec::new()
                },
            )
            .await
        {
            Ok(()) => {
                let approved = decision == Decision::Approve;
                self.completed = Some(Completed {
                    approved,
                    agent_id: device.agent_id.clone(),
                });
                self.device = None;
                self.code = String::new();
                self.error = None;
            }
            Err(error) => {
                // A code that stopped waiting returns to the code step.
                let refused = refusal(error)?;
                if matches!(
                    refused,
                    OAuthError::InvalidUserCode | OAuthError::ExpiredToken
                ) {
                    self.device = None;
                }
                self.error = Some(device_message(&refused));
            }
        }
        Ok(())
    }
    fn reset(&mut self) {
        self.device = None;
        self.duplicates = Vec::new();
        self.completed = None;
        self.code = String::new();
        self.error = None;
    }
}
impl LiveState<Scope, Error> for DevicePage {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(values) => {
                    only(values, &["user_code"])?;
                    let code = Self::code(values.get("user_code").map_or("", String::as_str))?;
                    // Back or Forward to another code; the address the page
                    // itself wrote changes nothing.
                    if code != self.code {
                        self.reset();
                        self.code = code;
                        self.lookup_complete(request.scope).await?;
                    }
                }
                LiveInput::Event(value) => {
                    match serde_json::from_value::<DeviceEvent>(value.clone())
                        .map_err(|_| Error::Invalid)?
                    {
                        DeviceEvent::Lookup { user_code } => {
                            self.lookup(request.scope, &user_code).await?
                        }
                        DeviceEvent::Approve { fields } => {
                            if let Some(replace) = replacement(&fields, !self.duplicates.is_empty())
                            {
                                self.decide(request.scope, Decision::Approve, replace)
                                    .await?
                            } else {
                                self.error = Some(device_message(&OAuthError::InvalidRequest));
                            }
                        }
                        DeviceEvent::Deny => {
                            self.decide(request.scope, Decision::Deny, false).await?
                        }
                        DeviceEvent::Another => self.reset(),
                        DeviceEvent::Dismiss => self.error = None,
                    }
                }
            }
            let mut query = HashMap::new();
            if !self.code.is_empty() {
                query.insert("user_code".into(), self.code.clone());
            }
            Ok(LiveBody {
                html: self.view(request.scope).await?,
                query,
                ..LiveBody::default()
            })
        })
    }
}
