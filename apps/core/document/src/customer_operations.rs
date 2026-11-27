//! Operation contracts for sharing documents with portal customers (issue
//! 524). The record page calls the same write contracts. Recipients are
//! `{kind, id}` records of kind `person` or `company`, as `base.records.search`
//! returns them.
use crate::agent_operations::{invalid, owner};
use crate::{CustomerShare, CustomerShareInput, Recipient, Reference, MANAGE, VIEW};
use bilibi_agent_api::{
    Channels, Definitions, Handler, Operation, OperationClient, OperationFuture, ReadHandler,
    WriteHandler, DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_records::Resolved;
use bilibi_tenancy::Scope;
use jiff::civil::Date;
use serde_json::{json, Value};

fn id(input: &Value) -> bilibi_agent_api::Result<i64> {
    Ok(input["id"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or(Error::Invalid)?)
}

fn share_input(input: &Value) -> bilibi_agent_api::Result<CustomerShareInput> {
    let reference = Reference::new(
        input["recipient"]["kind"].as_str().unwrap_or_default(),
        input["recipient"]["id"].as_str().unwrap_or_default(),
    );
    let Some(recipient) = Recipient::from_reference(&reference) else {
        return Err(bilibi_agent_api::invalid(
            "recipient",
            "must be a person or company record, such as one base.records.search returns",
        ));
    };
    let until = match input["until"].as_str().map(str::trim) {
        None | Some("") => None,
        Some(day) => Some(day.parse::<Date>().map_err(|_| {
            bilibi_agent_api::invalid("until", "must be a date, such as 2026-11-02")
        })?),
    };
    Ok(CustomerShareInput { recipient, until })
}

fn share_record(share: &CustomerShare, named: Option<&Resolved>) -> Value {
    let reference = share.recipient.reference();
    let mut recipient = json!({"kind": reference.kind, "id": reference.id});
    if let Some(named) = named {
        recipient["label"] = json!(named.label);
        recipient["path"] = json!(named.path);
    }
    json!({
        "id": share.id,
        "recipient": recipient,
        "expires_at": share.expires_at.map(|t| t.to_string()),
        "shared_by": share.shared_by,
        "shared_at": share.shared_at.to_string(),
    })
}

struct Shares;
impl ReadHandler for Shares {
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
            let owner = owner()?;
            let shares = owner
                .customer_shares(scope, id(input)?)
                .await
                .map_err(|e| invalid(e, "", "id"))?;
            let references: Vec<Reference> =
                shares.iter().map(|s| s.recipient.reference()).collect();
            let named = owner.records().resolve(scope, &references).await?;
            Ok(json!({"shares": shares
                .iter()
                .zip(&named)
                .map(|(share, named)| share_record(share, Some(named)))
                .collect::<Vec<_>>()})
            .into())
        })
    }
}

struct Share;
impl WriteHandler for Share {
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
            let share = owner()?
                .preview_share_customer(client, scope, id(input)?, &share_input(input)?)
                .await
                .map_err(|e| invalid(e, "", "recipient"))?;
            Ok(json!({"shares": share_record(&share, None)}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let share = owner()?
                .share_customer(tx, scope, id(input)?, &share_input(input)?)
                .await
                .map_err(|e| invalid(e, "", "recipient"))?;
            Ok(share_record(&share, None).into())
        })
    }
}

struct Unshare;
impl WriteHandler for Unshare {
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
            let share = owner()?
                .preview_unshare_customer(
                    client,
                    scope,
                    id(input)?,
                    input["share"].as_i64().unwrap_or_default(),
                )
                .await
                .map_err(|e| invalid(e, "", "share"))?;
            Ok(json!({"revokes": share_record(&share, None)}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            owner()?
                .unshare_customer(
                    tx,
                    scope,
                    id(input)?,
                    input["share"].as_i64().unwrap_or_default(),
                )
                .await
                .map_err(|e| invalid(e, "", "share"))?;
            Ok(json!({"revoked": true}).into())
        })
    }
}

fn get_schema() -> Value {
    json!({"type":"object","required":["id"],"properties":{"id":{"type":"integer","minimum":1}}})
}
fn share_schema() -> Value {
    json!({"type":"object","required":["id","recipient"],"properties":{
        "id":{"type":"integer","minimum":1,"description":"A document that keeps a file."},
        "recipient":{"type":"object","required":["kind","id"],"description":"The customer: a person record, or a company record for every business customer who signs in for it, from base.records.search.","properties":{
            "kind":{"type":"string","enum":["person","company"]},
            "id":{"type":"string","maxLength":64}
        }},
        "until":{"type":"string","format":"date","description":"The last day (UTC) the customer sees the document; it does not end when omitted."}
    }})
}
fn unshare_schema() -> Value {
    json!({"type":"object","required":["id","share"],"properties":{
        "id":{"type":"integer","minimum":1},
        "share":{"type":"integer","minimum":1,"description":"The share ID returned by core.document.customer_shares."}
    }})
}
fn review_output() -> Value {
    json!({"type":"object","description":"The review shows what will change; the commit returns the result."})
}

static SHARES: Shares = Shares;
static SHARE: Share = Share;
static UNSHARE: Unshare = Unshare;
const KEYWORDS: &[&str] = &["document", "file", "customer", "portal", "share"];

inventory::submit! { Contribution::new(DEFINITIONS, "core/document.customer_operations", Provenance { module: crate::MODULE, source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key: "core.document.customer_shares", title: "List a document's customer shares", summary: "The portal customers one document is shared with, ended shares too. Needs sharing permission on that document.", keywords: KEYWORDS, capability: VIEW, input: get_schema, output: review_output, handler: Handler::Read(&SHARES), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.document.customer_share", title: "Share a document with a customer", summary: "Let a portal customer (a person, or every business customer of a company) see and download a document's latest file.", keywords: KEYWORDS, capability: MANAGE, input: share_schema, output: review_output, handler: Handler::SafeWrite(&SHARE), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.document.customer_unshare", title: "Stop sharing a document with a customer", summary: "Revoke one customer share; the customer no longer sees the document.", keywords: KEYWORDS, capability: MANAGE, input: unshare_schema, output: review_output, handler: Handler::SafeWrite(&UNSHARE), channels: Channels::Every, step_up: &[] },
    ],
    guides: &[],
}) }
