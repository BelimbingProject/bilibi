//! Manual dashboard specimen, deliberately separate from product routes.
//! Run with an isolated, migrated DATABASE_URL and APP_ORIGIN, then
//! open `/dashboard` (see `apps/base/dashboard` README).
//!
//! It contributes the entries a real module would, so the page, embed dispatch,
//! persisted arrangement and per-panel refresh/permission behaviour can be
//! seen without a product owner:
//!
//! * `ref-once`: no capability, `refresh_interval` 0: reads once, then never.
//! * `ref-live`: no capability, refreshes every second: reads on every host tick.
//! * `ref-notices`: needs `companies.view` on the entry; its panel also
//!   checks the capability itself and shows an em dash when it is gone.
//! * `ref-guarded`: no entry capability but an embed that requires
//!   `companies.view`: when the grant goes the card stays and Web empties
//!   only that panel's anchor.
//! * `ref-section`: a full-width section below the grid.
use askama::Template;
use bilibi_base::dashboard::{contribute, PanelArgs, PanelCache, Widget};
use bilibi_database::{Db, Error, Result};
use bilibi_module_registry::{
    contributions::{Access, Contribution, Embed, EmbedContext, EMBEDS},
    Layer, Module, Provenance, Service,
};
use bilibi_security::Scope;
use bilibi_ui::{
    page::{
        LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageFuture,
        PageRequest, PageService,
    },
    StatItem, StatStrip,
};
use std::{sync::Arc, time::SystemTime};
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

#[derive(Clone, Copy)]
struct Kind {
    title: &'static str,
    /// Checked by the panel itself on every read.
    reads_with: Option<&'static str>,
}
const KINDS: [(&str, Kind); 5] = [
    (
        "once",
        Kind {
            title: "Read once",
            reads_with: None,
        },
    ),
    (
        "live",
        Kind {
            title: "Every second",
            reads_with: None,
        },
    ),
    (
        "notices",
        Kind {
            title: "Notices",
            reads_with: Some("companies.view"),
        },
    ),
    (
        "guarded",
        Kind {
            title: "Guarded embed",
            reads_with: None,
        },
    ),
    (
        "section",
        Kind {
            title: "Reference section",
            reads_with: None,
        },
    ),
];
fn clock() -> String {
    let seconds = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!(
        "{:02}:{:02}:{:02} UTC",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}

struct Panels {
    db: Db,
}
struct Panel {
    db: Db,
    kind: Kind,
    args: PanelArgs,
    cache: PanelCache,
    reads: u32,
}
impl Page<Scope, Error> for Panels {
    fn render<'a>(&'a self, _request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async { Err(Error::NotFound) })
    }
    fn mount<'a>(&'a self, request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            let args = PanelArgs::from_request(&request)?;
            let (_, kind) = KINDS
                .iter()
                .find(|(operation, _)| *operation == request.operation)
                .ok_or(Error::NotFound)?;
            let state: Box<dyn LiveState<Scope, Error>> = Box::new(Panel {
                db: self.db.clone(),
                kind: *kind,
                args,
                cache: PanelCache::default(),
                reads: 0,
            });
            Ok(Some(state))
        })
    }
}
impl LiveState<Scope, Error> for Panel {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            if !matches!(request.input, LiveInput::Refresh) {
                return Err(Error::Invalid);
            }
            if let Some(html) = self.cache.fresh(&self.args) {
                return Ok(LiveBody {
                    html: html.to_owned(),
                    ..LiveBody::default()
                });
            }
            let allowed = match self.kind.reads_with {
                None => true,
                Some(key) => {
                    let client = self.db.get().await?;
                    match bilibi_authz::authorize_key(&**client, request.scope, key).await {
                        Ok(()) => true,
                        Err(Error::Forbidden) => false,
                        Err(error) => return Err(error),
                    }
                }
            };
            // A panel that cannot read shows an em dash and does not query.
            let (reads, at) = if allowed {
                self.reads += 1;
                (self.reads.to_string(), clock())
            } else {
                ("—".into(), "—".into())
            };
            let id = self.args.id();
            let href = if self.args.editing {
                ""
            } else {
                "/live-reference"
            };
            let html = StatStrip {
                id: &id,
                title: self.kind.title,
                href,
                items: vec![
                    StatItem::number("Reads", reads),
                    StatItem::text("Read at", at),
                    StatItem::text("Editing", self.args.editing.to_string()),
                ],
            }
            .render()
            .map_err(|_| Error::Internal)?;
            Ok(LiveBody {
                html: self.cache.store(html),
                ..LiveBody::default()
            })
        })
    }
}

inventory::submit! {
    Module {id: "extensions/dashboard_reference", layer: Layer::Extensions,
        dependencies: &["base/dashboard", "base/ui", "base/database", "base/authz"], migrations: &[], services: &[Service {
            id: "extensions/dashboard_reference.panels", factory: |services| {
                Ok(Arc::new(PageService::<Scope, Error>(Arc::new(Panels {
                    db: services.get::<Db>("base/database.pool")?.as_ref().clone(),
                }))))
            },
        }],
    }
}
macro_rules! embed {
    ($name:literal, $access:expr) => {
        inventory::submit! {
            Contribution::new(EMBEDS, concat!("dashboard-reference.", $name), Provenance {module:"extensions/dashboard_reference", source:file!(), line:line!()}, &Embed {
                slot: concat!("dashboard.reference-", $name), context: EmbedContext::Dashboard,
                service: "extensions/dashboard_reference.panels", operation: $name, access: $access,
            })
        }
    };
}
embed!("once", Access::SignedIn);
embed!("live", Access::SignedIn);
embed!("notices", Access::SignedIn);
embed!("guarded", Access::Capability("companies.view"));
embed!("section", Access::SignedIn);
contribute!(
    "extensions/dashboard_reference",
    Widget::new("ref-once", "Read once", "dashboard-reference.once").order(10)
);
contribute!(
    "extensions/dashboard_reference",
    Widget::new("ref-live", "Every second", "dashboard-reference.live")
        .order(20)
        .refresh(1000)
);
contribute!(
    "extensions/dashboard_reference",
    Widget::new("ref-notices", "Notices", "dashboard-reference.notices")
        .order(30)
        .requires("companies.view")
        .refresh(1000)
);
contribute!(
    "extensions/dashboard_reference",
    Widget::new(
        "ref-guarded",
        "Guarded embed",
        "dashboard-reference.guarded"
    )
    .order(40)
    .refresh(1000)
);
contribute!(
    "extensions/dashboard_reference",
    Widget::new(
        "ref-section",
        "Reference section",
        "dashboard-reference.section"
    )
    .section()
    .order(50)
    .refresh(60_000)
);

#[tokio::main]
async fn main() -> Result<()> {
    let db = bilibi_database::pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let origin = std::env::var("APP_ORIGIN").map_err(|_| Error::Invalid)?;
    let bind = std::env::var("BIND_ADDR").map_err(|_| Error::Invalid)?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|_| Error::Internal)?;
    println!("Dashboard reference listening at {origin}");
    axum::serve(
        listener,
        bilibi_web::staff_router(bilibi_web::App::new(db, &origin)?),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .map_err(|_| Error::Internal)
}
