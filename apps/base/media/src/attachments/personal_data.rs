//! A subject's attachments under an approved privacy request (ADR 05): the
//! sealed `base/personal_data` grant, not a `SubjectAction`, is the
//! authority to list and read them for an export, turn them into tombstones
//! and remove their bytes.
use super::{bound, facts, valid_public_id, Attachment, AttachmentSubject, CLEANUP_BATCH, SELECT};
use crate::{audit, storage, Error, Media, Result, StreamFile};
use bilibi_database::{Change, MutationTransaction};
use bilibi_party_ids::PersonId;
use bilibi_personal_data::{Cause, Disclosure, Erasure, Holds};
use bilibi_tenancy::{ProofClient, Scope};
use serde_json::json;
use tokio_postgres::{Client, GenericClient};

/// The audit record kinds of an attachment row and of its asset.
pub const ATTACHMENT_KIND: &str = "base/media/attachment";
pub const ASSET_KIND: &str = "base/media/asset";
/// Attachments read per page when a request lists a subject's files.
const DISCLOSE_PAGE: i64 = 500;
/// Subjects read per page when a sweep looks for files whose bytes remain.
const SWEPT_PAGE: i64 = 100;

/// One attachment an erasure turned into a tombstone. Its row and its removed
/// asset are both audit records whose evidence the erasure clears.
#[derive(Clone, Debug)]
pub struct ErasedAttachment {
    pub public_id: String,
    /// The attachment row, kind [`ATTACHMENT_KIND`].
    pub id: i64,
    /// The removed asset row, kind [`ASSET_KIND`].
    pub asset_id: i64,
}
/// What `erase_attachments` did.
#[derive(Clone, Debug, Default)]
pub struct AttachmentErasure {
    pub erased: Vec<ErasedAttachment>,
    /// Attachments a legal hold kept, on the attachment or on its asset:
    /// (attachment row, hold).
    pub held: Vec<(i64, i64)>,
}
/// What one `remove_erased_bytes` call did.
#[derive(Clone, Copy, Debug, Default)]
pub struct Removal {
    /// Files removed by this call; each is one audited change.
    pub removed: u64,
    /// Files still waiting: call again (another call may be removing them now).
    pub more: bool,
}

impl Media {
    /// Whether an approved request's grant reaches `subject`'s files: its kind
    /// is declared person-keyed and its ID is the grant's person, or its owner
    /// says it belongs to that person (`SubjectOwner::person_subject`), asked
    /// on `client`. A sweep grant (no person) reaches no files.
    async fn disclosed_subject(
        &self,
        client: &dyn ProofClient,
        scope: &Scope,
        grant: &Disclosure,
        subject: AttachmentSubject<'_>,
    ) -> Result<()> {
        grant.check(scope)?;
        match grant.check_file_subject(scope, subject.kind, subject.id) {
            Err(bilibi_database::Error::Forbidden) => {
                self.vouched(client, scope, Some(grant.person()), subject)
                    .await
            }
            declared => Ok(declared?),
        }
    }
    /// As `disclosed_subject`, for an erasure. A sweep grant names no
    /// person: it reaches a subject only when its owner says its own sweep
    /// erased that record (`SubjectOwner::swept_subject`).
    async fn erased_subject(
        &self,
        client: &dyn ProofClient,
        scope: &Scope,
        grant: &Erasure,
        subject: AttachmentSubject<'_>,
    ) -> Result<()> {
        grant.check(scope)?;
        if let (Cause::Sweep(_), None) = (grant.cause(), grant.person()) {
            return if self
                .subjects
                .swept_subject(client, scope, subject.kind, subject.id)
                .await?
            {
                Ok(())
            } else {
                Err(Error::Database(bilibi_database::Error::Forbidden))
            };
        }
        match grant.check_file_subject(scope, subject.kind, subject.id) {
            Err(bilibi_database::Error::Forbidden) => {
                self.vouched(client, scope, grant.person(), subject).await
            }
            declared => Ok(declared?),
        }
    }
    async fn vouched(
        &self,
        client: &dyn ProofClient,
        scope: &Scope,
        person: Option<PersonId>,
        subject: AttachmentSubject<'_>,
    ) -> Result<()> {
        let Some(person) = person else {
            return Err(Error::Database(bilibi_database::Error::Forbidden));
        };
        if self
            .subjects
            .person_subject(client, scope, subject.kind, person, subject.id)
            .await?
        {
            Ok(())
        } else {
            Err(Error::Database(bilibi_database::Error::Forbidden))
        }
    }

