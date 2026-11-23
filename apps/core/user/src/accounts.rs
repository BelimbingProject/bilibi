//! Account administration: the tenant-owned writes behind `/users/{id}` and
//! the signed-in person's own profile. Every command takes
//! the sealed scope, re-proves the actor, authorizes its capability inside one
//! `MutationTransaction`, and honours the Company → Person → User lock order
//! for tenant membership changes. Shared identity credentials are never an
//! administrator input.
use crate::{
    contributions::{DELETE, UPDATE},
    identity::{read_account, Account, LinkError},
    password::check_policy,
    reset::normalize_email,
    UserService,
};
use bilibi_database::{Change, Error, MutationTransaction, Result};
use bilibi_security::Scope;
use bilibi_settings::Target;
use serde_json::json;
use std::collections::BTreeSet;
use tokio_postgres::GenericClient;

/// The user-scope setting the profile page edits: `ui.landing_menu_id`, the menu page opened after sign-in.
pub const LANDING_KEY: &str = "ui.landing_menu_id";
pub const NAME_MAX: usize = 255;
const BLANK: &str = "can't be blank";
const EMAIL_FORMAT: &str = "must be an email address";
pub(crate) const EMAIL_TAKEN: &str = "has already been taken";
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    pub field: &'static str,
    pub message: String,
}
pub(crate) fn field_error(field: &'static str, message: impl Into<String>) -> FieldError {
    FieldError {
        field,
        message: message.into(),
    }
}

/// Why an account command did not happen. `Validation` is worded for the
/// field that refused; `Access` is a denial, a missing target or a fault.
#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("Invalid account fields")]
    Validation(Vec<FieldError>),
    #[error("Not found")]
    NotFound,
    #[error("The selected company is archived")]
    Archived,
    #[error("This affiliation belongs to another person")]
    PersonMismatch,
    #[error("has changed since this edit began")]
    Conflict,
    #[error("You cannot remove your own membership")]
    OwnAccount,
    /// The account holds a permission the actor does not
    /// (`bilibi_authz::holds_everything_of`).
    #[error("This account has permissions you do not have")]
    Outranked,
    /// The request comes from an impersonated session: creating, changing
    /// and removing memberships is never done while viewing as someone else.
    #[error("Accounts cannot be changed while viewing as someone else")]
    Impersonating,
    #[error(transparent)]
    Access(#[from] Error),
}
impl From<LinkError> for AccountError {
    fn from(error: LinkError) -> Self {
        match error {
            LinkError::NotFound => Self::NotFound,
            LinkError::Access(error) => Self::Access(error),
        }
    }
}
impl From<bilibi_company::WriteError> for AccountError {
    fn from(error: bilibi_company::WriteError) -> Self {
        match error {
            bilibi_company::WriteError::NotFound => Self::NotFound,
            bilibi_company::WriteError::Archived => Self::Archived,
            bilibi_company::WriteError::Access(error) => Self::Access(error),
            _ => Self::Access(Error::Forbidden),
        }
    }
}
impl From<tokio_postgres::Error> for AccountError {
    fn from(error: tokio_postgres::Error) -> Self {
        Error::from(error).into()
    }
}

/// The tenant-owned fact an administrator edits in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountField {
    Name,
}
impl AccountField {
    pub fn key(self) -> &'static str {
        match self {
            Self::Name => "name",
        }
    }
}

/// A bounded name and a shaped, normalized email.
pub(crate) fn validate_name(name: &str) -> Option<FieldError> {
    let name = name.trim();
    if name.is_empty() {
        Some(field_error("name", BLANK))
    } else if name.chars().count() > NAME_MAX || name.contains('\0') {
        Some(field_error(
            "name",
            format!("should be at most {NAME_MAX} character(s)"),
        ))
    } else {
        None
    }
}
pub(crate) fn validate_email(email: &str) -> std::result::Result<String, FieldError> {
    if email.trim().is_empty() {
        return Err(field_error("email", BLANK));
    }
    let email = normalize_email(email).map_err(|_| field_error("email", EMAIL_FORMAT))?;
    let shaped = email
        .split_once('@')
        .is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty());
    if shaped {
        Ok(email)
    } else {
        Err(field_error("email", EMAIL_FORMAT))
    }
}
pub(crate) fn validate_password(password: &str) -> Option<FieldError> {
    if password.is_empty() {
        Some(field_error("password", BLANK))
    } else {
        check_policy(password)
            .err()
            .map(|violation| field_error("password", violation.to_string()))
    }
}

