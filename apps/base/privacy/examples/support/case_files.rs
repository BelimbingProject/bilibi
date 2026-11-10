//! A walkthrough Domain that files attachments on a record about a person (a
//! case), not on the person (#478). Its subject IDs are case IDs, so no store
//! declares the subject person-keyed: the subject owner vouches, from its own
//! table, that a case is the request's person's, and Media's grant paths then
//! reach the case's files for export and erasure. The table lives only in the
//! disposable walkthrough database; `install` creates it.
use bilibi_base::media::{
    AttachmentSubject, Media, Subject as MediaSubject, SubjectAction, SubjectOwner, ASSET_KIND,
    ATTACHMENT_KIND, SUBJECTS,
};
use bilibi_database::{Error, MutationTransaction, Result};
use bilibi_module_registry::{Contribution, Module, Provenance};
use bilibi_party_ids::PersonId;
use bilibi_personal_data::{
    Action, Disclosure, Erased, ErasedRecord, Erasure, Exported, File, FileRef, Handler,
    HandlerFuture, Holds, Kept, KindCount, Located, PersonalData, Progress, Subject, PERSONAL_DATA,
};
use bilibi_tenancy::{ProofClient, ProofFuture, Scope};
use serde_json::json;
use std::sync::{Arc, Mutex, OnceLock};
use tokio_postgres::Client;

pub const MODULE: &str = "wt_cases/cases";
pub const CASE: &str = "wt_cases.case";
const PROVENANCE: Provenance = Provenance {
    module: MODULE,
    source: file!(),
    line: line!(),
};

inventory::submit! { Module {
    id: MODULE, layer: bilibi_module_registry::Layer::Domains,
    dependencies: &["base/media", "base/personal_data"], migrations: &[], services: &[],
} }
inventory::submit! { Contribution::new(SUBJECTS, CASE, PROVENANCE, &MediaSubject { owner: &CASES }) }
inventory::submit! { Contribution::new(PERSONAL_DATA, "wt_cases/cases.files", PROVENANCE,
&PersonalData { label: "Case files (walkthrough)", kinds: &[ATTACHMENT_KIND, ASSET_KIND], person_subjects: &[], handler: &CASES }) }

static MEDIA: OnceLock<Arc<Media>> = OnceLock::new();
/// What the handler saw when it asked for a case that is not the request's
/// person's, printed by the walkthrough.
static REFUSED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Creates the case table and binds Media for the handler.
pub async fn install(client: &Client, media: Arc<Media>) -> std::result::Result<(), Error> {
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS walkthrough_cases (tenant_id bigint NOT NULL, id bigint NOT NULL, \
             person_id bigint NOT NULL, PRIMARY KEY (tenant_id, id))",
        )
        .await?;
    let _ = MEDIA.set(media);
    Ok(())
}
pub fn refused() -> Vec<String> {
    REFUSED.lock().map(|r| r.clone()).unwrap_or_default()
}
fn refuse<T>(
    step: &str,
    case: i64,
    result: std::result::Result<T, bilibi_base::media::Error>,
) -> Result<()> {
    if !matches!(result, Err(bilibi_base::media::Error::Database(Error::Forbidden))) {
        return Err(Error::Internal);
    }
    if let Ok(mut refused) = REFUSED.lock() {
        refused.push(format!("{step} case {case}: Forbidden"));
    }
    Ok(())
}
fn media() -> Result<&'static Media> {
    MEDIA.get().map(|m| &**m).ok_or(Error::Internal)
}
fn media_error(error: bilibi_base::media::Error) -> Error {
    match error {
        bilibi_base::media::Error::Database(error) => error,
        bilibi_base::media::Error::NotFound => Error::NotFound,
        _ => Error::Internal,
    }
}
fn subject(id: &str) -> AttachmentSubject<'_> {
    AttachmentSubject { kind: CASE, id }
}
/// The person's cases, and one case of someone else (the lowest), to show
/// the grant does not reach it.
async fn cases(
    client: &(impl tokio_postgres::GenericClient + Sync),
    scope: &Scope,
    person: PersonId,
) -> Result<(Vec<i64>, Option<i64>)> {
    let own = client
        .query(
            "SELECT id FROM walkthrough_cases WHERE tenant_id=$1 AND person_id=$2 ORDER BY id",
            &[&scope.tenant(), &person.get()],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    let other = client
        .query_opt(
            "SELECT id FROM walkthrough_cases WHERE tenant_id=$1 AND person_id<>$2 ORDER BY id LIMIT 1",
            &[&scope.tenant(), &person.get()],
        )
        .await?
        .map(|row| row.get(0));
    Ok((own, other))
}

struct Cases;
static CASES: Cases = Cases;

impl SubjectOwner for Cases {
    fn authorize<'a>(
        &'a self,
        client: &'a dyn ProofClient,
        scope: &'a Scope,
        _action: SubjectAction,
        subject_id: &'a str,
    ) -> ProofFuture<'a, ()> {
        Box::pin(async move {
            let id: i64 = subject_id.parse().map_err(|_| Error::NotFound)?;
            client
                .query_opt(
                    "SELECT 1 FROM walkthrough_cases WHERE tenant_id=$1 AND id=$2",
                    &[&scope.tenant(), &id],
                )
                .await?
                .ok_or(Error::NotFound)?;
            Ok(())
        })
    }
    /// A case belongs to the person it is about.
    fn person_subject<'a>(
        &'a self,
        client: &'a dyn ProofClient,
        scope: &'a Scope,
        person: PersonId,
        subject_id: &'a str,
    ) -> ProofFuture<'a, bool> {
        Box::pin(async move {
            let Ok(id) = subject_id.parse::<i64>() else {
                return Ok(false);
            };
            Ok(client
                .query_opt(
                    "SELECT 1 FROM walkthrough_cases WHERE tenant_id=$1 AND id=$2 AND person_id=$3",
                    &[&scope.tenant(), &id, &person.get()],
                )
                .await?
                .is_some())
        })
    }
}