    /// Every live attachment of `subject` (ADR 05), however many, for an
    /// approved access request. The grant is the authority: the subject's
    /// owner is not asked to authorize, so a retired record's files are still
    /// listed; an owner whose subjects are not person IDs only vouches that
    /// the subject is the request's person's.
    pub async fn disclosed_attachments(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        grant: &Disclosure,
        subject: AttachmentSubject<'_>,
    ) -> Result<Vec<Attachment>> {
        self.disclosed_subject(client, scope, grant, subject)
            .await?;
        let (mut all, mut after) = (Vec::new(), 0_i64);
        loop {
            let rows = client
                .query(
                    &format!("{SELECT} WHERE a.tenant_id=$1 AND a.subject_type=$2 AND a.subject_id=$3 AND a.id>$4 ORDER BY a.id LIMIT $5"),
                    &[&scope.tenant(), &subject.kind, &subject.id, &after, &DISCLOSE_PAGE],
                )
                .await?;
            let done = (rows.len() as i64) < DISCLOSE_PAGE;
            for row in &rows {
                let row = bound(row)?;
                after = row.id;
                all.push(row.summary);
            }
            if done {
                return Ok(all);
            }
        }
    }

    /// The bytes of one of `subject`'s live attachments, for an approved
    /// access request's bundle. Another subject's file is `NotFound`.
    pub async fn disclosed_attachment(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        grant: &Disclosure,
        subject: AttachmentSubject<'_>,
        public_id: &str,
    ) -> Result<StreamFile> {
        self.disclosed_subject(client, scope, grant, subject)
            .await?;
        if !valid_public_id(public_id) {
            return Err(Error::NotFound);
        }
        let row = client
            .query_opt(
                &format!("{SELECT} WHERE a.tenant_id=$1 AND a.public_id=$2 AND a.subject_type=$3 AND a.subject_id=$4"),
                &[&scope.tenant(), &public_id, &subject.kind, &subject.id],
            )
            .await?
            .ok_or(Error::NotFound)?;
        let row = bound(&row)?;
        let asset = self.fetch(client, scope.tenant(), row.asset_id).await?;
        let bytes = storage::read(&self.disks(client).await?, &asset.disk, &asset.key).await?;
        Ok(StreamFile {
            bytes,
            filename: asset
                .asset
                .original_filename
                .unwrap_or_else(|| public_id.to_owned()),
            mime_type: asset.asset.mime_type,
        })
    }

