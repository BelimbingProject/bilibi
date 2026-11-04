//! Operation contracts for documents. The pages and panels call the write
//! contracts too, so every door shares one rule set. Records are
//! `{kind, id}` in input; reads add `label` and `path`. Files come only from
//! the pages' uploads (`store_file`): an agent names a file by its
//! `artifact_id` or keeps an external document's link.
use crate::{
    AccessGrant, Document, Documents, Draft, Filter, Link, Patch, PlannedRevision, Reference,
    Revision, RevisionInput, Sort, Source, WriteError, MANAGE, VIEW,
};
use bilibi_agent_api::{
    Channels, Definitions, Guide, Handler, Operation, OperationClient, OperationFuture,
    ReadHandler, WriteHandler, DEFINITIONS,
};
use bilibi_database::{Error, MutationTransaction};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_records::Resolved;
use bilibi_tenancy::Scope;
use jiff::civil::Date;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

static OWNER: OnceLock<Arc<Documents>> = OnceLock::new();
pub(crate) fn bind(owner: Arc<Documents>) -> bilibi_module_registry::Result<()> {
    OWNER.set(owner).map_err(|_| {
        bilibi_module_registry::RegistryError::Service(
            "duplicate document operations binding".into(),
        )
    })
}
fn owner() -> bilibi_agent_api::Result<&'static Documents> {
    OWNER.get().map(Arc::as_ref).ok_or(Error::Internal.into())
}

type Fields = BTreeMap<String, Vec<String>>;
fn refuse(fields: &mut Fields, path: &str, message: impl Into<String>) {
    fields.entry(path.into()).or_default().push(message.into());
}

/// Reads the typed input under `prefix`. A value the schema accepts but the
/// rules refuse (a malformed date) is a field error on its path.
struct Reader<'a> {
    input: &'a Value,
    prefix: &'static str,
    fields: Fields,
}
impl<'a> Reader<'a> {
    fn new(input: &'a Value, prefix: &'static str) -> Self {
        Self {
            input,
            prefix,
            fields: Fields::new(),
        }
    }
    fn at(&self) -> &'a Value {
        if self.prefix.is_empty() {
            self.input
        } else {
            &self.input[self.prefix]
        }
    }
    fn refuse(&mut self, key: &str, message: &str) {
        let path = if self.prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{}.{key}", self.prefix)
        };
        refuse(&mut self.fields, &path, message);
    }
    fn text(&self, key: &str) -> Option<String> {
        self.at()[key].as_str().map(str::to_owned)
    }
    /// `None` when omitted; `Some(None)` for an empty string.
    fn clearable_text(&self, key: &str) -> Option<Option<String>> {
        self.text(key).map(|v| (!v.trim().is_empty()).then_some(v))
    }
    /// `None` when omitted; `Some(None)` for an empty string.
    fn date(&mut self, key: &str) -> Option<Option<Date>> {
        let value = self.text(key)?;
        if value.trim().is_empty() {
            return Some(None);
        }
        match value.trim().parse::<Date>() {
            Ok(date) => Some(Some(date)),
            Err(_) => {
                self.refuse(key, "must be a date, such as 2026-11-02");
                None
            }
        }
    }
    fn reference(&self, value: &Value) -> Reference {
        Reference::new(
            value["kind"].as_str().unwrap_or_default(),
            value["id"].as_str().unwrap_or_default(),
        )
    }
    fn done<T>(self, value: T) -> bilibi_agent_api::Result<T> {
        if self.fields.is_empty() {
            Ok(value)
        } else {
            Err(bilibi_agent_api::Error::InvalidInput(self.fields))
        }
    }
}

fn draft(input: &Value) -> bilibi_agent_api::Result<Draft> {
    let mut reader = Reader::new(input, "");
    let source = match reader.text("source").as_deref() {
        None | Some("internal") => Source::Internal,
        Some(_) => Source::External,
    };
    let related = input["related"]
        .as_array()
        .map(|items| items.iter().map(|r| reader.reference(r)).collect())
        .unwrap_or_default();
    let draft = Draft {
        name: reader.text("name"),
        publish_date: reader.date("publish_date").flatten(),
        source,
        artifact_id: reader.text("artifact_id"),
        external_url: reader.text("external_url"),
        category: reader.text("category"),
        sub_category: reader.text("sub_category"),
        document_type: reader.text("document_type"),
        status: reader.text("status"),
        change_log: reader.text("change_log"),
        related,
    };
    reader.done(draft)
}

