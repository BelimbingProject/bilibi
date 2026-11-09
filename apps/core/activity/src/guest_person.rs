//! Create a person from the guest panel: an email-address guest that matches
//! no Core person becomes a person (`core/person`), and the guest row points
//! at them instead of the address. One transaction, under the actor's own
//! right to create people; the guest keeps their answer and invitation
//! state. README, Invitations.
use crate::store::{editable_now, guest, guest_row, guest_values, Store, GUEST_COLUMNS};
use crate::{FieldError, GuestRow, WriteError, WriteResult, GUEST_RECORD};
use bilibi_database::{Change, Error, MutationTransaction};
use bilibi_person::{ContactAddress, PersonId, PersonInput, PersonKind};
use bilibi_tenancy::{ReadClient, Scope};

/// What the person would be: the guest's address and the name given.
pub(crate) fn input(row: &GuestRow, name: &str) -> WriteResult<PersonInput> {
    let invalid = |field, message: &str| WriteError::Invalid(vec![FieldError::new(field, message)]);
    if row.user_id.is_some() || row.person.is_some() {
        return Err(invalid("guest_id", "is not an email address guest"));
    }
    let email = row
        .email
        .clone()
        .ok_or_else(|| invalid("guest_id", "is not an email address guest"))?;
    let name = name.trim();
    if name.is_empty() {
        return Err(invalid("name", "can't be blank"));
    }
    Ok(PersonInput {
        name: name.to_owned(),
        kind: PersonKind::Human,
        email: Some(email),
        phone: None,
    })
}

/// A person write's refusal as this module's: a field of the person is a
/// field of this input.
fn refused(error: bilibi_person::WriteError) -> WriteError {
    match error {
        bilibi_person::WriteError::Backend(error) => WriteError::Access(error),
        bilibi_person::WriteError::Validation(errors) => WriteError::Invalid(
            errors
                .into_iter()
                .map(|e| {
                    FieldError::new(
                        if e.field == "email" {
                            "guest_id"
                        } else {
                            "name"
                        },
                        e.message,
                    )
                })
                .collect(),
        ),
        _ => WriteError::Invalid(vec![FieldError::new("name", "cannot be created")]),
    }
}

impl Store<'_> {
    /// The checks `create_person_guest` makes that need no write: the
    /// activity is one the actor may change, the guest is an address, and the
    /// actor may create the person.
    pub(crate) async fn prepare_person_guest(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        id: i64,
        guest_id: i64,
        name: &str,
    ) -> WriteResult<(crate::Activity, GuestRow, PersonInput)> {
        let activity = editable_now(client, scope, id, false).await?;
        let row = guest(client, scope, id, guest_id).await?;
        let person = input(&row, name)?;
        bilibi_person::preview_person(client, scope, &person)
            .await
            .map_err(refused)?;
        Ok((activity, row, person))
    }

    /// Creates the person and moves the guest onto them.
    pub(crate) async fn create_person_guest(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        guest_id: i64,
        name: &str,
    ) -> WriteResult<GuestRow> {
        let activity = editable_now(tx.sql(), scope, id, true).await?;
        let before = guest(tx.sql(), scope, id, guest_id).await?;
        let person = input(&before, name)?;
        let address = ContactAddress::Email(person.email.clone().unwrap_or_default());
        // A person who already has this address is the guest to pick, not a
        // second person with it. The check sees a person this transaction
        // already created.
        let matches = self
            .people
            .match_contact_in(tx.sql(), scope, &address, 1)
            .await?;
        if !matches.entries.is_empty() || !matches.complete() {
            return Err(WriteError::Invalid(vec![FieldError::new(
                "guest_id",
                "matches a person already; add that person as a guest instead",
            )]));
        }
        let created = bilibi_person::create_person(tx, scope, &person)
            .await
            .map_err(refused)?;
        let (tenant, activity_id) = (scope.tenant(), activity.id);
        let before_values = guest_values(&before, activity_id);
        let person_id: PersonId = created.id;
        let after = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let row = sql
                        .query_opt(
                            &format!("UPDATE core_activity_guests SET guest_person_id=$4, guest_email=NULL WHERE tenant_id=$1 AND activity_id=$2 AND id=$3 AND guest_email IS NOT NULL RETURNING {GUEST_COLUMNS}"),
                            &[&tenant, &activity_id, &guest_id, &person_id.get()],
                        )
                        .await?
                        .ok_or(Error::Conflict)?;
                    let after = guest_row(&row)?;
                    let change = Change {
                        kind: GUEST_RECORD.into(),
                        id: after.id,
                        old: before_values,
                        new: guest_values(&after, activity_id),
                    };
                    Ok((after, vec![change]))
                })
            })
            .await
            .map_err(WriteError::from)?;
        Ok(after)
    }
}
