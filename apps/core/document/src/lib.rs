//! A library of documents with revisions, related to records in any area
//! (`core/document`). See `README.md`. This Domain has no Bilimbi
//! counterpart; files are private `base/artifacts` documents owned here
//! (the owner adapter pattern of Core Company's files), and related records
//! are `base/records` references (the records reference holder).
mod agent_operations;
mod contributions;
mod custom;
mod email;
mod files;
mod library;
mod model;
mod reminders;
mod store;
mod web;

use bilibi_artifacts::Artifacts;
use bilibi_database::{Db, Error, MutationBoundary, MutationTransaction, Result};
use bilibi_datetime::DisplayPolicy;
use bilibi_media::Media;
use bilibi_module_registry::{Layer, Module, Service};
use bilibi_notifications::NotificationBoundary;
use bilibi_picklists::PickLists;
use bilibi_principal_directory::PrincipalDirectory;
use bilibi_records::Records;
use bilibi_settings::Settings;
use bilibi_tenancy::{Authentication, ReadClient, Scope};
use jiff::{civil::Date, Timestamp};
use std::sync::Arc;
use store::On;

pub use bilibi_records::Reference;
pub use contributions::{
    ALLOWED_TYPES, DOWNLOAD, LIST_CATEGORY, LIST_STATUS, LIST_SUB_CATEGORY, LIST_TYPE, MANAGE,
    MAX_UPLOAD_BYTES, MERGE, VIEW,
};
pub use email::{ACTOR_PER_HOUR, EMAIL_RECORD, RECIPIENT_PER_DAY, TENANT_PER_HOUR};
pub use model::{
    Custom, Draft, FieldError, Patch, RevisionInput, ScanStatus, Source, Upload, FILE_TYPES,
    FIRST_CHANGE_LOG,
};

pub const MODULE: &str = "core/document";
pub const SERVICE: &str = "core/document.documents";
/// The audit kind of a document and its `base/records` kind.
pub const RECORD: &str = "core.document.document";
/// The audit kind of a revision.
pub const REVISION_RECORD: &str = "core.document.revision";
/// The audit kind of a link to a related record.
pub const LINK_RECORD: &str = "core.document.link";
/// The audit kind of an explicit document access grant.
pub const ACCESS_RECORD: &str = "core.document.access";
/// The audit kind of a stored file's ledger row.
pub const FILE_RECORD: &str = "core.document.file";
/// The audit kind of a person's favourite and follow marks.
pub const MARK_RECORD: &str = "core.document.mark";

/// One document as stored.
#[derive(Clone, Debug)]
pub struct Document {
    pub id: i64,
    /// The responsible filing company. Access remains explicit and is never
    /// inferred from company membership.
    pub company_id: i64,
    pub name: String,
    pub publish_date: Date,
    pub source: Source,
    /// An external document's latest link.
    pub external_url: Option<String>,
    pub category: Option<String>,
    pub sub_category: Option<String>,
    pub document_type: Option<String>,
    pub status: String,
    /// When the document expires. Past that date it counts as expired
    /// whatever its status says.
    pub expires_on: Option<Date>,
    /// The Core Activity task that reminds of the expiry, when there is one.
    pub expiry_activity_id: Option<i64>,
    /// Whose task that is: the person who set the expiry date.
    pub expiry_user_id: Option<i64>,
    /// The tenant's custom field values (`base/custom_fields`).
    pub custom: Custom,
    /// The latest revision's number.
    pub revision: i32,
    pub last_revision_at: Timestamp,
    pub created_by: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// Set while Artifacts handles every revision's bytes before metadata is removed.
    pub deleting_at: Option<Timestamp>,
    pub version: i64,
}

/// One revision of a document. An internal revision has a file; an external
/// one a link.
#[derive(Clone, Debug)]
pub struct Revision {
    pub id: i64,
    pub document_id: i64,
    pub revision: i32,
    pub name: String,
    pub artifact_id: Option<String>,
    pub file_name: Option<String>,
    pub content_type: Option<String>,
    pub byte_size: Option<i64>,
    pub sha256: Option<String>,
    pub external_url: Option<String>,
    pub change_log: String,
    pub scan_status: ScanStatus,
    pub created_by: i64,
    pub created_at: Timestamp,
}