    /// After a retention sweep (ADR 05): removes the bytes of erased
    /// attachments of subjects of `kind` whose owner says its sweep erased
    /// them (`SubjectOwner::swept_subject`), through
    /// [`Media::remove_erased_bytes`], one file per transaction. Only a sweep
    /// grant. A subject the owner does not vouch for (one a person's request
    /// erased, whose cleanup is that request's) is left as it is. A file
    /// that will not go does not hold back the other subjects: they are
    /// removed first, then `CleanupFailed` reports it, left pending for the
    /// next call. Removes at most one batch; `more` says files may remain:
    /// call again, for example from the next sweep.
    pub async fn remove_swept_bytes(
        &self,
        client: &mut Client,
        scope: &Scope,
        grant: &Erasure,
        kind: &str,
    ) -> Result<Removal> {
        grant.check(scope)?;
        if !matches!(grant.cause(), Cause::Sweep(_)) || grant.person().is_some() {
            return Err(Error::Database(bilibi_database::Error::Forbidden));
        }
        let mut removal = Removal::default();
        let mut after = String::new();
        let mut failed = false;
        loop {
            let subjects: Vec<String> = client
                .query(
                    "SELECT DISTINCT subject_id FROM base_media_attachments WHERE tenant_id=$1 AND subject_type=$2 \
                     AND state='erased' AND bytes_removed_at IS NULL AND subject_id>$3 ORDER BY subject_id LIMIT $4",
                    &[&scope.tenant(), &kind, &after, &SWEPT_PAGE],
                )
                .await?
                .iter()
                .map(|row| row.get(0))
                .collect();
            for id in &subjects {
                after.clone_from(id);
                let subject = AttachmentSubject { kind, id };
                match self
                    .remove_erased_bytes(client, scope, grant, subject)
                    .await
                {
                    Ok(done) => {
                        removal.removed += done.removed;
                        removal.more |= done.more;
                    }
                    // Not this owner's sweep: another path removes its bytes.
                    Err(Error::Database(bilibi_database::Error::Forbidden)) => {}
                    Err(Error::CleanupFailed) => failed = true,
                    Err(error) => return Err(error),
                }
                if removal.removed >= CLEANUP_BATCH as u64 {
                    removal.more = true;
                    return Ok(removal);
                }
            }
            if (subjects.len() as i64) < SWEPT_PAGE {
                return if failed {
                    Err(Error::CleanupFailed)
                } else {
                    Ok(removal)
                };
            }
        }
    }

    /// Erasure (ADR 05): turn every live attachment of `subject` that no hold
    /// names (on the attachment or on its asset) into a tombstone and remove
    /// its asset row, in the caller's
    /// erasure transaction. The grant is the authority, so no `SubjectAction`
    /// is asked; the database admits the change only through
    /// `base_media_erase_attachment`. The asset's change records its filename
    /// as `[erased]`; the caller clears the earlier evidence of both records.
    /// Bytes go afterwards, through [`Media::remove_erased_bytes`]. Repeating
    /// it finds nothing left to erase.
    pub async fn erase_attachments(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        grant: &Erasure,
        subject: AttachmentSubject<'_>,
        holds: &Holds,
    ) -> Result<AttachmentErasure> {
        if tx.context().tenant != scope.tenant() {
            return Err(Error::Database(bilibi_database::Error::Forbidden));
        }
        self.erased_subject(tx.sql(), scope, grant, subject).await?;
        let rows = tx
            .sql()
            .query(
                &format!("{SELECT} WHERE a.tenant_id=$1 AND a.subject_type=$2 AND a.subject_id=$3 ORDER BY a.id FOR UPDATE OF a"),
                &[&scope.tenant(), &subject.kind, &subject.id],
            )
            .await?;
        let rows = rows.iter().map(bound).collect::<Result<Vec<_>>>()?;
        let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
        let assets: Vec<i64> = rows.iter().map(|row| row.asset_id).collect();
        // A hold on the attachment or on its asset keeps both, and the bytes.
        let held = holds.records(grant, ATTACHMENT_KIND, &ids).await?;
        let held_assets = holds.records(grant, ASSET_KIND, &assets).await?;
        let mut done = AttachmentErasure::default();
        for row in rows {
            if let Some(hold) = held.get(&row.id).or_else(|| held_assets.get(&row.asset_id)) {
                done.held.push((row.id, *hold));
                continue;
            }
            let asset = self.fetch(tx.sql(), scope.tenant(), row.asset_id).await?;
            let mut old_asset = audit(&asset);
            if old_asset
                .get("original_filename")
                .is_some_and(|name| !name.is_null())
            {
                old_asset["original_filename"] = json!(bilibi_personal_data::ERASED);
            }
            // A system principal (owed cleanup after a hold) is recorded as no user.
            let user = scope.principal().is_none().then(|| scope.user());
            let (tenant, id, asset_id) = (scope.tenant(), row.id, row.asset_id);
            let changes = vec![
                Change {
                    kind: ATTACHMENT_KIND.into(),
                    id,
                    old: facts(&row.summary),
                    new: json!({"public_id": row.summary.public_id, "subject_type": row.summary.subject_type,
                        "subject_id": row.summary.subject_id, "state": "erased", "uploaded_by_id": row.summary.uploaded_by.user(),
                        "uploaded_by_principal": row.summary.uploaded_by.principal()}),
                },
                Change {
                    kind: ASSET_KIND.into(),
                    id: asset_id,
                    old: old_asset,
                    new: json!({}),
                },
            ];
            tx.mutate(move |sql| {
                Box::pin(async move {
                    sql.execute(
                        "SELECT base_media_erase_attachment($1,$2,$3)",
                        &[&tenant, &id, &user],
                    )
                    .await?;
                    Ok(((), changes))
                })
            })
            .await?;
            done.erased.push(ErasedAttachment {
                public_id: row.summary.public_id.clone(),
                id,
                asset_id,
            });
        }
        Ok(done)
    }

