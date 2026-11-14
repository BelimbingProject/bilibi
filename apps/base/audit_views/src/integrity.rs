//! "Verify audit log" (ADR 05): `/audit/integrity`, for platform operators.
//! The evidence hash chain links every mutation and action row of a tenant
//! over its non-personal facts, as its transaction commits; verifying
//! recomputes every tenant's chains and says where one breaks, including a
//! committed row with no link.
//!
//! The ledgers can be large, so the page never verifies inside its own event: the
//! press starts one verification (`bilibi_audit::request_verification`) and
//! enqueues the `base/audit-views-verify` job in the same audited transaction.
//! The job walks the chains in bounded steps, storing where it is, and hands
//! over to a fresh job when its time is up. The page shows the latest
//! verification on every draw; the live fallback refresh brings progress and
//! the result without a press. Every finished verification is recorded as
//! `audit.chain.verified`.
use crate::present;
use askama::Template;
use bilibi_audit::{Attribution, ChainReport, Verification};
use bilibi_database::{Db, Error, MutationBoundary, Result};
use bilibi_datetime::{DisplayContext, DisplayPolicy};
use bilibi_locale::Locales;
use bilibi_queue::{Execution, Handler, HandlerFuture, Outcome, Queue};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    page::{
        LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, MountFuture, Page, PageBody,
        PageFuture, PageRequest,
    },
    Button, ButtonVariant, Card, Fact, Facts, FlashMap, Header, Html, SectionHeading, StatusKind,
    NO_FLASH,
};
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) const PATH: &str = "/audit/integrity";
/// The queue worker that walks a verification.
pub const VERIFY_WORKER: &str = "base/audit-views-verify";
/// How long one job walks before it hands over to a fresh one, well inside
/// the executor's attempt timeout.
const JOB_BUDGET: Duration = Duration::from_secs(300);

#[derive(Template)]
#[template(path = "integrity.html")]
struct View<'a> {
    flash: FlashMap<'a>,
    header: Html,
    body: Html,
}

#[derive(Clone)]
pub(crate) struct Pages {
    pub(crate) db: Db,
    pub(crate) boundary: Arc<MutationBoundary>,
    pub(crate) queue: Arc<Queue>,
    pub(crate) display: Arc<DisplayPolicy>,
    pub(crate) locales: Arc<Locales>,
}
struct Integrity {
    pages: Pages,
    flash: Option<(StatusKind, &'static str)>,
}
fn render(fragment: impl Template) -> Result<Html> {
    Html::render(&fragment).map_err(|_| Error::Internal)
}
fn ledger(chain: &str) -> &'static str {
    if chain == "mutations" {
        "Data mutations"
    } else {
        "Actions"
    }
}
fn chain_text(report: &ChainReport) -> String {
    let Some(broken) = &report.break_at else {
        return format!(
            "Whole: {} rows checked in {} tenant chains.",
            report.checked, report.tenants
        );
    };
    let what = match broken.reason.as_str() {
        "missing" => "a row was removed",
        "changed" => "a row's facts no longer match its link",
        "tail" => "the last rows were removed",
        "head" => "a tenant's whole chain was removed",
        "unsealed" => "a committed row was never linked",
        other => other,
    };
    let mut place = String::new();
    if let Some(tenant) = broken.tenant {
        place.push_str(&format!(", tenant {tenant}"));
    }
    if let Some(seq) = broken.seq {
        place.push_str(&format!(", link {seq}"));
    }
    if let Some(row) = broken.row {
        place.push_str(&format!(", row {row}"));
    }
    format!(
        "Broken: {what}{place}. {} rows checked before it.",
        report.checked
    )
}

/// The worker takes one argument: the verification it walks.
pub(crate) fn job_arguments(args: Value) -> Result<Value> {
    match args.as_object() {
        Some(object)
            if object.len() == 1
                && object
                    .get("verification")
                    .and_then(Value::as_i64)
                    .is_some_and(|id| id > 0) =>
        {
            Ok(args)
        }
        _ => Err(Error::Invalid),
    }
}

