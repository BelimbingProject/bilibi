//! Disposable live walkthrough for the shared softphone. This example is not
//! registered by the production app. Its fake source acknowledges actions.
use bilibi_database::{Error, Result};
use bilibi_module_registry::{
    contributions::{Access, Contribution, Method, Route, ROUTES},
    Layer, Module, Provenance, Service,
};
use bilibi_security::Scope;
use bilibi_ui::page::{
    LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
    PageFuture, PageRequest, PageService,
};
use bilibi_ui::{Button, CallAction, CallPhase, Html, PhoneHealth, Softphone, SoftphoneAction};
use std::sync::Arc;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

struct Demo;
struct State {
    phase: CallPhase,
    available: bool,
    version: u64,
    notice: String,
    stale: bool,
}
impl State {
    fn new() -> Self {
        Self {
            phase: CallPhase::Ringing,
            available: true,
            version: 1,
            notice: String::new(),
            stale: false,
        }
    }
    fn render(&self) -> Result<String> {
        let actions = [
            CallAction::Answer,
            CallAction::HangUp,
            CallAction::Hold,
            CallAction::Resume,
            CallAction::Dial,
            CallAction::Transfer,
            CallAction::Conference,
            CallAction::Mute,
            CallAction::Unmute,
        ]
        .into_iter()
        .map(SoftphoneAction::enabled)
        .collect();
        let phone = Softphone {
            id: "walkthrough-phone".into(),
            phase: self.phase,
            health: if self.available {
                PhoneHealth::Available
            } else {
                PhoneHealth::Unavailable("Try again when it reconnects.".into())
            },
            caller: "Example caller".into(),
            actions,
            event: "softphone_action".into(),
            version: if self.stale {
                self.version.saturating_sub(1)
            } else {
                self.version
            }
            .to_string(),
            notice: self
                .notice
                .starts_with("Action not accepted")
                .then(|| self.notice.clone()),
            screen_pop: Html::text("Screen pop: Example customer"),
        };
        let mut tools = Html::EMPTY;
        for (value, label) in [
            ("connecting", "Source: connecting"),
            ("ringing", "Source: ringing"),
            ("talking", "Source: talking"),
            ("held", "Source: held"),
            ("ended", "Source: ended"),
            ("failed", "Source: failed"),
            ("offline", "Source: unavailable"),
            ("online", "Source: available"),
            ("stale", "Source: stale control"),
        ] {
            tools.push(
                &Html::render(&Button {
                    event: "source",
                    name: "state",
                    value,
                    body: Html::text(label),
                    ..Button::default()
                })
                .map_err(|_| Error::Internal)?,
            );
        }
        let phone = Html::render(&phone).map_err(|_| Error::Internal)?;
        Ok(format!("<section id=\"softphone-walkthrough\" data-target=\"page\"><h1>Softphone walkthrough</h1><p>Disposable fake call source; actions are acknowledged by this walkthrough.</p><div class=\"flex flex-wrap gap-2\">{tools}</div>{phone}<p id=\"ack\">{}</p></section>", Html::text(&self.notice)))
    }
}
impl Page<Scope, Error> for Demo {
    fn render<'a>(&'a self, _: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            Ok(PageBody {
                html: State::new().render()?,
                live: true,
                title: None,
            })
        })
    }
    fn mount<'a>(&'a self, _: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            Ok(Some(
                Box::new(State::new()) as Box<dyn LiveState<Scope, Error>>
            ))
        })
    }
}
impl LiveState<Scope, Error> for State {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            match request.input {
                LiveInput::Refresh => {}
                LiveInput::Params(values) if values.is_empty() => {}
                LiveInput::Event(value) if value["type"] == "source" => {
                    match value["state"].as_str() {
                        Some("connecting") => self.phase = CallPhase::Connecting,
                        Some("ringing") => self.phase = CallPhase::Ringing,
                        Some("talking") => self.phase = CallPhase::Talking,
                        Some("held") => self.phase = CallPhase::Held,
                        Some("ended") => self.phase = CallPhase::Ended,
                        Some("failed") => self.phase = CallPhase::Failed,
                        Some("offline") => self.available = false,
                        Some("online") => self.available = true,
                        Some("stale") => self.stale = true,
                        _ => return Err(Error::Invalid),
                    }
                    self.version += 1;
                    self.notice.clear();
                }
                LiveInput::Event(value) if value["type"] == "softphone_action" => {
                    if value["version"].as_str() != Some(&self.version.to_string())
                        || !self.available
                    {
                        self.notice = "Action not accepted. Refresh the call.".into();
                        self.stale = false;
                    } else if let Some(action) = value["action"].as_str() {
                        match action {
                            "answer" | "resume" => self.phase = CallPhase::Talking,
                            "hang_up" => self.phase = CallPhase::Ended,
                            "hold" => self.phase = CallPhase::Held,
                            "dial" => self.phase = CallPhase::Connecting,
                            "transfer" | "conference" | "mute" | "unmute" => {}
                            _ => return Err(Error::Invalid),
                        }
                        self.version += 1;
                        self.notice = format!("Acknowledged: {action}");
                    } else {
                        return Err(Error::Invalid);
                    }
                }
                _ => return Err(Error::Invalid),
            }
            Ok(LiveBody {
                html: self.render()?,
                ..LiveBody::default()
            })
        })
    }
}
inventory::submit! { Module { id:"extensions/softphone_walkthrough", layer:Layer::Extensions, dependencies:&["base/ui"], migrations:&[], services:&[Service { id:"extensions/softphone_walkthrough.pages", factory: |_| Ok(Arc::new(PageService::<Scope, Error>(Arc::new(Demo)))) }] } }
inventory::submit! { Contribution::new(ROUTES, "extensions/softphone_walkthrough.page", Provenance {module:"extensions/softphone_walkthrough",source:file!(),line:line!()}, &Route { company_scope:bilibi_module_registry::contributions::CompanyScope::Tenant, method:Method::Get, path:"/softphone-walkthrough", service:"extensions/softphone_walkthrough.pages", operation:"page", access:Access::SignedIn, framed:true }) }
#[tokio::main]
async fn main() -> Result<()> {
    let db = bilibi_database::pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let origin = std::env::var("APP_ORIGIN").map_err(|_| Error::Invalid)?;
    let bind = std::env::var("BIND_ADDR").map_err(|_| Error::Invalid)?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|_| Error::Internal)?;
    println!("Softphone walkthrough listening at {origin}");
    axum::serve(
        listener,
        bilibi_web::router(bilibi_web::App::new(db, &origin)?),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .map_err(|_| Error::Internal)
}
