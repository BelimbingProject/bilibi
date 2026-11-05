//! One consent and opt-out state that every sender checks (`core/consent`).
//! See `README.md`. This Core module has no Bilimbi counterpart; the person panel
//! follows the Platform's record-panel pattern (Core Address) and the
//! suppressions page its list pattern (Core Person).
mod agent_operations;
mod contributions;
mod model;
mod store;
mod web;

use bilibi_database::{Db, Error, MutationTransaction, Result};
use bilibi_module_registry::{Layer, Module, Service};
use bilibi_party_ids::PersonId;
use bilibi_picklists::PickLists;
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::{Authentication, ReadClient, Scope};
use std::sync::Arc;

pub use contributions::{CHECK, DEFAULT_NEW_ADDRESS, MANAGE, PURPOSES, RECORD, VIEW};
pub use model::{
    normalise, Answer, Capture, Channel, Contact, DecisionInput, FieldError, NewAddress, Question,
    Reason, Refusal, Source, SuppressionInput, ADDRESS_MAX, EVIDENCE_MAX, KEY_MAX, LIFT_REASON_MAX,
};

pub const MODULE: &str = "core/consent";
pub const SERVICE: &str = "core/consent.consent";
/// The audit kinds of a decision, a current-state row, a suppression and a
/// logged check.
pub const DECISION_RECORD: &str = "core.consent.decision";
pub const CURRENT_RECORD: &str = "core.consent.current";
pub const SUPPRESSION_RECORD: &str = "core.consent.suppression";
pub const LOG_RECORD: &str = "core.consent.log";

/// One version of a consent decision.
#[derive(Clone, Debug)]
pub struct Decision {
    pub id: i64,
    pub company_id: i64,
    pub contact: Contact,
    pub purpose: String,
    pub channel: Channel,
    pub granted: bool,
    pub source: Source,
    pub captured_on: Capture,
    pub evidence: Option<String>,
    pub version: i32,
    pub recorded_at: jiff::Timestamp,
    /// The account that recorded it, or `None` for a system principal.
    pub recorded_by: Option<i64>,
    pub recorded_principal: Option<String>,
}

/// The latest decision for one person or address, purpose and channel.
#[derive(Clone, Debug)]
pub struct Current {
    pub id: i64,
    pub company_id: i64,
    pub contact: Contact,
    pub purpose: String,
    pub channel: Channel,
    pub granted: bool,
    pub source: Source,
    pub captured_on: Capture,
    pub version: i32,
    pub recorded_at: jiff::Timestamp,
}

/// An address or a person not contacted on a channel.
#[derive(Clone, Debug)]
pub struct Suppression {
    pub id: i64,
    pub company_id: i64,
    pub contact: Contact,
    pub channel: Channel,
    pub reason: Reason,
    pub evidence: Option<String>,
    pub suppressed_at: jiff::Timestamp,
    pub lifted_at: Option<jiff::Timestamp>,
    pub lifted_by: Option<i64>,
    pub lifted_principal: Option<String>,
    pub lift_reason: Option<String>,
}

/// A person's consent: the latest decision per purpose and channel, their
/// active person-level suppressions, and what applies with no decision.
#[derive(Clone, Debug)]
pub struct Standing {
    pub current: Vec<Current>,
    pub suppressions: Vec<Suppression>,
    pub default: NewAddress,
}

/// Active, lifted, or both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Active,
    Lifted,
    All,
}

/// A page of suppressions. `search` matches the address or the person's id;
/// `page` counts from 1.
#[derive(Clone, Debug)]
pub struct SuppressionFilter {
    pub search: String,
    pub channel: Option<Channel>,
    pub reason: Option<Reason>,
    pub state: State,
    pub page: i64,
    pub page_size: i64,
}
impl Default for SuppressionFilter {
    fn default() -> Self {
        Self {
            search: String::new(),
            channel: None,
            reason: None,
            state: State::Active,
            page: 1,
            page_size: 25,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SuppressionPage {
    pub entries: Vec<Suppression>,
    pub total: i64,
}

pub type WriteResult<T> = std::result::Result<T, WriteError>;
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("The person or suppression could not be found.")]
    NotFound,
    #[error("This suppression was already lifted.")]
    Lifted,
    #[error("Invalid input")]
    Invalid(Vec<FieldError>),
    #[error(transparent)]
    Access(#[from] Error),
}
fn invalid(errors: Vec<FieldError>) -> WriteResult<()> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(WriteError::Invalid(errors))
    }
}