    /// Removes the bytes of `subject`'s erased attachments, one file per
    /// transaction: each file is claimed under its row lock, removed, and
    /// recorded once, so concurrent calls never remove or record one file
    /// twice. A file already gone counts as removed. Removes at most one batch;
    /// `more` says files remain, and a file that will not go is
    /// `CleanupFailed`, left pending for the next call.
    pub async fn remove_erased_bytes(
        &self,
        client: &mut Client,
        scope: &Scope,
        grant: &Erasure,
        subject: AttachmentSubject<'_>,
    ) -> Result<Removal> {
        self.erased_subject(&*client, scope, grant, subject).await?;
        let roots = self.disks(&*client).await?;
        let mut removal = Removal::default();
        for _ in 0..CLEANUP_BATCH {
            let mut tx = self
                .boundary
                .begin(
                    client,
                    scope.mutation_context("media.attachment.bytes_removed"),
                )
                .await?;
            let Some(row) = tx
                .sql()
                .query_opt(
                    "SELECT id,erased_disk,erased_key FROM base_media_attachments WHERE tenant_id=$1 AND subject_type=$2 AND subject_id=$3 \
                     AND state='erased' AND bytes_removed_at IS NULL ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED",
                    &[&scope.tenant(), &subject.kind, &subject.id],
                )
                .await?
            else {
                tx.rollback().await?;
                removal.more = client
                    .query_one(
                        "SELECT EXISTS (SELECT 1 FROM base_media_attachments WHERE tenant_id=$1 AND subject_type=$2 AND subject_id=$3 \
                         AND state='erased' AND bytes_removed_at IS NULL)",
                        &[&scope.tenant(), &subject.kind, &subject.id],
                    )
                    .await?
                    .get(0);
                return Ok(removal);
            };
            let (id, disk, key): (i64, String, String) = (row.get(0), row.get(1), row.get(2));
            if storage::delete(&roots, &disk, &key).await.is_err() {
                tx.rollback().await?;
                return Err(Error::CleanupFailed);
            }
            tx.mutate(move |sql| {
                Box::pin(async move {
                    sql.execute(
                        "UPDATE base_media_attachments SET bytes_removed_at=now() WHERE id=$1 AND bytes_removed_at IS NULL",
                        &[&id],
                    )
                    .await?;
                    let change = Change {
                        kind: ATTACHMENT_KIND.into(),
                        id,
                        old: json!({"bytes": "present"}),
                        new: json!({"bytes": "removed"}),
                    };
                    Ok(((), vec![change]))
                })
            })
            .await?;
            tx.commit().await?;
            removal.removed += 1;
        }
        removal.more = true;
        Ok(removal)
    }
}