/// Walks one verification as the operator who asked for it.
pub(crate) struct VerifyJob {
    pub(crate) db: Db,
    pub(crate) queue: Arc<Queue>,
}
impl Handler for VerifyJob {
    fn perform<'a>(&'a self, execution: &'a Execution, args: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let Some(id) = args.get("verification").and_then(Value::as_i64) else {
                return Outcome::Cancel("invalid_verification");
            };
            let Some(scope) = execution.scope() else {
                return Outcome::Cancel("needs_operator");
            };
            match self.walk(execution, scope, id).await {
                Ok(()) => Outcome::Completed,
                Err(Error::Forbidden | Error::NotFound | Error::Invalid) => {
                    Outcome::Cancel("verification_refused")
                }
                Err(_) => Outcome::Retry("verification_failed"),
            }
        })
    }
}
impl VerifyJob {
    /// Steps until the verification finishes or this job's time is up; then a
    /// fresh job continues from the stored place. A repeated job finds the
    /// verification where the last step left it, so nothing is walked twice.
    async fn walk(&self, execution: &Execution, scope: &Scope, id: i64) -> Result<()> {
        let mut client = self.db.get().await?;
        bilibi_tenancy::prove_operator(&**client, scope).await?;
        let who = Attribution::from_context(&scope.mutation_context("audit.chain.verify"))?;
        let started = Instant::now();
        while bilibi_audit::advance_verification(&mut client, id, &who)
            .await?
            .running()
        {
            if started.elapsed() >= JOB_BUDGET {
                let mut tx = execution.begin(&mut client, "audit.chain.verify").await?;
                self.queue
                    .enqueue_for(&mut tx, scope, VERIFY_WORKER, json!({"verification": id}))
                    .await?;
                return tx.commit().await;
            }
        }
        Ok(())
    }
}

impl Integrity {
    /// Starts a verification and its job in one audited transaction. The
    /// route guard already proved the operator; this asks again, since the
    /// evidence is every tenant's.
    async fn verify(&mut self, scope: &Scope) -> Result<()> {
        let mut client = self.pages.db.get().await?;
        bilibi_tenancy::prove_operator(&**client, scope).await?;
        let started: Result<()> = async {
            let mut tx = self
                .pages
                .boundary
                .begin(&mut client, scope.mutation_context("audit.chain.verify"))
                .await?;
            let id = bilibi_audit::request_verification(&mut tx, scope).await?;
            self.pages
                .queue
                .enqueue_for(&mut tx, scope, VERIFY_WORKER, json!({"verification": id}))
                .await?;
            tx.commit().await
        }
        .await;
        self.flash = Some(match started {
            Ok(()) => (
                StatusKind::Success,
                "Verification started. The result appears here when it finishes.",
            ),
            Err(Error::Conflict) => (StatusKind::Error, "A verification is already running."),
            Err(Error::Forbidden) => (
                StatusKind::Error,
                "A verification cannot be started while acting as someone else.",
            ),
            Err(error) => return Err(error),
        });
        Ok(())
    }