/// The `core/consent` facade. Every call refreshes the scope and checks its
/// capability; no call takes a tenant from input.
pub struct Consent {
    db: Db,
    authentication: Arc<Authentication>,
    picklists: Arc<PickLists>,
    settings: Arc<Settings>,
}
impl Consent {
    async fn fresh(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
    ) -> Result<Scope> {
        self.authentication.reprove(client, scope).await
    }
    /// What applies to a contact with no decision for a purpose.
    async fn default(&self, client: &dyn ReadClient, scope: &Scope) -> Result<NewAddress> {
        let value = self
            .settings
            .get(client, scope, Target::Tenant, DEFAULT_NEW_ADDRESS)
            .await?
            .value;
        Ok(value
            .as_str()
            .and_then(NewAddress::parse)
            .unwrap_or(NewAddress::OptedOut))
    }
    /// The question with its address normalised.
    fn question(question: &Question, errors: &mut Vec<FieldError>) -> Question {
        if question.channel == Channel::Any {
            errors.push(model::field(
                "channel",
                "must be the channel the message is sent on",
            ));
        }
        Question {
            company_id: question.company_id,
            contact: model::contact(&question.contact, question.channel, true, errors),
            channel: question.channel,
            purpose: question.purpose.clone(),
        }
    }
    async fn purpose(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        purpose: &str,
        errors: &mut Vec<FieldError>,
    ) -> Result<()> {
        if !self
            .picklists
            .contains_choice(client, scope, PURPOSES, purpose, None)
            .await?
        {
            errors.push(model::field("purpose", "is not one of the purposes"));
        }
        Ok(())
    }

    /// May we contact this person or address, on this channel, for this
    /// purpose, now? Logs a fresh answer in the caller's transaction under
    /// correlation `key`. Senders call this immediately before every
    /// elective dispatch; delivery idempotency stays with the sender. A
    /// required business message does not call Consent. Needs
    /// `core.consent.check`.
    pub async fn check(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        question: &Question,
        key: &str,
        caller: &'static str,
    ) -> WriteResult<Answer> {
        let scope = self.fresh(tx.sql(), scope).await?;
        bilibi_authz::authorize_proof(tx.sql(), &scope, CHECK).await?;
        let key = key.trim();
        let mut errors = Vec::new();
        if key.is_empty() || key.len() > KEY_MAX {
            errors.push(model::field("key", "must be 1 to 200 bytes"));
        }
        if caller.is_empty() || caller.len() > 100 {
            errors.push(model::field("caller", "must be the caller's module id"));
        }
        let question = Self::question(question, &mut errors);
        invalid(errors)?;
        bilibi_company::authorize_work_target(tx.sql(), &scope, question.company_id, CHECK).await?;
        self.lock_purpose(tx, &scope, &question.purpose).await?;
        let default = self.default(tx.sql(), &scope).await?;
        Ok(store::log(tx, &scope, &question, key, caller, default).await?)
    }
    /// Locks the purpose `FOR SHARE` on the write's transaction, so a
    /// concurrent retirement waits for this write.
    async fn lock_purpose(
        &self,
        tx: &MutationTransaction<'_>,
        scope: &Scope,
        purpose: &str,
    ) -> WriteResult<()> {
        match self
            .picklists
            .check(tx, scope, PURPOSES, purpose, None)
            .await?
        {
            Ok(()) => Ok(()),
            Err(refusal) => Err(WriteError::Invalid(vec![model::field(
                "purpose",
                refusal.message(),
            )])),
        }
    }
    /// The answer `check` would give now, logging nothing. Needs
    /// `core.consent.view`.
    pub async fn preview(&self, scope: &Scope, question: &Question) -> WriteResult<Answer> {
        let client = self.db.get().await.map_err(Error::from)?;
        let scope = self.fresh(&**client, scope).await?;
        bilibi_authz::authorize_proof(&**client, &scope, VIEW).await?;
        let mut errors = Vec::new();
        let question = Self::question(question, &mut errors);
        self.purpose(&**client, &scope, &question.purpose, &mut errors)
            .await?;
        invalid(errors)?;
        bilibi_company::authorize_work_target(&**client, &scope, question.company_id, VIEW).await?;
        let default = self.default(&**client, &scope).await?;
        Ok(store::answer(&**client, &scope, &question, default).await?)
    }