/// A staff login email is unique platform-wide (`user_identities_staff_email_idx`);
/// the check runs inside the transaction so the index refusal is the backstop.
pub(crate) async fn email_taken(
    client: &(impl GenericClient + Sync),
    email: &str,
    except: Option<i64>,
) -> Result<bool> {
    Ok(client
        .query_opt(
            "SELECT 1 FROM user_identities i WHERE lower(i.email)=$1 AND i.kind='staff' AND i.deleted_at IS NULL AND ($2::bigint IS NULL OR i.id<>(SELECT identity_id FROM users WHERE id=$2))",
            &[&email, &except],
        )
        .await?
        .is_some())
}

/// What an emailed page needs after a profile save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileOutcome {
    pub email_changed: bool,
}

/// One menu page the signed-in account may open: a landing-page choice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandingPage {
    pub id: &'static str,
    pub label: &'static str,
    pub route: &'static str,
}

impl UserService {
    /// Opens the administrative write on one tenant-owned account: checks the
    /// capability, locks the row and verifies its optional version.
    pub(crate) async fn open_account_write(
        &self,
        tx: &MutationTransaction<'_>,
        scope: &Scope,
        user: i64,
        version: Option<i64>,
        capability: &str,
    ) -> std::result::Result<Account, AccountError> {
        bilibi_authz::authorize_key(tx.sql(), scope, capability).await?;
        let account = read_account(tx.sql(), scope, user, true)
            .await?
            .ok_or(AccountError::NotFound)?;
        if version.is_some_and(|version| version != account.version) {
            return Err(AccountError::Conflict);
        }
        Ok(account)
    }

    /// Refuses a write that would hand the actor more than they hold: the
    /// account's credentials, or its removal, stay with someone who holds
    /// everything it holds.
    async fn require_covered(
        tx: &MutationTransaction<'_>,
        scope: &Scope,
        user: i64,
    ) -> std::result::Result<(), AccountError> {
        if bilibi_authz::holds_everything_of(tx.sql(), scope, user).await? {
            Ok(())
        } else {
            Err(AccountError::Outranked)
        }
    }

    /// Replaces a tenant-owned account name. The installation-owned sign-in
    /// address is never an account-administration field.
    pub async fn update_account(
        &self,
        scope: &Scope,
        user: i64,
        version: i64,
        field: AccountField,
        value: &str,
    ) -> std::result::Result<Account, AccountError> {
        if scope.impersonator().is_some() {
            return Err(AccountError::Impersonating);
        }
        let value = match field {
            AccountField::Name => {
                if let Some(error) = validate_name(value) {
                    return Err(AccountError::Validation(vec![error]));
                }
                value.trim().to_owned()
            }
        };
        let mut client = self.db.get().await.map_err(Error::from)?;
        let mut tx = self
            .mutations
            .begin(&mut client, scope.mutation_context("user.update"))
            .await?;
        let scope = self.fresh(tx.sql(), scope).await?;
        let account = self
            .open_account_write(&tx, &scope, user, Some(version), UPDATE)
            .await?;
        let current = account.name.clone();
        if current == value {
            tx.rollback().await?;
            return Ok(account);
        }
        let tenant = scope.tenant();
        let (new_value, old_value) = (value.clone(), current);
        tx.mutate(move |sql| {
            Box::pin(async move {
                sql.execute(
                    "UPDATE users SET name=$3,version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2",
                    &[&tenant, &user, &new_value],
                )
                .await?;
                let changes = vec![Change {
                        kind: "user".into(),
                        id: user,
                        old: json!({"name": old_value}),
                        new: json!({"name": new_value}),
                    }];
                Ok(((), changes))
            })
        })
        .await?;
        let account = read_account(tx.sql(), &scope, user, false)
            .await?
            .ok_or(AccountError::NotFound)?;
        tx.commit().await?;
        Ok(account)
    }

