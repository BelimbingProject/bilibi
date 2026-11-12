//! Operation contracts for consent. The person panel and the suppressions
//! page call the write contracts too, so every door shares one rule set.
use crate::{
    Answer, Capture, Channel, Consent, Contact, Current, Decision, DecisionInput, Question, Reason,
    Source, Suppression, SuppressionInput, WriteError, CHECK, MANAGE, RECORD, VIEW,
};
use bilibi_agent_api::{
    Channels, Definitions, Guide, Handler, Operation, OperationClient, OperationFuture,
    ReadHandler, WriteHandler, DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_party_ids::PersonId;
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

static OWNER: OnceLock<Arc<Consent>> = OnceLock::new();
pub(crate) fn bind(owner: Arc<Consent>) -> bilibi_module_registry::Result<()> {
    OWNER.set(owner).map_err(|_| {
        bilibi_module_registry::RegistryError::Service(
            "duplicate consent operations binding".into(),
        )
    })
}
fn owner() -> bilibi_agent_api::Result<&'static Consent> {
    OWNER.get().map(Arc::as_ref).ok_or(Error::Internal.into())
}

type Fields = BTreeMap<String, Vec<String>>;

/// Reads the typed input. A value the schema accepts but the rules refuse
/// (an unknown code, a bad time) is a field error on its path.
struct Reader<'a> {
    input: &'a Value,
    fields: Fields,
}
impl<'a> Reader<'a> {
    fn new(input: &'a Value) -> Self {
        Self {
            input,
            fields: Fields::new(),
        }
    }
    fn refuse(&mut self, key: &str, message: &str) {
        self.fields
            .entry(key.into())
            .or_default()
            .push(message.into());
    }
    fn text(&self, key: &str) -> Option<String> {
        self.input[key].as_str().map(str::to_owned)
    }
    fn code<T>(&mut self, key: &str, parse: fn(&str) -> Option<T>) -> Option<T> {
        let parsed = self.text(key).and_then(|v| parse(&v));
        if parsed.is_none() && self.input.get(key).is_some() {
            self.refuse(key, "is not one of the allowed values");
        }
        parsed
    }
    fn required<T>(&mut self, key: &str, parse: fn(&str) -> Option<T>) -> Option<T> {
        let parsed = self.code(key, parse);
        if self.input.get(key).is_none() {
            self.refuse(key, "is required");
        }
        parsed
    }
    fn person(&mut self, key: &str) -> Option<PersonId> {
        let id = self.input[key].as_i64()?;
        let person = PersonId::new(id).ok();
        if person.is_none() {
            self.refuse(key, "must be positive");
        }
        person
    }
    fn contact(&mut self) -> Contact {
        Contact {
            person: self.person("person_id"),
            address: self.text("address").filter(|a| !a.trim().is_empty()),
        }
    }
    fn done<T>(self, value: T) -> bilibi_agent_api::Result<T> {
        if self.fields.is_empty() {
            Ok(value)
        } else {
            Err(bilibi_agent_api::Error::InvalidInput(self.fields))
        }
    }
}