    /// A person's consent matrix and active suppressions. Needs
    /// `core.consent.view` and the person (`NotFound` when the reader cannot
    /// see them).
    pub async fn for_person(
        &self,
        scope: &Scope,
        company_id: i64,
        person: PersonId,
    ) -> Result<Standing> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        bilibi_authz::authorize_proof(&**client, &scope, VIEW).await?;
        bilibi_company::authorize_work_target(&**client, &scope, company_id, VIEW).await?;
        bilibi_person::read_person(&**client, &scope, person).await?;
        Ok(Standing {
            current: store::current_for_person(&**client, &scope, company_id, person).await?,
            suppressions: store::active_for_person(&**client, &scope, company_id, person).await?,
            default: self.default(&**client, &scope).await?,
        })
    }
    /// Every version of a person's decisions, oldest first, optionally for
    /// one purpose or channel. Needs `core.consent.view`.
    pub async fn history(
        &self,
        scope: &Scope,
        company_id: i64,
        person: PersonId,
        purpose: Option<&str>,
        channel: Option<Channel>,
    ) -> Result<Vec<Decision>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        bilibi_authz::authorize_proof(&**client, &scope, VIEW).await?;
        bilibi_company::authorize_work_target(&**client, &scope, company_id, VIEW).await?;
        bilibi_person::read_person(&**client, &scope, person).await?;
        store::history(&**client, &scope, company_id, person, purpose, channel).await
    }
    /// The purposes a decision may be recorded for, in the reader's
    /// language: every active `core.consent.purposes` value.
    /// Needs `core.consent.view`.
    pub async fn purposes(&self, scope: &Scope) -> Result<Vec<bilibi_ui::Choice>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        bilibi_authz::authorize_proof(&**client, &scope, VIEW).await?;
        self.picklists
            .choices(&**client, &scope, PURPOSES, None)
            .await
    }
    /// Whether the reader holds `capability` now.
    pub async fn may(&self, scope: &Scope, capability: &'static str) -> Result<bool> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        bilibi_authz::holds_proof(&**client, &scope, capability).await
    }

    /// The checks `record` makes, with no write, on the client a plan hands
    /// its preview: the input with its address normalised and evidence
    /// trimmed.
    pub async fn preview_record(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        input: &DecisionInput,
    ) -> WriteResult<DecisionInput> {
        bilibi_authz::authorize_proof(client, scope, RECORD).await?;
        let mut errors = Vec::new();
        let contact = model::contact(&input.contact, input.channel, false, &mut errors);
        self.purpose(client, scope, &input.purpose, &mut errors)
            .await?;
        bilibi_company::authorize_work_target(client, scope, input.company_id, RECORD).await?;
        let evidence = model::evidence(input.evidence.as_deref()).unwrap_or_else(|e| {
            errors.push(e);
            None
        });
        invalid(errors)?;
        if let Some(person) = contact.person {
            match bilibi_person::read_person(client, scope, person).await {
                Ok(_) => {}
                Err(Error::NotFound) => return Err(WriteError::NotFound),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(DecisionInput {
            company_id: input.company_id,
            contact,
            evidence,
            ..input.clone()
        })
    }
    /// Records a decision as the next version for its person or address,
    /// purpose and channel, in the caller's transaction. Needs
    /// `core.consent.record`.
    pub async fn record(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        input: &DecisionInput,
    ) -> WriteResult<Decision> {
        let scope = self.fresh(tx.sql(), scope).await?;
        let input = self.preview_record(tx.sql(), &scope, input).await?;
        self.lock_purpose(tx, &scope, &input.purpose).await?;
        Ok(store::record(tx, &scope, &input).await?)
    }

    /// The checks `suppress` makes, with no write.
    pub async fn preview_suppress(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        input: &SuppressionInput,
    ) -> WriteResult<SuppressionInput> {
        bilibi_authz::authorize_proof(client, scope, CHECK).await?;
        let mut errors = Vec::new();
        if input.channel == Channel::Any {
            errors.push(model::field("channel", "must be one channel"));
        }
        let contact = model::contact(&input.contact, input.channel, false, &mut errors);
        bilibi_company::authorize_work_target(client, scope, input.company_id, CHECK).await?;
        let evidence = model::evidence(input.evidence.as_deref()).unwrap_or_else(|e| {
            errors.push(e);
            None
        });
        invalid(errors)?;
        Ok(SuppressionInput {
            company_id: input.company_id,
            contact,
            evidence,
            ..input.clone()
        })
    }
    /// Stops contact with an address or a person on a channel, in the
    /// caller's transaction. Idempotent: an active suppression with the same
    /// channel, contact and reason is returned unchanged. Needs
    /// `core.consent.check`.
    pub async fn suppress(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        input: &SuppressionInput,
    ) -> WriteResult<Suppression> {
        let scope = self.fresh(tx.sql(), scope).await?;
        let input = self.preview_suppress(tx.sql(), &scope, input).await?;
        Ok(store::suppress(tx, &scope, &input).await?.0)
    }

    /// The active suppression `lift` would lift, with no write.
    pub async fn preview_lift(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        reason: &str,
    ) -> WriteResult<Suppression> {
        self.liftable(client, scope, id, reason, false).await
    }
    async fn liftable(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        reason: &str,
        lock: bool,
    ) -> WriteResult<Suppression> {
        bilibi_authz::authorize_proof(client, scope, MANAGE).await?;
        let reason = reason.trim();
        if reason.is_empty() || reason.chars().count() > LIFT_REASON_MAX {
            return Err(WriteError::Invalid(vec![model::field(
                "reason",
                "must be 1 to 500 characters",
            )]));
        }
        let suppression = match store::suppression_by_id(client, scope, id, lock).await {
            Ok(suppression) => suppression,
            Err(Error::NotFound) => return Err(WriteError::NotFound),
            Err(error) => return Err(error.into()),
        };
        bilibi_company::authorize_work_target(client, scope, suppression.company_id, MANAGE)
            .await?;
        if suppression.lifted_at.is_some() {
            return Err(WriteError::Lifted);
        }
        Ok(suppression)
    }
    /// Lifts an active suppression with a reason, in the caller's
    /// transaction. Needs `core.consent.manage`.
    pub async fn lift(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        reason: &str,
    ) -> WriteResult<Suppression> {
        let scope = self.fresh(tx.sql(), scope).await?;
        let before = self.liftable(tx.sql(), &scope, id, reason, true).await?;
        Ok(store::lift(tx, &scope, before, reason.trim()).await?)
    }
    /// One suppression. Needs `core.consent.view`.
    pub async fn suppression(
        &self,
        scope: &Scope,
        company_id: i64,
        id: i64,
    ) -> Result<Suppression> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        bilibi_authz::authorize_proof(&**client, &scope, VIEW).await?;
        bilibi_company::authorize_work_target(&**client, &scope, company_id, VIEW).await?;
        let suppression = store::suppression_by_id(&**client, &scope, id, false).await?;
        if suppression.company_id != company_id {
            return Err(Error::NotFound);
        }
        Ok(suppression)
    }
    /// A page of suppressions, newest first. Needs `core.consent.view`.
    pub async fn suppressions(
        &self,
        scope: &Scope,
        company_id: i64,
        filter: &SuppressionFilter,
    ) -> Result<SuppressionPage> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        bilibi_authz::authorize_proof(&**client, &scope, VIEW).await?;
        bilibi_company::authorize_work_target(&**client, &scope, company_id, VIEW).await?;
        store::suppressions(&**client, &scope, company_id, filter).await
    }
}