fn patch(input: &Value) -> bilibi_agent_api::Result<Patch> {
    let mut reader = Reader::new(input, "patch");
    let patch = Patch {
        name: reader.text("name"),
        publish_date: reader.date("publish_date").flatten(),
        category: reader.clearable_text("category"),
        sub_category: reader.clearable_text("sub_category"),
        document_type: reader.clearable_text("document_type"),
        status: reader.text("status"),
    };
    if reader
        .text("publish_date")
        .is_some_and(|v| v.trim().is_empty())
    {
        reader.refuse("publish_date", "can't be blank");
    }
    reader.done(patch)
}

fn revision_input(input: &Value) -> RevisionInput {
    RevisionInput {
        artifact_id: input["artifact_id"].as_str().map(str::to_owned),
        external_url: input["external_url"].as_str().map(str::to_owned),
        revision: input["revision"]
            .as_i64()
            .and_then(|r| i32::try_from(r).ok()),
        change_log: input["change_log"].as_str().unwrap_or_default().to_owned(),
    }
}

fn id(input: &Value) -> bilibi_agent_api::Result<i64> {
    Ok(input["id"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or(Error::Invalid)?)
}
fn version(input: &Value) -> bilibi_agent_api::Result<i64> {
    Ok(input["version"]
        .as_i64()
        .filter(|v| *v > 0)
        .ok_or(Error::Invalid)?)
}
fn record_input(input: &Value) -> Reference {
    Reference::new(
        input["record"]["kind"].as_str().unwrap_or_default(),
        input["record"]["id"].as_str().unwrap_or_default(),
    )
}

/// An owner refusal, typed for every door. Field errors keep their input
/// path (`patch.` for an update); a refused record is a field error on
/// `field`.
pub(crate) fn invalid(error: WriteError, prefix: &str, field: &str) -> bilibi_agent_api::Error {
    let one = |key: &str, message: String| {
        bilibi_agent_api::Error::InvalidInput(BTreeMap::from([(key.into(), vec![message])]))
    };
    match error {
        WriteError::Access(error) => error.into(),
        WriteError::NotFound => bilibi_agent_api::Error::NotFound,
        WriteError::Forbidden => bilibi_agent_api::Error::Forbidden,
        WriteError::Stale => Error::Conflict.into(),
        WriteError::RelatedRefused(message) => one(field, message.into()),
        error @ (WriteError::TooLarge { .. }
        | WriteError::Quarantined
        | WriteError::Retained
        | WriteError::LegalHold) => one("file", error.to_string()),
        WriteError::Invalid(errors) => {
            let mut fields = Fields::new();
            for e in errors {
                let path = if prefix.is_empty() || e.field == "company" {
                    e.field.to_owned()
                } else {
                    format!("{prefix}.{}", e.field)
                };
                refuse(&mut fields, &path, e.message);
            }
            bilibi_agent_api::Error::InvalidInput(fields)
        }
    }
}

fn reference(reference: &Reference, resolved: Option<&Resolved>) -> Value {
    let mut value = json!({"kind": reference.kind, "id": reference.id});
    if let Some(resolved) = resolved {
        value["label"] = json!(resolved.label);
        value["path"] = json!(resolved.path);
    }
    value
}
pub(crate) fn record(d: &Document) -> Value {
    json!({
        "id": d.id,
        "name": d.name,
        "publish_date": d.publish_date.to_string(),
        "source": d.source.code(),
        "external_url": d.external_url,
        "category": d.category,
        "sub_category": d.sub_category,
        "document_type": d.document_type,
        "status": d.status,
        "revision": d.revision,
        "last_revision_at": d.last_revision_at.to_string(),
        "company_id": d.company_id,
        "created_by": d.created_by,
        "created_at": d.created_at.to_string(),
        "updated_at": d.updated_at.to_string(),
        "version": d.version,
        "path": crate::web::path(d.id),
    })
}
fn revision_record(document: i64, r: &Revision) -> Value {
    json!({
        "revision": r.revision,
        "name": r.name,
        "file_name": r.file_name,
        "content_type": r.content_type,
        "byte_size": r.byte_size,
        "sha256": r.sha256,
        "external_url": r.external_url,
        "change_log": r.change_log,
        "scan_status": r.scan_status.code(),
        "created_by": r.created_by,
        "created_at": r.created_at.to_string(),
        "download_path": r.file_name.as_ref().map(|_| crate::web::file_path(document, r.revision)),
    })
}
fn draft_record(d: &Draft) -> Value {
    json!({
        "name": d.name,
        "publish_date": d.publish_date.map_or(json!("today"), |d| json!(d.to_string())),
        "source": d.source.code(),
        "artifact_id": d.artifact_id,
        "external_url": d.external_url,
        "category": d.category,
        "sub_category": d.sub_category,
        "document_type": d.document_type,
        "status": d.status.as_ref().map_or(json!("the list's default"), |s| json!(s)),
        "change_log": d.change_log,
        "related": d.related.iter().map(|r| reference(r, None)).collect::<Vec<_>>(),
    })
}
fn planned_record(p: &PlannedRevision) -> Value {
    json!({
        "revision": p.revision,
        "file_name": p.file_name,
        "external_url": p.external_url,
        "change_log": p.change_log,
    })
}
fn link_record(link: &Link) -> Value {
    json!({"record": reference(&link.record, None), "linked_by": link.linked_by, "linked_at": link.linked_at.to_string()})
}
fn access_record(grant: &AccessGrant) -> Value {
    json!({
        "id": grant.id,
        "principal_type": grant.principal_type,
        "principal_id": grant.principal_id,
        "view": grant.can_view,
        "download": grant.can_download,
        "manage": grant.can_manage,
        "share": grant.can_share,
    })
}

struct List;
impl ReadHandler for List {
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
            let reader = Reader::new(input, "");
            let sort = match reader.text("sort").as_deref() {
                Some("name") => Sort::Name,
                Some("publish_date") => Sort::PublishDate,
                Some("created") => Sort::Created,
                _ => Sort::LastRevision,
            };
            let filter = Filter {
                search: reader.text("search").unwrap_or_default(),
                category: reader.text("category"),
                document_type: reader.text("document_type"),
                status: reader.text("status"),
                related: input.get("related").map(|r| reader.reference(r)),
                sort,
                descending: sort != Sort::Name,
                page: input["page"].as_i64().unwrap_or(1).max(1),
                page_size: input["page_size"].as_i64().unwrap_or(25).clamp(1, 100),
            };
            let filter = reader.done(filter)?;
            let page = owner()?.list(scope, &filter).await?;
            let documents: Vec<Value> = page.entries.iter().map(record).collect();
            Ok(json!({"documents": documents, "total": page.total, "page": filter.page, "page_size": filter.page_size}).into())
        })
    }
}