    pub async fn send_password_recovery(
        &self,
        scope: &Scope,
        user: i64,
    ) -> std::result::Result<(), AccountError> {
        if scope.impersonator().is_some() {
            return Err(AccountError::Impersonating);
        }
        let mut client = self.db.get().await.map_err(Error::from)?;
        let tx = self
            .mutations
            .begin(
                &mut client,
                scope.mutation_context("user.recovery_requested"),
            )
            .await?;
        let scope = self.fresh(tx.sql(), scope).await?;
        let account = self
            .open_account_write(&tx, &scope, user, None, UPDATE)
            .await?;
        if account.status != "active" || !account.email_verified {
            return Err(AccountError::Access(Error::Invalid));
        }
        tx.rollback().await?;
        drop(client);
        self.reset_mail()
            .mail_with_reset_link(
                scope.tenant(),
                user,
                "Recover your Bilibi sign-in",
                "Your workspace administrator requested a recovery email for you.",
                true,
            )
            .await?;
        Ok(())
    }

    /// Removes a membership: `admin.user.delete`, never the actor's own nor one
    /// holding a permission the actor lacks. It always retires only this
    /// tenant membership; customer administrators never delete a shared identity.
    pub async fn remove_membership(
        &self,
        scope: &Scope,
        user: i64,
    ) -> std::result::Result<(), AccountError> {
        if scope.impersonator().is_some() {
            return Err(AccountError::Impersonating);
        }
        if scope.user() == user {
            return Err(AccountError::OwnAccount);
        }
        let mut client = self.db.get().await.map_err(Error::from)?;
        let mut tx = self
            .mutations
            .begin(&mut client, scope.mutation_context("user.delete"))
            .await?;
        let scope = self.fresh(tx.sql(), scope).await?;
        let account = self
            .open_account_write(&tx, &scope, user, None, DELETE)
            .await?;
        Self::require_covered(&tx, &scope, user).await?;
        let tenant = scope.tenant();
        tx
            .mutate(move |sql| {
                Box::pin(async move {
                    sql.execute(
                        "UPDATE users SET deleted_at=now(),status='deleted',version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2",
                        &[&tenant, &user],
                    )
                    .await?;
                    let changes = vec![Change {
                        kind: "user".into(),
                        id: user,
                        old: json!({"email": account.email, "deleted_at": null}),
                        new: json!({"email": account.email, "deleted_at": "now"}),
                    }];
                    Ok(((), changes))
                })
            })
            .await?;
        let ended = self.sessions.revoke(&mut tx, user, None).await?;
        let sessions = self.sessions.clone();
        tx.after_commit(move || sessions.announce(ended));
        tx.commit().await?;
        Ok(())
    }

    /// The signed-in account's own name and portable identity email.
    /// The account is the session's, never a submitted id, and no capability
    /// applies. A changed email is unverified again and requires portable
    /// authentication authority.
    pub async fn update_profile(
        &self,
        token: &str,
        csrf: &str,
        name: &str,
        email: &str,
    ) -> std::result::Result<ProfileOutcome, AccountError> {
        let mut errors = Vec::new();
        errors.extend(validate_name(name));
        let email = match validate_email(email) {
            Ok(email) => Some(email),
            Err(error) => {
                errors.push(error);
                None
            }
        };
        if !errors.is_empty() {
            return Err(AccountError::Validation(errors));
        }
        let email = email.expect("validated");
        let name = name.trim().to_owned();
        let mut client = self.db.get().await.map_err(Error::from)?;
        let scope = self.sessions.activity(&**client, token).await?;
        if scope.csrf != csrf {
            return Err(Error::Forbidden.into());
        }
        let mut tx = self
            .mutations
            .begin(&mut client, scope.mutation_context("user.profile"))
            .await?;
        let user = scope.user();
        let account = read_account(tx.sql(), &scope, user, true)
            .await?
            .ok_or(Error::Unauthorized)?;
        let email_changed = account.email != email;
        if account.name == name && !email_changed {
            tx.rollback().await?;
            return Ok(ProfileOutcome { email_changed });
        }
        if email_changed {
            self.sessions
                .require_portable_authority(tx.sql(), &scope)
                .await?;
        }
        if email_changed && email_taken(tx.sql(), &email, Some(user)).await? {
            return Err(AccountError::Validation(vec![field_error(
                "email",
                EMAIL_TAKEN,
            )]));
        }
        let tenant = scope.tenant();
        let identity = crate::sign_in_identity::id(tx.sql(), tenant, user).await?;
        let (old_name, old_email) = (account.name, account.email);
        let (new_name, new_email) = (name, email);
        tx.mutate(move |sql| {
            Box::pin(async move {
                sql.execute(
                    "UPDATE users SET name=$3,version=version+1,updated_at=now() WHERE tenant_id=$1 AND id=$2",
                    &[&tenant, &user, &new_name],
                )
                .await?;
                if email_changed {
                    sql.execute(
                        "UPDATE user_identities SET email=$3,email_verified_at=NULL,updated_at=now() WHERE id=(SELECT identity_id FROM users WHERE tenant_id=$1 AND id=$2)",
                        &[&tenant, &user, &new_email],
                    )
                    .await?;
                }
                let mut changes = vec![Change {
                        kind: "user".into(),
                        id: user,
                        old: json!({"name": old_name.clone()}),
                        new: json!({"name": new_name.clone()}),
                    }];
                if email_changed {
                    changes.push(Change {
                        kind: "user_identity".into(),
                        id: identity,
                        old: json!({"email": old_email}),
                        new: json!({"email": new_email}),
                    });
                }
                Ok(((), changes))
            })
        })
        .await?;
        tx.commit().await?;
        Ok(ProfileOutcome { email_changed })
    }