    fn body(display: &DisplayContext, latest: Option<&Verification>) -> Result<Html> {
        let mut body = Html::EMPTY;
        let Some(run) = latest else {
            body.push(&render(SectionHeading {
                title: "Not verified yet",
                id: "audit-integrity-pending",
                description: "Start a check to see whether the audit history is intact.",
                ..SectionHeading::default()
            })?);
            return Ok(body);
        };
        let (title, description) = match run.state.as_str() {
            "running" => (
                "Verifying",
                format!(
                    "Checking {}: {} rows so far. This page updates by itself.",
                    ledger(&run.chain).to_lowercase(),
                    run.checked
                ),
            ),
            "whole" => (
                "The audit log is whole",
                "The remaining entries and recorded removals passed verification.".to_owned(),
            ),
            "broken" => (
                "The audit log is broken",
                "The ledgers below say where.".to_owned(),
            ),
            _ => (
                "The last verification stopped",
                "It did not finish. Verify audit log starts a new one.".to_owned(),
            ),
        };
        body.push(&render(SectionHeading {
            title,
            id: "audit-integrity-state",
            description: &description,
            ..SectionHeading::default()
        })?);
        let mut items = vec![Fact {
            title: "Started",
            id: "audit-integrity-started",
            value: present::time(display, "audit-integrity-started-at", run.requested_at, "")?,
        }];
        if let Some(finished) = run.finished_at {
            items.push(Fact {
                title: "Finished",
                id: "audit-integrity-finished",
                value: present::time(display, "audit-integrity-finished-at", finished, "")?,
            });
        }
        for report in &run.reports {
            let mutations = report.chain == "mutations";
            items.push(Fact {
                title: ledger(&report.chain),
                id: if mutations {
                    "audit-integrity-mutations"
                } else {
                    "audit-integrity-actions"
                },
                value: Html::text(&chain_text(report)),
            });
        }
        body.push(&render(Card {
            id: "audit-integrity-card",
            body: render(Facts {
                id: "audit-integrity-facts",
                items,
            })?,
            ..Card::default()
        })?);
        Ok(body)
    }

    async fn render(&self, scope: &Scope) -> Result<String> {
        let (display, latest) = {
            let client = self.pages.db.get().await?;
            let locale = self
                .pages
                .locales
                .resolve(&**client, scope, None)
                .await?
                .locale;
            (
                self.pages
                    .display
                    .display(&**client, scope, None, locale)
                    .await?,
                bilibi_audit::latest_verification(&**client).await?,
            )
        };
        let header = render(Header {
            title: Html::text("Verify audit log"),
            subtitle: "Approved erasures and removals preserve this check; changes to saved values are not detected.",
            actions: render(Button {
                variant: ButtonVariant::Primary,
                event: "verify",
                id: "audit-integrity-verify",
                working: "Starting…",
                body: Html::text("Verify audit log"),
                ..Button::default()
            })?,
            ..Header::default()
        })?;
        let mut flash = NO_FLASH;
        if let Some((kind, message)) = &self.flash {
            match kind {
                StatusKind::Success => flash.success = *message,
                _ => flash.error = *message,
            }
        }
        View {
            flash,
            header,
            body: Self::body(&display, latest.as_ref())?,
        }
        .render()
        .map_err(|_| Error::Internal)
    }
}
impl LiveState<Scope, Error> for Integrity {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            if let LiveInput::Event(value) = request.input {
                match value.get("type").and_then(Value::as_str) {
                    Some("verify") => self.verify(request.scope).await?,
                    Some("clear-flash") => self.flash = None,
                    _ => return Err(Error::Invalid),
                }
            }
            Ok(LiveBody {
                html: self.render(request.scope).await?,
                ..LiveBody::default()
            })
        })
    }
}
impl Page<Scope, Error> for Pages {
    fn title(&self, _operation: &str) -> Option<&'static str> {
        Some("Verify audit log")
    }
    fn render<'a>(&'a self, request: PageRequest<'a, Scope>) -> PageFuture<'a, Error> {
        Box::pin(async move {
            let scope = request.scope.ok_or(Error::Unauthorized)?;
            let page = Integrity {
                pages: self.clone(),
                flash: None,
            };
            Ok(PageBody {
                html: page.render(scope).await?,
                live: true,
                title: None,
            })
        })
    }
    fn mount<'a>(&'a self, _request: PageRequest<'a, Scope>) -> MountFuture<'a, Scope, Error> {
        Box::pin(async move {
            let state: Box<dyn LiveState<Scope, Error>> = Box::new(Integrity {
                pages: self.clone(),
                flash: None,
            });
            Ok(Some(state))
        })
    }
}