struct Get;
impl ReadHandler for Get {
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
            let id = id(input)?;
            let document = owner.get(scope, id).await?;
            let revisions = owner.revisions(scope, id).await?;
            let links = owner.links(scope, id).await?;
            let records: Vec<Reference> = links.iter().map(|l| l.record.clone()).collect();
            let named = owner.records().resolve(scope, &records).await?;
            let mut value = record(&document);
            value["revisions"] = json!(revisions
                .iter()
                .map(|r| revision_record(id, r))
                .collect::<Vec<_>>());
            value["related"] = json!(links
                .iter()
                .zip(&named)
                .map(|(l, n)| reference(&l.record, Some(n)))
                .collect::<Vec<_>>());
            Ok(value.into())
        })
    }
}

struct Access;
impl ReadHandler for Access {
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
            let grants = owner()?
                .access(scope, id(input)?)
                .await
                .map_err(|e| invalid(e, "", "id"))?;
            Ok(json!({"access": grants.iter().map(access_record).collect::<Vec<_>>()}).into())
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
            let grant = owner()?
                .preview_share(
                    client,
                    scope,
                    id(input)?,
                    input["principal_type"].as_str().unwrap_or_default(),
                    input["principal_id"].as_i64().unwrap_or_default(),
                    input["download"].as_bool().unwrap_or(false),
                    input["manage"].as_bool().unwrap_or(false),
                    input["share"].as_bool().unwrap_or(false),
                )
                .await
                .map_err(|e| invalid(e, "", "principal"))?;
            Ok(json!({"grants": access_record(&grant)}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let grant = owner()?
                .share(
                    tx,
                    scope,
                    id(input)?,
                    input["principal_type"].as_str().unwrap_or_default(),
                    input["principal_id"].as_i64().unwrap_or_default(),
                    input["download"].as_bool().unwrap_or(false),
                    input["manage"].as_bool().unwrap_or(false),
                    input["share"].as_bool().unwrap_or(false),
                )
                .await
                .map_err(|e| invalid(e, "", "principal"))?;
            Ok(access_record(&grant).into())
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
            let grant = owner()?
                .preview_unshare(
                    client,
                    scope,
                    id(input)?,
                    input["grant"].as_i64().unwrap_or_default(),
                )
                .await
                .map_err(|e| invalid(e, "", "grant"))?;
            Ok(json!({"removes": access_record(&grant)}).into())
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
                .unshare(
                    tx,
                    scope,
                    id(input)?,
                    input["grant"].as_i64().unwrap_or_default(),
                )
                .await
                .map_err(|e| invalid(e, "", "grant"))?;
            Ok(json!({"removed": true}).into())
        })
    }
}