/// One explicit document authority granted to one person.
#[derive(Clone, Debug)]
pub struct AccessGrant {
    pub id: i64,
    pub user_id: i64,
    pub can_view: bool,
    pub can_download: bool,
    pub can_manage: bool,
    pub can_share: bool,
    /// When the grant ends; `None` for one that does not expire.
    pub expires_at: Option<Timestamp>,
}

/// What a share did: the grant, and what became of the email request.
#[derive(Clone, Debug)]
pub struct Shared {
    pub grant: AccessGrant,
    /// `None` when no email was asked for.
    pub email: Option<EmailOutcome>,
}

/// What became of a request to email a link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmailOutcome {
    /// Recorded and queued for delivery.
    Queued,
    /// Not sent: a limit was reached (`actor_limit`, `recipient_limit` or
    /// `tenant_limit`). The share itself stands.
    RateLimited(&'static str),
}
impl EmailOutcome {
    pub fn code(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::RateLimited(_) => "rate_limited",
        }
    }
}

/// An explicit grant to give one active account. `until` is the last day it
/// works, in the actor's zone; `email` sends the person a link to the
/// document after the change commits.
#[derive(Clone, Copy, Debug, Default)]
pub struct ShareInput {
    pub user_id: i64,
    pub can_download: bool,
    pub can_manage: bool,
    pub can_share: bool,
    pub until: Option<Date>,
    pub email: bool,
}

/// A record a document relates to.
#[derive(Clone, Debug)]
pub struct Link {
    pub id: i64,
    pub record: Reference,
    pub linked_by: i64,
    pub linked_at: Timestamp,
}

/// A file `store_file` kept, ready to become a revision.
#[derive(Clone, Debug)]
pub struct StoredFile {
    pub artifact_id: String,
    pub file_name: String,
    pub content_type: String,
    pub byte_size: i64,
    pub sha256: String,
}

/// A revision's file, read for one download. Never cache it for another
/// reader.
pub struct File {
    pub file_name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// A revision `add_revision` would add, as its preview shows it.
#[derive(Clone, Debug)]
pub struct PlannedRevision {
    pub revision: i32,
    pub file_name: Option<String>,
    pub external_url: Option<String>,
    pub change_log: String,
}

/// What `delete` staged: the files still to delete from `base/artifacts`
/// once the caller's transaction commits.
#[derive(Clone, Debug, Default)]
pub struct Deleted {
    pub files: Vec<String>,
}

/// One reader's own favourite and follow marks on one document. Marks are
/// personal: they grant nothing and nobody else sees them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Marks {
    pub favourite: bool,
    pub following: bool,
}

/// The marks a change sets; `None` leaves a mark as it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MarkPatch {
    pub favourite: Option<bool>,
    pub following: Option<bool>,
}

/// Narrows the library by expiry date.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpiryFilter {
    /// The expiry date has passed.
    Expired,
    /// Expires within the next 30 days.
    Soon,
}
impl ExpiryFilter {
    pub fn code(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Soon => "soon",
        }
    }
    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "expired" => Some(Self::Expired),
            "soon" => Some(Self::Soon),
            _ => None,
        }
    }
}

/// Narrows the library to the reader's own marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkFilter {
    Favourites,
    Following,
}
impl MarkFilter {
    pub fn code(self) -> &'static str {
        match self {
            Self::Favourites => "favourites",
            Self::Following => "following",
        }
    }
    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "favourites" => Some(Self::Favourites),
            "following" => Some(Self::Following),
            _ => None,
        }
    }
}

/// Why another document looks like the one asked about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DuplicateReason {
    /// Some revision holds a file with the same content.
    SameFile,
    /// An external document with the same latest link.
    SameLink,
    /// The same name, ignoring case and spaces at the ends.
    SameName,
}
impl DuplicateReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::SameFile => "same_file",
            Self::SameLink => "same_link",
            Self::SameName => "same_name",
        }
    }
    pub fn text(self) -> &'static str {
        match self {
            Self::SameFile => "Same file",
            Self::SameLink => "Same link",
            Self::SameName => "Same name",
        }
    }
}

/// A document the reader may see that looks like another one.
#[derive(Clone, Debug)]
pub struct Duplicate {
    pub document: Document,
    pub reasons: Vec<DuplicateReason>,
}