inventory::submit! { Module {
    id: MODULE, layer: Layer::Core,
    dependencies: &["base/agent_api", "base/authz", "base/database", "base/datetime", "base/locale", "base/menu", "base/party_ids", "base/picklists", "base/principal_directory", "base/records", "base/settings", "base/tenancy", "base/ui", "core/company", "core/person", "core/user"],
    migrations: &[
        ("core-consent-001-decisions", include_str!("../migrations/001-decisions.sql")),
        ("core-consent-002-suppressions", include_str!("../migrations/002-suppressions.sql")),
        ("core-consent-003-log", include_str!("../migrations/003-log.sql")),
    ],
    services: &[Service { id: SERVICE, factory: |s| {
        let consent = Arc::new(Consent {
            db: s.get::<Db>("base/database.pool")?.as_ref().clone(),
            authentication: s.get("base/tenancy.authentication")?,
            picklists: s.get(bilibi_picklists::SERVICE)?,
            settings: s.get("base/settings.store")?,
        });
        agent_operations::bind(consent.clone())?;
        Ok(consent)
    }}, Service { id: web::SERVICE, factory: |s| {
        Ok(Arc::new(bilibi_ui::page::PageService::<Scope, Error>(Arc::new(web::Pages {
            consent: s.get(SERVICE)?,
            records: s.get(bilibi_records::SERVICE)?,
            contracts: s.get(bilibi_agent_api::page::SERVICE)?,
            directory: s.get("base/principal_directory.directory")?,
            display: s.get("base/datetime.display")?,
            locales: s.get("base/locale.locales")?,
            db: s.get::<Db>("base/database.pool")?.as_ref().clone(),
        }))))
    }}],
} }