struct Create;
impl WriteHandler for Create {
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
            let draft = draft(input)?;
            let checked = owner()?
                .preview_create(client, scope, &draft)
                .await
                .map_err(|e| invalid(e, "", "related"))?;
            Ok(json!({"creates": draft_record(&checked)}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let draft = draft(input)?;
            let document = owner()?
                .create(tx, scope, &draft)
                .await
                .map_err(|e| invalid(e, "", "related"))?;
            Ok(record(&document).into())
        })
    }
}

struct Update;
impl WriteHandler for Update {
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
            let (id, version, patch) = (id(input)?, version(input)?, patch(input)?);
            let (before, after) = owner()?
                .preview_update(client, scope, id, version, &patch)
                .await
                .map_err(|e| invalid(e, "patch", "patch"))?;
            Ok(json!({"before": record(&before), "after": record(&after)}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, version, patch) = (id(input)?, version(input)?, patch(input)?);
            let document = owner()?
                .update(tx, scope, id, version, &patch)
                .await
                .map_err(|e| invalid(e, "patch", "patch"))?;
            Ok(record(&document).into())
        })
    }
}

struct AddRevision;
impl WriteHandler for AddRevision {
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
            let (id, version) = (id(input)?, version(input)?);
            let (document, planned) = owner()?
                .preview_revision(client, scope, id, version, &revision_input(input))
                .await
                .map_err(|e| invalid(e, "", "artifact_id"))?;
            Ok(json!({"document": record(&document), "adds": planned_record(&planned)}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, version) = (id(input)?, version(input)?);
            let revision = owner()?
                .add_revision(tx, scope, id, version, &revision_input(input))
                .await
                .map_err(|e| invalid(e, "", "artifact_id"))?;
            Ok(revision_record(id, &revision).into())
        })
    }
}

struct LinkRecord;
impl WriteHandler for LinkRecord {
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
            let id = id(input)?;
            let (document, record_ref, linked) = owner()?
                .preview_link(client, scope, id, &record_input(input))
                .await
                .map_err(|e| invalid(e, "", "record"))?;
            let path = owner()?
                .records()
                .kind(&record_ref.kind)
                .and_then(|kind| kind.path_of(&record_ref.id));
            let mut target = reference(&record_ref, None);
            if let Some(path) = path {
                target["path"] = json!(path);
            }
            Ok(
                json!({"document": record(&document), "links": target, "already_linked": linked})
                    .into(),
            )
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let id = id(input)?;
            let link = owner()?
                .link(tx, scope, id, &record_input(input))
                .await
                .map_err(|e| invalid(e, "", "record"))?;
            Ok(link_record(&link).into())
        })
    }
}

struct Unlink;
impl WriteHandler for Unlink {
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
            let id = id(input)?;
            let (document, link) = owner()?
                .preview_unlink(client, scope, id, &record_input(input))
                .await
                .map_err(|e| invalid(e, "", "record"))?;
            Ok(json!({"document": record(&document), "unlinks": link_record(&link)}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let id = id(input)?;
            let record_ref = record_input(input);
            owner()?
                .unlink(tx, scope, id, &record_ref)
                .await
                .map_err(|e| invalid(e, "", "record"))?;
            Ok(json!({"id": id, "unlinked": reference(&record_ref, None)}).into())
        })
    }
}

struct Delete;
impl WriteHandler for Delete {
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
            let (id, version) = (id(input)?, version(input)?);
            let document = owner()?
                .preview_delete(client, scope, id, version)
                .await
                .map_err(|e| invalid(e, "", ""))?;
            Ok(json!({"deletes": record(&document), "with_every_revision_up_to": document.revision}).into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let (id, version) = (id(input)?, version(input)?);
            let deleted = owner()?
                .delete(tx, scope, id, version)
                .await
                .map_err(|e| invalid(e, "", ""))?;
            Ok(json!({"id": id, "deleted": true, "files": deleted.files.len()}).into())
        })
    }
}

// ---- Schemas ----------------------------------------------------------------