/// One document a merge removes, fenced on the version the reader saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergeSource {
    pub id: i64,
    pub version: i64,
}

/// What a merge does, as its preview shows it.
#[derive(Clone, Debug)]
pub struct MergePlan {
    pub survivor: Document,
    /// The documents that go, in id order.
    pub sources: Vec<Document>,
    /// Revisions that move to the survivor, renumbered after its latest.
    pub revisions: i64,
    /// Links the survivor does not have yet.
    pub links: i64,
}

/// What a merge did.
#[derive(Clone, Debug)]
pub struct Merged {
    pub survivor: Document,
    pub removed: Vec<i64>,
    pub revisions: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    LastRevision,
    Name,
    PublishDate,
    Created,
}

/// A page of the documents the reader may see. `page` counts from 1.
#[derive(Clone, Debug)]
pub struct Filter {
    /// Part of the name, ignoring case.
    pub search: String,
    pub category: Option<String>,
    pub document_type: Option<String>,
    pub status: Option<String>,
    /// Related to this record.
    pub related: Option<Reference>,
    /// Only the reader's own favourites or follows.
    pub marked: Option<MarkFilter>,
    pub expiry: Option<ExpiryFilter>,
    pub sort: Sort,
    pub descending: bool,
    pub page: i64,
    pub page_size: i64,
}
impl Default for Filter {
    fn default() -> Self {
        Self {
            search: String::new(),
            category: None,
            document_type: None,
            status: None,
            related: None,
            marked: None,
            expiry: None,
            sort: Sort::LastRevision,
            descending: true,
            page: 1,
            page_size: 25,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Page {
    pub entries: Vec<Document>,
    pub total: i64,
}

pub type WriteResult<T> = std::result::Result<T, WriteError>;
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("The document could not be found.")]
    NotFound,
    #[error("You may not change this document.")]
    Forbidden,
    #[error("The document changed since this edit began. Reload before saving.")]
    Stale,
    #[error("Invalid input")]
    Invalid(Vec<FieldError>),
    /// `base/records.check` refused the related record; the text completes
    /// "The related record …".
    #[error("The related record {0}.")]
    RelatedRefused(&'static str),
    #[error("The file is larger than the {} MiB allowed.", max_bytes / (1024 * 1024))]
    TooLarge { max_bytes: u64 },
    #[error("This file is quarantined and cannot be downloaded.")]
    Quarantined,
    #[error("The documents cannot be merged: {0}.")]
    MergeRefused(&'static str),
    #[error("This file is retained until its retain-until date and cannot be deleted yet.")]
    Retained,
    #[error("This file is under legal hold and cannot be deleted.")]
    LegalHold,
    #[error(transparent)]
    Access(#[from] Error),
}
impl WriteError {
    pub(crate) fn field(field: &'static str, message: impl Into<String>) -> Self {
        Self::Invalid(vec![FieldError::new(field, message)])
    }
}

/// The `core/document` facade. Every call refreshes the scope and checks its
/// capability; no call takes a tenant, company or actor from input.
pub struct Documents {
    db: Db,
    authentication: Arc<Authentication>,
    boundary: Arc<MutationBoundary>,
    artifacts: Arc<Artifacts>,
    media: Arc<Media>,
    records: Arc<Records>,
    picklists: Arc<PickLists>,
    settings: Arc<Settings>,
    display: Arc<DisplayPolicy>,
    directory: Arc<PrincipalDirectory>,
    notifications: Arc<NotificationBoundary>,
    custom: Arc<bilibi_custom_fields::CustomFields>,
    activities: Arc<bilibi_activity::Activities>,
    queue: Arc<bilibi_queue::Queue>,
}
impl Documents {
    async fn fresh(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
    ) -> Result<Scope> {
        self.authentication.reprove(client, scope).await
    }
    fn store(&self) -> store::Store<'_> {
        store::Store {
            records: &self.records,
            picklists: &self.picklists,
            display: &self.display,
            directory: &self.directory,
            notifications: &self.notifications,
            settings: &self.settings,
            custom: &self.custom,
            activities: &self.activities,
            artifacts: &self.artifacts,
            queue: &self.queue,
        }
    }

    /// Keeps `upload` as a private file under the `core.document` artifacts
    /// owner, filed under the actor's company, outside any business
    /// transaction (as `base/artifacts` requires). Needs
    /// `core.document.manage`. The type comes from the file name's extension,
    /// must be one of `core.document.allowed_types` and must match the bytes;
    /// the size must be within `core.document.max_upload_bytes` and the
    /// operator's `artifacts.max_bytes`. Attach the result with `attach`,
    /// `create` or `add_revision`; a file never attached is removed by the
    /// cleanup job.
    pub async fn store_file(&self, scope: &Scope, upload: Upload<'_>) -> WriteResult<StoredFile> {
        let mut client = self.db.get().await.map_err(Error::from)?;
        let scope = self.fresh(&**client, scope).await?;
        files::store(
            &mut client,
            &scope,
            &self.boundary,
            &self.artifacts,
            &self.media,
            &self.settings,
            upload,
        )
        .await
    }

    /// Files a document with revision 1 from a file `store_file` returned to
    /// this actor, and links it to `related` after `base/records.check` for
    /// the actor. Idempotent on `artifact_id`: once attached, a second call
    /// returns the same document and adds the link. Needs
    /// `core.document.manage`.
    pub async fn attach(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        related: &Reference,
        artifact_id: &str,
        name: Option<&str>,
        category: Option<&str>,
    ) -> WriteResult<i64> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store()
            .attach(tx, &scope, related, artifact_id, name, category)
            .await
    }

    /// Creates a document from `draft` with its first revision and links.
    /// Needs `core.document.manage`.
    pub async fn create(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        draft: &Draft,
    ) -> WriteResult<Document> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().create(tx, &scope, draft).await
    }

    /// Changes the fields `patch` names, fenced on `version`. Needs
    /// `core.document.manage` and a document the actor may see.
    pub async fn update(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
        patch: &Patch,
    ) -> WriteResult<Document> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().update(tx, &scope, id, version, patch).await
    }