    /// The menu pages the actor can open right now, in menu order: the only
    /// values the landing-page choice accepts, so a sign-in never lands on a
    /// page that would turn the person straight back out.
    pub async fn landing_pages(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
    ) -> Result<Vec<LandingPage>> {
        let capabilities = bilibi_authz::effective_capabilities(client, scope).await?;
        Ok(reachable_pages(&|key| capabilities.contains(key)))
    }

    /// The stored landing choice of the signed-in account (`""` = default).
    pub async fn landing_choice(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
    ) -> Result<String> {
        let resolved = self
            .settings
            .get(client, scope, Target::User { company: None }, LANDING_KEY)
            .await?;
        Ok(resolved.value.as_str().unwrap_or("").to_owned())
    }

    /// Prepare the account's landing choice using its reachable menu.
    pub(crate) async fn check_landing(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
        value: &str,
    ) -> Result<()> {
        if scope.impersonator().is_some() {
            return Err(Error::Forbidden);
        }
        if !value.is_empty()
            && !self
                .landing_pages(client, scope)
                .await?
                .iter()
                .any(|page| page.id == value)
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }

    pub(crate) async fn lock_landing(
        tx: &bilibi_database::MutationTransaction<'_>,
        scope: &Scope,
    ) -> Result<()> {
        let lock = format!("core/user:landing:{}:{}", scope.tenant(), scope.user());
        tx.sql()
            .execute(
                "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
                &[&lock],
            )
            .await?;
        Ok(())
    }

    pub(crate) async fn apply_landing(
        &self,
        tx: &mut bilibi_database::MutationTransaction<'_>,
        scope: &Scope,
        value: &str,
    ) -> Result<()> {
        self.check_landing(tx.sql(), scope, value).await?;
        let own = Target::User { company: None };
        if value.is_empty() {
            self.settings.delete(tx, scope, own, LANDING_KEY).await?;
        } else if self.landing_choice(tx.sql(), scope).await? != value {
            self.settings
                .put(tx, scope, own, LANDING_KEY, json!(value))
                .await?;
        }
        Ok(())
    }