fn reference_schema(description: &str) -> Value {
    json!({"type":"object","required":["kind","id"],"description":description,"properties":{
        "kind":{"type":"string","maxLength":100},
        "id":{"type":"string","maxLength":64}
    }})
}
fn create_schema() -> Value {
    json!({"type":"object","properties":{
        "name":{"type":"string","maxLength":255,"description":"1–255 characters; the file's name when omitted."},
        "publish_date":{"type":"string","format":"date","description":"Today in your zone when omitted."},
        "source":{"type":"string","enum":["internal","external"],"description":"internal (a stored file, the default) or external (an https link)."},
        "artifact_id":{"type":"string","maxLength":64,"description":"An internal document's file, stored by you through the documents pages."},
        "external_url":{"type":"string","maxLength":2000,"description":"An external document's https link."},
        "category":{"type":"string","maxLength":64,"description":"A code of the core.document.category pick-list."},
        "sub_category":{"type":"string","maxLength":64,"description":"A code of core.document.sub_category under the category."},
        "document_type":{"type":"string","maxLength":64,"description":"A code of core.document.type."},
        "status":{"type":"string","maxLength":64,"description":"A code of core.document.status; the list's default when omitted."},
        "change_log":{"type":"string","maxLength":2000,"description":"Revision 1's change log; \"Document created\" when omitted."},
        "related":{"type":"array","description":"Records the document relates to, from base.records.search.","items":reference_schema("A record: its kind and id.")}
    }})
}
fn update_schema() -> Value {
    json!({"type":"object","required":["id","version","patch"],"properties":{
        "id":{"type":"integer","minimum":1},
        "version":{"type":"integer","minimum":1,"description":"The stored version; a changed document refuses the save."},
        "patch":{"type":"object","description":"Only the fields to change. An empty string clears a field that may be empty. The revision and files change only through core.document.add_revision.","properties":{
            "name":{"type":"string","maxLength":255},
            "publish_date":{"type":"string","format":"date"},
            "category":{"type":"string","maxLength":64,"description":"A new category clears a sub-category you do not name."},
            "sub_category":{"type":"string","maxLength":64},
            "document_type":{"type":"string","maxLength":64},
            "status":{"type":"string","maxLength":64}
        }}
    }})
}
fn revision_schema() -> Value {
    json!({"type":"object","required":["id","version","change_log"],"properties":{
        "id":{"type":"integer","minimum":1},
        "version":{"type":"integer","minimum":1},
        "artifact_id":{"type":"string","maxLength":64,"description":"An internal document's new file, stored by you through the documents pages."},
        "external_url":{"type":"string","maxLength":2000,"description":"An external document's new https link."},
        "revision":{"type":"integer","minimum":1,"description":"Above the latest; the latest plus one when omitted."},
        "change_log":{"type":"string","maxLength":2000,"description":"What changed, 1–2,000 characters."}
    }})
}
fn link_schema() -> Value {
    json!({"type":"object","required":["id","record"],"properties":{
        "id":{"type":"integer","minimum":1},
        "record":reference_schema("A record from base.records.search: its kind and id.")
    }})
}
fn share_schema() -> Value {
    json!({"type":"object","required":["id","principal_type","principal_id"],"properties":{
        "id":{"type":"integer","minimum":1},
        "principal_type":{"type":"string","enum":["user","company"],"description":"Share with an active account, or the current workplace."},
        "principal_id":{"type":"integer","minimum":1},
        "download":{"type":"boolean","description":"Allow file downloads as well as metadata viewing."},
        "manage":{"type":"boolean","description":"Allow document changes."},
        "share":{"type":"boolean","description":"Allow changing this document's access."}
    }})
}
fn unshare_schema() -> Value {
    json!({"type":"object","required":["id","grant"],"properties":{
        "id":{"type":"integer","minimum":1},
        "grant":{"type":"integer","minimum":1,"description":"The access grant ID returned by core.document.access."}
    }})
}
fn version_schema() -> Value {
    json!({"type":"object","required":["id","version"],"properties":{
        "id":{"type":"integer","minimum":1},
        "version":{"type":"integer","minimum":1}
    }})
}
fn get_schema() -> Value {
    json!({"type":"object","required":["id"],"properties":{"id":{"type":"integer","minimum":1}}})
}
fn list_schema() -> Value {
    json!({"type":"object","properties":{
        "search":{"type":"string","maxLength":200,"description":"Part of the name."},
        "category":{"type":"string","maxLength":64},
        "document_type":{"type":"string","maxLength":64},
        "status":{"type":"string","maxLength":64},
        "related":reference_schema("Only documents linked to this record."),
        "sort":{"type":"string","enum":["last_revision","name","publish_date","created"],"description":"Newest revision first by default."},
        "page":{"type":"integer","minimum":1},
        "page_size":{"type":"integer","minimum":1,"maximum":100}
    }})
}
fn document_output() -> Value {
    json!({"type":"object","description":"A document: id, name, publish_date, source, external_url, category, sub_category, document_type, status, revision, last_revision_at, company_id, created_by, created_at, updated_at, version and path. get adds revisions (newest first, each with its download_path) and related records {kind, id, label, path}."})
}
fn list_output() -> Value {
    json!({"type":"object","description":"documents (each as core.document.get without revisions and related), total, page and page_size."})
}
fn review_output() -> Value {
    json!({"type":"object","description":"The review shows what will change; the commit returns the result."})
}