    /// Adds a revision: a new stored file for an internal document, a new
    /// link for an external one. The number defaults to the latest plus one
    /// and must be above it; skipped numbers stay skipped. Fenced on the
    /// document's `version`, which it moves on.
    pub async fn add_revision(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
        input: &RevisionInput,
    ) -> WriteResult<Revision> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store()
            .add_revision(tx, &scope, id, version, input)
            .await
    }

    /// Relates the document to `record` after `base/records.check` for the
    /// actor. Linking a record already linked returns that link.
    pub async fn link(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        record: &Reference,
    ) -> WriteResult<Link> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().link(tx, &scope, id, record).await
    }

    /// Removes the document's link to `record`.
    pub async fn unlink(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        record: &Reference,
    ) -> WriteResult<()> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().unlink(tx, &scope, id, record).await
    }

    /// Removes the document with its revisions, links and marks in the
    /// caller's transaction, fenced on `version`. Its files remain attached
    /// and readable until `base/artifacts` deletes them as one protected
    /// batch once it commits. A retained or held revision refuses the delete
    /// before anything is marked (`WriteError::Retained` or `LegalHold`, in
    /// `preview_delete` too), so the document stays readable with its full
    /// history; the later batch check is the same fence under its own locks.
    pub async fn delete(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        version: i64,
    ) -> WriteResult<Deleted> {
        let scope = self.fresh(tx.sql(), scope).await?;
        let deleted = self.store().delete(tx, &scope, id, version).await?;
        {
            let (db, authentication, boundary, artifacts) = (
                self.db.clone(),
                self.authentication.clone(),
                self.boundary.clone(),
                self.artifacts.clone(),
            );
            let files = deleted.files.clone();
            let document = id;
            tx.after_commit(move || {
                tokio::spawn(async move {
                    let Ok(mut client) = db.get().await else {
                        return;
                    };
                    let Ok(scope) = authentication.reprove(&**client, &scope).await else {
                        return;
                    };
                    match files::discard_document(&mut client, &scope, &artifacts, &files).await {
                        Ok(()) => {
                            if let Err(error) = files::finalize_document_delete(&mut client, &scope, &boundary, document).await {
                                eprintln!("warning: document {document} metadata awaits delete finalization: {error}");
                            }
                        }
                        Err(error) => eprintln!(
                            "warning: document {document} and its full revision history await delete retry: {error}"
                        ),
                    }
                });
            });
        }
        Ok(deleted)
    }

    /// Deletes files this actor stored but never attached. Released document
    /// revisions are refused here; `delete` handles their history as one
    /// protected batch.
    pub async fn discard_files(
        &self,
        scope: &Scope,
        artifact_ids: &[String],
    ) -> Result<Vec<(String, WriteError)>> {
        let mut client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        Ok(files::discard(
            &mut client,
            &scope,
            &self.boundary,
            &self.artifacts,
            artifact_ids,
        )
        .await)
    }

    /// One document the reader may see through an explicit user
    /// grant. Links never grant access.
    pub async fn get(&self, scope: &Scope, id: i64) -> Result<Document> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        self.store().get(&**client, &scope, id).await
    }
    /// The document's revisions, newest number first.
    pub async fn revisions(&self, scope: &Scope, id: i64) -> Result<Vec<Revision>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        self.store().get(&**client, &scope, id).await?;
        store::revisions(&**client, &scope, id).await
    }
    /// The latest revision of each document given, by document id, in one
    /// read, for the pages. The documents are ones `list` or `get` returned
    /// to this reader.
    pub(crate) async fn latest_revisions(
        &self,
        scope: &Scope,
        documents: &[Document],
    ) -> Result<std::collections::HashMap<i64, Revision>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        let ids: Vec<i64> = documents.iter().map(|d| d.id).collect();
        store::latest_revisions(&**client, &scope, &ids).await
    }
    /// Which of `ids` the reader may change: the manage capability and an
    /// explicit grant that has not ended, in one read, for the pages.
    pub(crate) async fn changeable(
        &self,
        scope: &Scope,
        ids: &[i64],
    ) -> Result<std::collections::HashSet<i64>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        if !bilibi_authz::holds_proof(&**client, &scope, MANAGE).await? {
            return Ok(std::collections::HashSet::new());
        }
        Ok(client
            .query(
                "SELECT document_id FROM core_document_access WHERE tenant_id=$1 AND user_id=$2 AND document_id = ANY($3) AND can_manage AND (expires_at IS NULL OR expires_at > now())",
                &[&scope.tenant(), &scope.user(), &ids],
            )
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect())
    }
    /// The records the document relates to, oldest link first.
    pub async fn links(&self, scope: &Scope, id: i64) -> Result<Vec<Link>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        self.store().get(&**client, &scope, id).await?;
        store::links(&**client, &scope, id).await
    }
    /// A page of the library restricted to explicit user
    /// grants. `related` filters that already-authorized set; it grants no
    /// additional access.
    pub async fn list(&self, scope: &Scope, filter: &Filter) -> Result<Page> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        let zone = self
            .display
            .display(&**client, &scope, scope.current_company(), "en")
            .await?
            .timezone;
        let today = Timestamp::now()
            .in_tz(&zone)
            .map_err(|_| Error::Internal)?
            .date();
        self.store().list(&**client, &scope, filter, today).await
    }
    /// The documents linked to `record`, newest revision first; the record
    /// must be one the reader may view.
    pub async fn for_record(
        &self,
        scope: &Scope,
        record: &Reference,
        page_size: i64,
    ) -> Result<Page> {
        let filter = Filter {
            related: Some(record.clone()),
            page_size,
            ..Filter::default()
        };
        self.list(scope, &filter).await
    }
    /// One revision's file, read through `base/artifacts` (which rechecks
    /// access and audits the read) every time. Needs
    /// `core.document.download` and a document the reader may see; artifacts
    /// also requires the reader to belong to the filing company.
    pub async fn read_file(&self, scope: &Scope, id: i64, revision: i32) -> WriteResult<File> {
        let mut client = self.db.get().await.map_err(Error::from)?;
        let scope = self.fresh(&**client, scope).await?;
        let document = self.store().get(&**client, &scope, id).await?;
        files::read(&mut client, &scope, &self.artifacts, &document, revision).await
    }
    /// What `create` would store, with no write, on the client a plan hands
    /// its preview: the draft with its defaults filled in and its records
    /// in their one spelling. An omitted publish date stays `None` (today on
    /// commit).
    pub async fn preview_create(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        draft: &Draft,
    ) -> WriteResult<Draft> {
        Ok(self
            .store()
            .prepare_create(On::Preview(client), scope, draft)
            .await?
            .draft())
    }
    /// What `update` would store, with no write: the document before and
    /// after.
    pub async fn preview_update(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        version: i64,
        patch: &Patch,
    ) -> WriteResult<(Document, Document)> {
        self.store()
            .prepare_update(On::Preview(client), scope, id, version, patch)
            .await
    }
    /// What `add_revision` would add, with no write.
    pub async fn preview_revision(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        version: i64,
        input: &RevisionInput,
    ) -> WriteResult<(Document, PlannedRevision)> {
        let plan = self
            .store()
            .prepare_revision(On::Preview(client), scope, id, version, input)
            .await?;
        let planned = PlannedRevision {
            revision: plan.revision,
            file_name: plan.file.as_ref().map(|f| f.file_name().to_owned()),
            external_url: plan.external_url,
            change_log: plan.change_log,
        };
        Ok((plan.document, planned))
    }
    /// What `link` would do, with no write: the document, the record in its
    /// one spelling, and whether it is linked already.
    pub async fn preview_link(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        record: &Reference,
    ) -> WriteResult<(Document, Reference, bool)> {
        let (document, record, existing) = self
            .store()
            .prepare_link(On::Preview(client), scope, id, record)
            .await?;
        Ok((document, record, existing.is_some()))
    }
    /// The link `unlink` would remove, with no write.
    pub async fn preview_unlink(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        record: &Reference,
    ) -> WriteResult<(Document, Link)> {
        self.store()
            .prepare_unlink(On::Preview(client), scope, id, record)
            .await
    }
    /// The document `delete` would remove, with no write.
    pub async fn preview_delete(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        version: i64,
    ) -> WriteResult<Document> {
        self.store()
            .prepare_delete(On::Preview(client), scope, id, version)
            .await
    }
    /// The reader's own favourite and follow marks on `ids`, in one read.
    /// Only documents the reader may see are answered; a document without
    /// marks is `Marks::default()`.
    pub async fn marks(
        &self,
        scope: &Scope,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Marks>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        library::marks(&**client, &scope, ids).await
    }

    /// Sets the reader's own marks on a document they may see. Needs
    /// `core.document.view` and an explicit view grant; it changes nothing but
    /// the reader's marks.
    pub async fn mark(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        patch: MarkPatch,
    ) -> WriteResult<Marks> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().mark(tx, &scope, id, patch).await
    }

    /// The document and the marks `mark` would leave, with no write.
    pub async fn preview_mark(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        patch: MarkPatch,
    ) -> WriteResult<(Document, Marks)> {
        self.store().preview_mark(client, scope, id, patch).await
    }

    /// The other documents the reader may see that look like `id`: the same
    /// file in any revision, the same latest link or the same name. A document
    /// the reader has no grant on is never named and never counted.
    pub async fn duplicates(&self, scope: &Scope, id: i64) -> Result<Vec<Duplicate>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        self.store().duplicates(&**client, &scope, id).await
    }

    /// The documents the reader may see that already hold a file with this
    /// SHA-256, for the warning after an upload.
    pub async fn duplicates_of_file(&self, scope: &Scope, sha256: &str) -> Result<Vec<Document>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        library::duplicates_of_file(&**client, &scope, sha256).await
    }

    /// Stores a private copy of the document's latest file under the actor's
    /// company, for `copy`. Like `store_file` it works outside any business
    /// transaction (the page-write exception for bytes) and the cleanup job
    /// removes the file if it is never attached. Needs `core.document.manage`
    /// and the download grant on the source. A link has no file to copy.
    pub async fn copy_file(&self, scope: &Scope, id: i64) -> WriteResult<StoredFile> {
        let mut client = self.db.get().await.map_err(Error::from)?;
        let scope = self.fresh(&**client, scope).await?;
        let document = self.store().copyable(&**client, &scope, id).await?;
        let file = files::read(
            &mut client,
            &scope,
            &self.artifacts,
            &document,
            document.revision,
        )
        .await?;
        files::store(
            &mut client,
            &scope,
            &self.boundary,
            &self.artifacts,
            &self.media,
            &self.settings,
            Upload {
                bytes: &file.bytes,
                file_name: &file.file_name,
            },
        )
        .await
    }

    /// Copies the document: the same name (prefixed "Copy of"), publication
    /// date, category, sub-category and type, and only the latest revision,
    /// whose change log is "Document created". `artifact_id` is the file
    /// `copy_file` returned; a link needs none. The copy is filed under the
    /// actor's company with the list's default status. Links, access grants
    /// and marks are not copied: only the actor can see the copy until they
    /// share it. Needs `core.document.manage`.
    pub async fn copy(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        artifact_id: Option<&str>,
    ) -> WriteResult<Document> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().copy(tx, &scope, id, artifact_id).await
    }

    /// The draft `copy` would create, with no write.
    pub async fn preview_copy(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        artifact_id: Option<&str>,
    ) -> WriteResult<Draft> {
        self.store()
            .preview_copy(client, scope, id, artifact_id)
            .await
    }

    /// What merging `sources` into `survivor` would do, with no write.
    pub async fn preview_merge(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        survivor: MergeSource,
        sources: &[MergeSource],
    ) -> WriteResult<MergePlan> {
        self.store()
            .preview_merge(client, scope, survivor, sources)
            .await
    }

    /// Merges duplicate documents into `survivor`. Each source's revisions
    /// move to the survivor after its latest, renumbered and with the change
    /// log saying where they came from; links, explicit grants of active
    /// accounts and marks are combined; each source is removed and
    /// `base/records` repoints every reference to it. Needs
    /// `core.document.merge` and, on every document, `core.document.manage`
    /// with the explicit manage and share grants, because a merge unites who
    /// may read them. The documents must be of one source (files or links) and
    /// filed under one company. Files stay where they are.
    pub async fn merge(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        survivor: MergeSource,
        sources: &[MergeSource],
    ) -> WriteResult<Merged> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().merge(tx, &scope, survivor, sources).await
    }

    /// Whether the reader may merge: the capability, plus manage and share
    /// grants on `document`.
    pub async fn may_merge(&self, scope: &Scope, document: &Document) -> Result<bool> {
        let client = self.db.get().await?;
        if !bilibi_authz::holds_proof(&**client, scope, MERGE).await?
            || !bilibi_authz::holds_proof(&**client, scope, MANAGE).await?
        {
            return Ok(false);
        }
        Ok(
            store::permission(&**client, scope, document.id, "manage").await?
                && store::permission(&**client, scope, document.id, "share").await?,
        )
    }
    /// Names references with the reader's permissions (`base/records`).
    pub(crate) fn records(&self) -> &Records {
        &self.records
    }
    /// Whether the reader may create and change documents.
    pub async fn may_manage(&self, scope: &Scope) -> Result<bool> {
        let client = self.db.get().await?;
        bilibi_authz::holds_proof(&**client, scope, MANAGE).await
    }
    /// Whether the reader holds the module capability and an explicit manage
    /// grant on `document`.
    pub async fn may_change(&self, scope: &Scope, document: &Document) -> Result<bool> {
        let client = self.db.get().await?;
        if !bilibi_authz::holds_proof(&**client, scope, MANAGE).await? {
            return Ok(false);
        }
        store::permission(&**client, scope, document.id, "manage").await
    }
    /// The tenant's active custom fields for documents, in the order defined.
    pub async fn custom_fields(
        &self,
        scope: &Scope,
    ) -> Result<Vec<bilibi_custom_fields::Definition>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        bilibi_authz::authorize_proof(&**client, &scope, VIEW).await?;
        self.custom
            .active(&**client, &scope, custom::RECORD_TYPE)
            .await
    }
    /// Whether the reader may download files.
    pub async fn may_download(&self, scope: &Scope) -> Result<bool> {
        let client = self.db.get().await?;
        bilibi_authz::holds_proof(&**client, scope, DOWNLOAD).await
    }

    /// The explicit grants, visible only to someone allowed to share this document.
    pub async fn access(&self, scope: &Scope, id: i64) -> WriteResult<Vec<AccessGrant>> {
        let client = self.db.get().await.map_err(Error::from)?;
        let scope = self.fresh(&**client, scope).await?;
        self.store().access(&**client, &scope, id).await
    }

    /// Adds or replaces one explicit user grant, optionally ending after a
    /// day (`until`, UTC). With `email`, the request to mail the person a link
    /// to the document is recorded (audited) and queued as a durable job
    /// unless a limit is reached (`EmailOutcome::RateLimited`; the grant still
    /// stands). The link carries no token: it opens the document only for
    /// that account, while the grant lasts, and delivery rechecks both.
    pub async fn share(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        input: &ShareInput,
    ) -> WriteResult<Shared> {
        let scope = self.fresh(tx.sql(), scope).await?;
        let (grant, document) = self.store().share(tx, &scope, id, input).await?;
        let email = if input.email {
            Some(
                self.store()
                    .request_email(tx, &scope, &document, grant.user_id)
                    .await?,
            )
        } else {
            None
        };
        Ok(Shared { grant, email })
    }

    /// The explicit grant `share` would add or replace, without writing it.
    pub async fn preview_share(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        input: &ShareInput,
    ) -> WriteResult<AccessGrant> {
        self.store().preview_share(client, scope, id, input).await
    }

    /// Removes one explicit grant. The caller's own grant cannot be removed.
    pub async fn unshare(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        grant: i64,
    ) -> WriteResult<()> {
        let scope = self.fresh(tx.sql(), scope).await?;
        self.store().unshare(tx, &scope, id, grant).await
    }

    /// The explicit grant `unshare` would remove, without writing it.
    pub async fn preview_unshare(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        grant: i64,
    ) -> WriteResult<AccessGrant> {
        self.store().preview_unshare(client, scope, id, grant).await
    }
}