fn current_company(scope: &Scope) -> bilibi_agent_api::Result<i64> {
    scope
        .current_company()
        .map(|company| company.get())
        .ok_or_else(|| bilibi_agent_api::invalid("company", "requires a selected company"))
}
fn question(scope: &Scope, input: &Value) -> bilibi_agent_api::Result<Question> {
    let mut reader = Reader::new(input);
    let contact = reader.contact();
    let channel = reader.required("channel", Channel::parse);
    let purpose = reader.text("purpose").unwrap_or_default();
    let Some(channel) = channel else {
        return Err(bilibi_agent_api::Error::InvalidInput(reader.fields));
    };
    reader.done(Question {
        company_id: current_company(scope)?,
        contact,
        channel,
        purpose,
    })
}
/// The decision an operation records. Its `source` is the door it came
/// through, never input: `api` for an agent's bearer request, `agent`
/// (staff) for a signed-in person on a page.
fn decision_input(scope: &Scope, input: &Value) -> bilibi_agent_api::Result<DecisionInput> {
    let mut reader = Reader::new(input);
    let contact = reader.contact();
    let channel = reader.required("channel", Channel::parse);
    let captured_on = reader.required("captured_on", Capture::parse);
    let granted = input["granted"].as_bool();
    let (Some(channel), Some(captured_on), Some(granted)) = (channel, captured_on, granted) else {
        if granted.is_none() {
            reader.refuse("granted", "is required");
        }
        return Err(bilibi_agent_api::Error::InvalidInput(reader.fields));
    };
    let decision = DecisionInput {
        company_id: current_company(scope)?,
        contact,
        purpose: reader.text("purpose").unwrap_or_default(),
        channel,
        granted,
        source: if scope.agent().is_some() {
            Source::Api
        } else {
            Source::Agent
        },
        captured_on,
        evidence: reader.text("evidence"),
    };
    reader.done(decision)
}
fn suppression_input(scope: &Scope, input: &Value) -> bilibi_agent_api::Result<SuppressionInput> {
    let mut reader = Reader::new(input);
    let contact = reader.contact();
    let channel = reader.required("channel", Channel::parse);
    let reason = reader.required("reason", Reason::parse);
    let (Some(channel), Some(reason)) = (channel, reason) else {
        return Err(bilibi_agent_api::Error::InvalidInput(reader.fields));
    };
    let suppression = SuppressionInput {
        company_id: current_company(scope)?,
        contact,
        channel,
        reason,
        evidence: reader.text("evidence"),
    };
    reader.done(suppression)
}
fn person_id(input: &Value) -> bilibi_agent_api::Result<PersonId> {
    input["person_id"]
        .as_i64()
        .and_then(|id| PersonId::new(id).ok())
        .ok_or_else(|| bilibi_agent_api::invalid("person_id", "must be positive"))
}
fn lift_input(input: &Value) -> bilibi_agent_api::Result<(i64, String)> {
    let id = input["suppression_id"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or_else(|| bilibi_agent_api::invalid("suppression_id", "must be positive"))?;
    Ok((id, bilibi_agent_api::text_field(input, "reason")))
}

/// An owner refusal, typed for every door.
pub(crate) fn refused(error: WriteError) -> bilibi_agent_api::Error {
    match error {
        WriteError::Access(error) => error.into(),
        WriteError::NotFound => bilibi_agent_api::Error::NotFound,
        WriteError::Lifted => bilibi_agent_api::invalid("lifted", WriteError::Lifted.to_string()),
        WriteError::CompanyArchived => {
            bilibi_agent_api::invalid("company_id", WriteError::CompanyArchived.to_string())
        }
        WriteError::Invalid(errors) => {
            let mut fields = Fields::new();
            for e in errors {
                fields.entry(e.field.into()).or_default().push(e.message);
            }
            bilibi_agent_api::Error::InvalidInput(fields)
        }
    }
}

fn contact_json(contact: &Contact) -> Value {
    json!({"person_id": contact.person.map(|p| p.get()), "address": contact.address})
}
fn answer_json(answer: Answer) -> Value {
    match answer {
        Answer::Allowed => json!({"outcome": "allowed", "allowed": true}),
        Answer::Refused(refusal) => {
            json!({"outcome": refusal.code(), "allowed": false, "reason": refusal.code()})
        }
    }
}
fn current_json(c: &Current) -> Value {
    json!({
        "purpose": c.purpose,
        "channel": c.channel.code(),
        "granted": c.granted,
        "source": c.source.code(),
        "captured_on": c.captured_on.code(),
        "version": c.version,
        "recorded_at": c.recorded_at.to_string(),
    })
}
pub(crate) fn decision_json(d: &Decision) -> Value {
    let mut value = contact_json(&d.contact);
    value["id"] = json!(d.id);
    value["purpose"] = json!(d.purpose);
    value["channel"] = json!(d.channel.code());
    value["granted"] = json!(d.granted);
    value["source"] = json!(d.source.code());
    value["captured_on"] = json!(d.captured_on.code());
    value["evidence"] = json!(d.evidence);
    value["version"] = json!(d.version);
    value["recorded_at"] = json!(d.recorded_at.to_string());
    value["recorded_by"] = json!(d.recorded_by);
    value["recorded_principal"] = json!(d.recorded_principal);
    value
}
fn draft_json(d: &DecisionInput) -> Value {
    let mut value = contact_json(&d.contact);
    value["purpose"] = json!(d.purpose);
    value["channel"] = json!(d.channel.code());
    value["granted"] = json!(d.granted);
    value["source"] = json!(d.source.code());
    value["captured_on"] = json!(d.captured_on.code());
    value["evidence"] = json!(d.evidence);
    value
}
pub(crate) fn suppression_json(s: &Suppression) -> Value {
    let mut value = contact_json(&s.contact);
    value["id"] = json!(s.id);
    value["channel"] = json!(s.channel.code());
    value["reason"] = json!(s.reason.code());
    value["evidence"] = json!(s.evidence);
    value["suppressed_at"] = json!(s.suppressed_at.to_string());
    value["lifted_at"] = json!(s.lifted_at.map(|t| t.to_string()));
    value["lifted_by"] = json!(s.lifted_by);
    value["lift_reason"] = json!(s.lift_reason);
    value
}

struct ForPerson;
impl ReadHandler for ForPerson {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn call<'a>(
        &'a self,
        _: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let person = person_id(input)?;
            let company = current_company(scope)?;
            let standing = owner()?.for_person(scope, company, person).await?;
            Ok(json!({
                "person_id": person.get(),
                "default_new_address": match standing.default {
                    crate::NewAddress::OptedOut => "opted_out",
                    crate::NewAddress::OptedIn => "opted_in",
                },
                "decisions": standing.current.iter().map(current_json).collect::<Vec<_>>(),
                "suppressions": standing.suppressions.iter().map(suppression_json).collect::<Vec<_>>(),
            })
            .into())
        })
    }
}
struct History;
impl ReadHandler for History {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn call<'a>(
        &'a self,
        _: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let person = person_id(input)?;
            let mut reader = Reader::new(input);
            let channel = reader.code("channel", Channel::parse);
            let purpose = reader.text("purpose");
            reader.done(())?;
            let versions = owner()?
                .history(
                    scope,
                    current_company(scope)?,
                    person,
                    purpose.as_deref(),
                    channel,
                )
                .await?;
            Ok(json!({"versions": versions.iter().map(decision_json).collect::<Vec<_>>()}).into())
        })
    }
}
struct Preview;
impl ReadHandler for Preview {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn call<'a>(
        &'a self,
        _: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let question = question(scope, input)?;
            let answer = owner()?.preview(scope, &question).await.map_err(refused)?;
            Ok(answer_json(answer).into())
        })
    }
}
struct Record;
impl WriteHandler for Record {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let input = decision_input(scope, input)?;
            let checked = owner()?
                .preview_record(client, scope, &input)
                .await
                .map_err(refused)?;
            Ok(json!({"records": draft_json(&checked)}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let input = decision_input(scope, input)?;
            let decision = owner()?.record(tx, scope, &input).await.map_err(refused)?;
            Ok(decision_json(&decision).into())
        })
    }
}
struct Suppress;
impl WriteHandler for Suppress {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let input = suppression_input(scope, input)?;
            let checked = owner()?
                .preview_suppress(client, scope, &input)
                .await
                .map_err(refused)?;
            let mut value = contact_json(&checked.contact);
            value["channel"] = json!(checked.channel.code());
            value["reason"] = json!(checked.reason.code());
            value["evidence"] = json!(checked.evidence);
            Ok(json!({"suppresses": value}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let input = suppression_input(scope, input)?;
            let suppression = owner()?
                .suppress(tx, scope, &input)
                .await
                .map_err(refused)?;
            Ok(suppression_json(&suppression).into())
        })
    }
}
struct Lift;
impl WriteHandler for Lift {
    fn owner(&self) -> &'static str {
        crate::MODULE
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, reason) = lift_input(input)?;
            let suppression = owner()?
                .preview_lift(client, scope, id, &reason)
                .await
                .map_err(refused)?;
            Ok(json!({"lifts": suppression_json(&suppression), "reason": reason.trim()}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, reason) = lift_input(input)?;
            let suppression = owner()?
                .lift(tx, scope, id, &reason)
                .await
                .map_err(refused)?;
            Ok(suppression_json(&suppression).into())
        })
    }
}