static LIST: List = List;
static GET: Get = Get;
static ACCESS: Access = Access;
static SHARE: Share = Share;
static UNSHARE: Unshare = Unshare;
static CREATE: Create = Create;
static UPDATE: Update = Update;
static ADD_REVISION: AddRevision = AddRevision;
static LINK: LinkRecord = LinkRecord;
static UNLINK: Unlink = Unlink;
static DELETE: Delete = Delete;
const KEYWORDS: &[&str] = &[
    "document",
    "file",
    "revision",
    "agreement",
    "attachment",
    "crm",
];
inventory::submit! { Contribution::new(DEFINITIONS, "core/document.operations", Provenance { module: crate::MODULE, source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key: "core.document.list", title: "List documents", summary: "Documents you may see, by name, category, type or status, or those linked to one record.", keywords: KEYWORDS, capability: VIEW, input: list_schema, output: list_output, handler: Handler::Read(&LIST), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.document.get", title: "Read a document", summary: "One document with its revisions and related records.", keywords: KEYWORDS, capability: VIEW, input: get_schema, output: document_output, handler: Handler::Read(&GET), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.document.access", title: "List document access", summary: "The people and workplace explicitly allowed to use one document. Needs sharing permission on that document.", keywords: KEYWORDS, capability: VIEW, input: get_schema, output: review_output, handler: Handler::Read(&ACCESS), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.document.share", title: "Share a document", summary: "Give an active account or the current workplace explicit document access.", keywords: KEYWORDS, capability: MANAGE, input: share_schema, output: review_output, handler: Handler::SafeWrite(&SHARE), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.document.unshare", title: "Remove document access", summary: "Remove one explicit access grant. You cannot remove your own grant.", keywords: KEYWORDS, capability: MANAGE, input: unshare_schema, output: review_output, handler: Handler::SafeWrite(&UNSHARE), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.document.create", title: "Create a document", summary: "A document from a file you stored, or an external https link, optionally related to records.", keywords: KEYWORDS, capability: MANAGE, input: create_schema, output: review_output, handler: Handler::SafeWrite(&CREATE), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.document.update", title: "Change a document", summary: "Change the fields named in patch, using the stored version.", keywords: KEYWORDS, capability: MANAGE, input: update_schema, output: review_output, handler: Handler::SafeWrite(&UPDATE), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.document.add_revision", title: "Add a revision", summary: "A new file (or link) for a document, with a change log.", keywords: KEYWORDS, capability: MANAGE, input: revision_schema, output: review_output, handler: Handler::SafeWrite(&ADD_REVISION), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.document.link", title: "Relate a document to a record", summary: "Link a document to a record in any area, such as a company or a contact.", keywords: KEYWORDS, capability: MANAGE, input: link_schema, output: review_output, handler: Handler::SafeWrite(&LINK), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.document.unlink", title: "Remove a document's link to a record", summary: "The document stays; only its link to the record goes.", keywords: KEYWORDS, capability: MANAGE, input: link_schema, output: review_output, handler: Handler::SafeWrite(&UNLINK), channels: Channels::Every, step_up: &[] },
        Operation { key: "core.document.delete", title: "Delete a document", summary: "Remove a document with every revision and file, using the stored version.", keywords: KEYWORDS, capability: MANAGE, input: version_schema, output: review_output, handler: Handler::SafeWrite(&DELETE), channels: Channels::Every, step_up: &[] },
    ],
    guides: &[Guide { key: "core.document", title: "Documents", summary: "The shared document library: files and links with revisions and explicit access.", keywords: KEYWORDS, capability: VIEW, body: include_str!("../guide.md") }],
}) }