inventory::submit! { Module {
    id: MODULE, layer: Layer::Core,
    dependencies: &["base/agent_api", "base/artifacts", "base/authz", "base/custom_fields", "base/database", "base/datetime", "base/grid", "base/locale", "base/mail", "base/media", "base/menu", "base/notifications", "base/party_ids", "base/picklists", "base/principal_directory", "base/queue", "base/records", "base/settings", "base/tenancy", "base/ui", "core/activity", "core/company", "core/user"],
    migrations: &[
        ("core-document-001-documents", include_str!("../migrations/001-documents.sql")),
        ("core-document-002-marks", include_str!("../migrations/002-marks.sql")),
        ("core-document-003-expiry-sharing-custom", include_str!("../migrations/003-expiry-sharing-custom.sql")),
        ("core-document-004-email-outbox", include_str!("../migrations/004-email-outbox.sql")),
    ],
    services: &[Service { id: SERVICE, factory: |s| {
        files::bind(s.get("base/tenancy.parties")?)?;
        let documents = Arc::new(Documents {
            db: s.get::<Db>("base/database.pool")?.as_ref().clone(),
            authentication: s.get("base/tenancy.authentication")?,
            boundary: s.get("base/database.mutations")?,
            artifacts: s.get("base/artifacts.store")?,
            media: s.get(bilibi_media::SERVICE)?,
            records: s.get(bilibi_records::SERVICE)?,
            picklists: s.get(bilibi_picklists::SERVICE)?,
            settings: s.get("base/settings.store")?,
            display: s.get("base/datetime.display")?,
            directory: s.get("base/principal_directory.directory")?,
            notifications: s.get("base/notifications.sink")?,
            custom: s.get(bilibi_custom_fields::SERVICE)?,
            activities: s.get(bilibi_activity::SERVICE)?,
            queue: s.get("base/queue.store")?,
        });
        agent_operations::bind(documents.clone())?;
        Ok(documents)
    }}, Service { id: email::HANDLER, factory: |s| {
        Ok(Arc::new(bilibi_queue::HandlerService(Arc::new(email::EmailJob {
            db: s.get::<Db>("base/database.pool")?.as_ref().clone(),
            mailer: s.get("base/mail.mailer")?,
            authentication: s.get("base/tenancy.authentication")?,
            origin: std::env::var("APP_ORIGIN").unwrap_or_else(|_| "http://localhost:3000".into()).trim_end_matches('/').to_owned(),
        }))))
    }}, Service { id: web::SERVICE, factory: |s| {
        Ok(Arc::new(bilibi_ui::page::PageService::<Scope, Error>(Arc::new(web::Pages::new(s)?))))
    }}, Service { id: web::FILES, factory: |s| {
        Ok(Arc::new(bilibi_ui::download::DownloadService::<Scope, Error>(Arc::new(web::Files(s.get(SERVICE)?)))))
    }}],
} }