fn codes(all: &[&'static str]) -> Value {
    json!(all)
}
fn channels(any: bool) -> Value {
    let codes: Vec<&str> = Channel::ALL
        .iter()
        .filter(|c| any || **c != Channel::Any)
        .map(|c| c.code())
        .collect();
    json!(codes)
}
fn contact_properties(properties: &mut serde_json::Map<String, Value>) {
    properties.insert(
        "person_id".into(),
        json!({"type":"integer","minimum":1,"description":"A Core person. Give this or address."}),
    );
    properties.insert("address".into(), json!({"type":"string","maxLength":254,"description":"An email address, a phone number with its country code (starting with + or 00) or another handle, normalised before use. Give this or person_id."}));
}
fn person_schema() -> Value {
    json!({"type":"object","required":["person_id"],"properties":{"person_id":{"type":"integer","minimum":1}}})
}
fn history_schema() -> Value {
    json!({"type":"object","required":["person_id"],"properties":{
        "person_id":{"type":"integer","minimum":1},
        "purpose":{"type":"string","maxLength":64,"description":"Only this purpose's versions."},
        "channel":{"type":"string","enum":channels(true),"description":"Only this channel's versions."}
    }})
}
fn preview_schema() -> Value {
    let mut properties = serde_json::Map::new();
    contact_properties(&mut properties);
    properties.insert("channel".into(), json!({"type":"string","enum":channels(false),"description":"The channel the message would be sent on."}));
    properties.insert("purpose".into(), json!({"type":"string","maxLength":64,"description":"An elective core.consent.purposes code. Required business messages are outside Consent."}));
    json!({"type":"object","required":["channel","purpose"],"properties":properties})
}
fn record_schema() -> Value {
    let mut properties = serde_json::Map::new();
    contact_properties(&mut properties);
    properties.insert("purpose".into(), json!({"type":"string","maxLength":64,"description":"An elective core.consent.purposes code."}));
    properties.insert("channel".into(), json!({"type":"string","enum":channels(true),"description":"any applies to every channel without a decision of its own."}));
    properties.insert(
        "granted".into(),
        json!({"type":"boolean","description":"true grants consent, false withdraws it."}),
    );
    properties.insert("captured_on".into(), json!({"type":"string","enum":codes(&Capture::ALL.iter().map(|c| c.code()).collect::<Vec<_>>()),"description":"The channel it was captured on."}));
    properties.insert("evidence".into(), json!({"type":"string","maxLength":2000,"description":"A proof reference or the wording shown."}));
    json!({"type":"object","required":["purpose","channel","granted","captured_on"],"properties":properties})
}
fn suppress_schema() -> Value {
    let mut properties = serde_json::Map::new();
    contact_properties(&mut properties);
    properties.insert(
        "channel".into(),
        json!({"type":"string","enum":channels(false)}),
    );
    properties.insert("reason".into(), json!({"type":"string","enum":codes(&Reason::ALL.iter().map(|r| r.code()).collect::<Vec<_>>()),"description":"Why elective contact is suppressed for this sending company."}));
    properties.insert("evidence".into(), json!({"type":"string","maxLength":2000,"description":"The bounce or complaint report reference, or the link used."}));
    json!({"type":"object","required":["channel","reason"],"properties":properties})
}
fn lift_schema() -> Value {
    json!({"type":"object","required":["suppression_id","reason"],"properties":{
        "suppression_id":{"type":"integer","minimum":1},
        "reason":{"type":"string","maxLength":500,"description":"Why contact may resume."}
    }})
}
fn standing_output() -> Value {
    json!({"type":"object","description":"person_id, default_new_address (opted_out or opted_in), decisions (the latest per purpose and channel: purpose, channel, granted, source, captured_on, version, recorded_at) and the person's active suppressions."})
}
fn history_output() -> Value {
    json!({"type":"object","description":"versions, oldest first: id, person_id, address, purpose, channel, granted, source, captured_on, evidence, version, recorded_at, recorded_by and recorded_principal."})
}
fn answer_output() -> Value {
    json!({"type":"object","description":"outcome (allowed, no_consent, withdrawn or suppressed), allowed, and reason when refused."})
}
fn review_output() -> Value {
    json!({"type":"object","description":"The review shows what will be stored; the commit returns the stored decision or suppression."})
}

static FOR_PERSON: ForPerson = ForPerson;
static HISTORY: History = History;
static PREVIEW: Preview = Preview;
static RECORD_OP: Record = Record;
static SUPPRESS: Suppress = Suppress;
static LIFT: Lift = Lift;
const KEYWORDS: &[&str] = &[
    "consent",
    "opt-out",
    "unsubscribe",
    "suppression",
    "preferences",
    "marketing",
];
inventory::submit! { Contribution::new(DEFINITIONS, "core/consent.operations", Provenance { module: crate::MODULE, source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key: "core.consent.for_person", title: "Read a person's consent", summary: "The latest decision per purpose and channel, the default for no decision, and the person's active suppressions.", keywords: KEYWORDS, capability: VIEW, input: person_schema, output: standing_output, handler: Handler::Read(&FOR_PERSON), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.consent.history", title: "Read consent history", summary: "Every version of a person's decisions, oldest first, with source, capture channel and evidence.", keywords: KEYWORDS, capability: VIEW, input: history_schema, output: history_output, handler: Handler::Read(&HISTORY), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.consent.preview", title: "Preview a consent check", summary: "Whether a message to a person or address, on a channel, for a purpose, would be allowed now. Logs nothing.", keywords: KEYWORDS, capability: VIEW, input: preview_schema, output: answer_output, handler: Handler::Read(&PREVIEW), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.consent.record", title: "Record consent", summary: "Grant or withdraw consent for a purpose on a channel, with where it was captured and the evidence. A new version with source api; history is kept.", keywords: KEYWORDS, capability: RECORD, input: record_schema, output: review_output, handler: Handler::SafeWrite(&RECORD_OP), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.consent.suppress", title: "Suppress an address", summary: "Stop contact with an address or a person on a channel after a hard bounce, a complaint or an unsubscribe. Idempotent.", keywords: KEYWORDS, capability: CHECK, input: suppress_schema, output: review_output, handler: Handler::SafeWrite(&SUPPRESS), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.consent.lift", title: "Lift a suppression", summary: "Allow contact again on an address or a person's channel, with a reason.", keywords: KEYWORDS, capability: MANAGE, input: lift_schema, output: review_output, handler: Handler::SafeWrite(&LIFT), channels: Channels::Every, step_up: &[] },
    ],
    guides: &[Guide { key: "core.consent", title: "Consent", summary: "Consent decisions, opt-outs and the check every sender makes before a message.", keywords: KEYWORDS, capability: VIEW, body: include_str!("../guide.md") }],
}) }