    /// Store the account's own choice from the profile form.
    pub(crate) async fn save_landing(
        &self,
        scope: &Scope,
        value: &str,
    ) -> std::result::Result<(), AccountError> {
        let mut client = self.db.get().await.map_err(Error::from)?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                scope.mutation_context("user.preference.landing"),
            )
            .await?;
        let scope = self.fresh(tx.sql(), scope).await?;
        Self::lock_landing(&tx, &scope).await?;
        match self.apply_landing(&mut tx, &scope, value).await {
            Err(Error::Invalid) => {
                return Err(AccountError::Validation(vec![field_error(
                    "landing_menu_id",
                    "is not a page you can open",
                )]))
            }
            outcome => outcome?,
        }
        tx.commit().await?;
        Ok(())
    }

    /// The signed-in account's picture as a short-lived signed Media link.
    /// A missing or unreadable asset quietly uses the initials fallback.
    pub async fn profile_image_url(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
    ) -> Option<String> {
        let asset = self
            .settings
            .get(
                client,
                scope,
                Target::User { company: None },
                crate::PROFILE_IMAGE_KEY,
            )
            .await
            .ok()?
            .value
            .as_i64()?;
        self.media.stream_url(client, scope, asset).await.ok()
    }

    /// Stores a replacement for the signed-in account's own picture. Media
    /// inspects the bytes; the browser's filename and content type are hints.
    pub async fn save_profile_image(
        &self,
        scope: &Scope,
        upload: bilibi_media::Upload,
    ) -> std::result::Result<(), bilibi_media::Error> {
        let mut client = self.db.get().await.map_err(Error::from)?;
        let scope = self.fresh(&**client, scope).await?;
        let asset = self
            .media
            .put_public_raster_image(
                &mut client,
                &scope,
                "profile-images",
                upload,
                crate::PROFILE_IMAGE_MAX_BYTES,
            )
            .await?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                scope.mutation_context("user.profile-image.save"),
            )
            .await?;
        self.settings
            .put(
                &mut tx,
                &scope,
                Target::User { company: None },
                crate::PROFILE_IMAGE_KEY,
                json!(asset.id),
            )
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Removes the signed-in account's picture choice. The old Media object is
    /// retained for the same lifecycle reason as a replaced portal logo.
    pub async fn remove_profile_image(
        &self,
        scope: &Scope,
    ) -> std::result::Result<(), bilibi_media::Error> {
        let mut client = self.db.get().await.map_err(Error::from)?;
        let scope = self.fresh(&**client, scope).await?;
        let mut tx = self
            .mutations
            .begin(
                &mut client,
                scope.mutation_context("user.profile-image.remove"),
            )
            .await?;
        self.settings
            .delete(
                &mut tx,
                &scope,
                Target::User { company: None },
                crate::PROFILE_IMAGE_KEY,
            )
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Where a fresh sign-in goes (Web's seam): the chosen landing page when
    /// the account still may open it, else `None` for the host's default.
    pub async fn landing_route(&self, token: &str) -> Result<Option<String>> {
        let client = self.db.get().await?;
        let scope = self.sessions.activity(&**client, token).await?;
        let choice = self.landing_choice(&**client, &scope).await?;
        if choice.is_empty() {
            return Ok(None);
        }
        Ok(self
            .landing_pages(&**client, &scope)
            .await?
            .into_iter()
            .find(|page| page.id == choice)
            .map(|page| page.route.to_owned()))
    }
}

/// Every contributed menu page whose requirement, and whose ancestors'
/// requirements, `decide` allows, in the order the navigation shows them
/// (siblings by `(order, id)`, children under their parent). The same
/// reading as `bilibi_menu::Menu::visible_tree`, over the inventory the
/// catalog is built from, since a module cannot hold the host's installed
/// catalog.
fn reachable_pages(decide: &dyn Fn(&str) -> bool) -> Vec<LandingPage> {
    use bilibi_menu::MenuItem;
    let mut items: Vec<&'static MenuItem> = inventory::iter::<bilibi_module_registry::Contribution>
        .into_iter()
        .filter(|entry| entry.consumer == bilibi_menu::ITEMS)
        .filter_map(|entry| entry.get::<MenuItem>().ok())
        .collect();
    items.sort_by_key(|item| (item.order, item.id));
    let ids: BTreeSet<&str> = items.iter().map(|item| item.id).collect();
    fn walk(
        items: &[&'static bilibi_menu::MenuItem],
        ids: &BTreeSet<&str>,
        parent: Option<&str>,
        depth: usize,
        decide: &dyn Fn(&str) -> bool,
        out: &mut Vec<LandingPage>,
    ) {
        if depth > 16 {
            return;
        }
        for item in items
            .iter()
            .filter(|item| item.parent == parent && item.parent.is_none_or(|id| ids.contains(id)))
        {
            // A landing page is a staff page; the portal menu is never one.
            if !item.requirement.allowed(decide) || item.id == bilibi_menu::PORTAL_ROOT {
                continue;
            }
            if let Some(route) = item.route.filter(|route| route.starts_with('/')) {
                out.push(LandingPage {
                    id: item.id,
                    label: item.label,
                    route,
                });
            }
            walk(items, ids, Some(item.id), depth + 1, decide, out);
        }
    }
    let mut pages = Vec::new();
    walk(&items, &ids, None, 0, decide, &mut pages);
    pages
}