impl Handler for Cases {
    fn locate<'a>(
        &'a self,
        client: &'a Client,
        scope: &'a Scope,
        subject: &'a Subject,
    ) -> HandlerFuture<'a, Located> {
        Box::pin(async move {
            let (own, _) = cases(client, scope, subject.person).await?;
            let mut files = 0;
            for case in own {
                let id = case.to_string();
                files += media()?
                    .count_attachments(client, scope, self::subject(&id))
                    .await
                    .map_err(media_error)?;
            }
            Ok(Located {
                kinds: vec![KindCount {
                    kind: ATTACHMENT_KIND.into(),
                    by_person: files,
                    by_identifier: 0,
                }],
                ..Located::default()
            })
        })
    }
    fn export<'a>(
        &'a self,
        client: &'a Client,
        scope: &'a Scope,
        subject: &'a Subject,
        grant: &'a Disclosure,
    ) -> HandlerFuture<'a, Exported> {
        Box::pin(async move {
            grant.check_subject(scope, subject)?;
            let (own, other) = cases(client, scope, subject.person).await?;
            if let Some(other) = other {
                let id = other.to_string();
                let result = media()?
                    .disclosed_attachments(client, scope, grant, self::subject(&id))
                    .await;
                refuse("export list", other, result)?;
                let public_id: String = client
                    .query_one(
                        "SELECT public_id FROM base_media_attachments WHERE tenant_id=$1 AND subject_type=$2 AND subject_id=$3 ORDER BY id LIMIT 1",
                        &[&scope.tenant(), &CASE, &id],
                    )
                    .await?
                    .get(0);
                refuse(
                    "export file",
                    other,
                    media()?
                        .disclosed_attachment(client, scope, grant, self::subject(&id), &public_id)
                        .await,
                )?;
            }
            let (mut records, mut files) = (Vec::new(), Vec::new());
            for case in own {
                let id = case.to_string();
                for file in media()?
                    .disclosed_attachments(client, scope, grant, self::subject(&id))
                    .await
                    .map_err(media_error)?
                {
                    records.push(json!({"case": case, "public_id": file.public_id, "filename": file.original_filename}));
                    files.push(FileRef {
                        reference: format!("{case}:{}", file.public_id),
                        name: file.original_filename.unwrap_or(file.public_id),
                        content_type: file
                            .mime_type
                            .unwrap_or_else(|| "application/octet-stream".into()),
                    });
                }
            }
            Ok(Exported {
                records: json!({ "case_files": records }),
                files,
            })
        })
    }
    fn file<'a>(
        &'a self,
        client: &'a mut Client,
        scope: &'a Scope,
        grant: &'a Disclosure,
        reference: &'a str,
    ) -> HandlerFuture<'a, File> {
        Box::pin(async move {
            let (case, public_id) = reference.split_once(':').ok_or(Error::NotFound)?;
            let file = media()?
                .disclosed_attachment(&*client, scope, grant, self::subject(case), public_id)
                .await
                .map_err(media_error)?;
            Ok(File {
                content_type: file
                    .mime_type
                    .unwrap_or_else(|| "application/octet-stream".into()),
                bytes: file.bytes,
            })
        })
    }
    fn erase<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        subject: &'a Subject,
        grant: &'a Erasure,
        holds: &'a Holds,
    ) -> HandlerFuture<'a, Erased> {
        Box::pin(async move {
            grant.check_subject(scope, subject)?;
            let (own, other) = cases(tx.sql(), scope, subject.person).await?;
            if let Some(other) = other {
                let id = other.to_string();
                let result = media()?
                    .erase_attachments(tx, scope, grant, self::subject(&id), holds)
                    .await;
                refuse("erase", other, result)?;
            }
            let mut erased = Erased::default();
            for case in own {
                let id = case.to_string();
                let files = media()?
                    .erase_attachments(tx, scope, grant, self::subject(&id), holds)
                    .await
                    .map_err(media_error)?;
                for (attachment, hold) in files.held {
                    erased
                        .kept
                        .push(Kept::held(ATTACHMENT_KIND, attachment, hold));
                }
                erased.count(ATTACHMENT_KIND, Action::Deleted, files.erased.len() as i64);
                for file in files.erased {
                    for (kind, id) in [(ATTACHMENT_KIND, file.id), (ASSET_KIND, file.asset_id)] {
                        erased.records.push(ErasedRecord {
                            kind: kind.into(),
                            id,
                            person: None,
                        });
                    }
                }
            }
            Ok(erased)
        })
    }
    fn after_erase<'a>(
        &'a self,
        client: &'a mut Client,
        scope: &'a Scope,
        subject: &'a Subject,
        grant: &'a Erasure,
        _holds: &'a Holds,
        _progress: &'a dyn Progress,
    ) -> HandlerFuture<'a, ()> {
        Box::pin(async move {
            grant.check_subject(scope, subject)?;
            let (own, other) = cases(&*client, scope, subject.person).await?;
            if let Some(other) = other {
                let id = other.to_string();
                let result = media()?
                    .remove_erased_bytes(client, scope, grant, self::subject(&id))
                    .await;
                refuse("remove bytes", other, result)?;
            }
            for case in own {
                let id = case.to_string();
                loop {
                    let removal = media()?
                        .remove_erased_bytes(client, scope, grant, self::subject(&id))
                        .await
                        .map_err(media_error)?;
                    if !removal.more {
                        break;
                    }
                    if removal.removed == 0 {
                        return Err(Error::Conflict);
                    }
                }
            }
            Ok(())
        })
    }
}
