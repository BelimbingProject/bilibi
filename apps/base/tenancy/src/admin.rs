//! Tenant administration (`list_tenants/0`,
//! `count_tenants/0`, `fetch_tenant/1`, `create_tenant/1`): live tenants as
//! stable identities, the audited fresh-tenant insert and the audited
//! suspend and reactivate moves. Each read requires an explicit OperatorProof;
//! `create_in` and `move_in` recheck the capability and operator identity on the
//! caller's transaction through the bound authorizer.
use crate::{prove_operator, require_operator_marker, Authorization, OperatorProof, Scope};
use bilibi_database::{Change, Db, MutationTransaction, Result};
use serde_json::json;
use std::sync::Arc;
use tokio_postgres::{GenericClient, Row};

/// Platform capability: see the tenant list (`admin.tenancy.tenant.list`).
pub const LIST: &str = "admin.tenancy.tenant.list";
/// Platform capability: create a tenant (`admin.tenancy.tenant.create`).
pub const CREATE: &str = "admin.tenancy.tenant.create";
/// Platform capability: suspend a tenant (`admin.tenancy.tenant.suspend`).
pub const SUSPEND: &str = "admin.tenancy.tenant.suspend";
/// Platform capability: reactivate a suspended tenant
/// (`admin.tenancy.tenant.reactivate`).
pub const REACTIVATE: &str = "admin.tenancy.tenant.reactivate";
/// The longest name a tenant takes, as the source validates.
pub const NAME_MAX: usize = 255;
/// The longest reason a suspension or reactivation takes.
pub const REASON_MAX: usize = 500;

/// A tenant's standing. Authentication only ever admits an `active` tenant
/// (`Authentication::authenticate`). `base/workflow` depends on this module, so
/// the lifecycle is this enum and the one [`TenantStatus::after`] function,
/// never a workflow definition, and the database CHECK on `tenants.status`
/// lists exactly these values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TenantStatus {
    Active,
    Suspended,
}
impl TenantStatus {
    pub const ALL: [TenantStatus; 2] = [TenantStatus::Active, TenantStatus::Suspended];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Suspended => "Suspended",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == value)
    }
    /// The one transition function: where `change` takes a tenant that stands
    /// at `self`, or `None` when the move is not allowed from here.
    pub fn after(self, change: Move) -> Option<Self> {
        match (self, change) {
            (Self::Active, Move::Suspend) => Some(Self::Suspended),
            (Self::Suspended, Move::Reactivate) => Some(Self::Active),
            _ => None,
        }
    }
}

/// A move between the two statuses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Move {
    Suspend,
    Reactivate,
}
impl Move {
    /// The platform capability the move needs.
    pub fn capability(self) -> &'static str {
        match self {
            Self::Suspend => SUSPEND,
            Self::Reactivate => REACTIVATE,
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "suspend" => Some(Self::Suspend),
            "reactivate" => Some(Self::Reactivate),
            _ => None,
        }
    }
}

/// Why and when a tenant is suspended; present exactly while it is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Suspension {
    /// RFC 3339 instant.
    pub at: String,
    pub reason: String,
}

/// Stable tenant read model: callers receive
/// tenant meaning, never the row. `is_platform_operator` is the explicit
/// `platform_operator` marker, read with the row, so it can never be inferred
/// from the id.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenantIdentity {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub status: TenantStatus,
    pub is_platform_operator: bool,
    /// Set exactly while `status` is `Suspended`.
    pub suspension: Option<Suspension>,
}
impl TenantIdentity {
    fn from_row(row: &Row) -> Self {
        let at: Option<String> = row.get(5);
        let reason: Option<String> = row.get(6);
        Self {
            id: row.get(0),
            parent_id: row.get(1),
            name: row.get(2),
            // The CHECK on `tenants.status` admits only known values; one
            // that is not known reads as suspended, never as active.
            status: TenantStatus::parse(row.get(3)).unwrap_or(TenantStatus::Suspended),
            is_platform_operator: row.get(4),
            suspension: at.zip(reason).map(|(at, reason)| Suspension { at, reason }),
        }
    }
}

/// What a create submits, before validation. A tenant is always created
/// active; suspending it is a separate, reasoned move.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NewTenant {
    pub name: String,
    pub parent_id: Option<i64>,
}

/// Field errors a create was refused with, in the source's wording, for the
/// form to show in place. A refusal is an outcome, never an `Error`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TenantErrors {
    pub name: Vec<String>,
    pub parent_id: Vec<String>,
}
impl TenantErrors {
    pub fn is_empty(&self) -> bool {
        self.name.is_empty() && self.parent_id.is_empty()
    }
}

#[derive(Debug)]
pub enum CreateOutcome {
    Created(CreatedTenant),
    Refused(TenantErrors),
}

/// Why a suspend or reactivate was refused, in the caller's terms. A refusal
/// is an outcome, never an `Error`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MoveRefusal {
    /// No live tenant has this id.
    NotFound,
    /// The platform operator's own tenant cannot be suspended: nobody could
    /// reactivate it, and every operator action would refuse.
    Operator,
    /// The tenant already stands where the move would take it (or at a status
    /// the move does not leave).
    NotAllowed(TenantStatus),
    /// The reason is blank or too long; the text is a field error.
    Reason(String),
}

/// A move checked against the tenant as it stands, before any write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MovePlan {
    pub tenant: TenantIdentity,
    pub to: TenantStatus,
    /// The trimmed reason the move will record.
    pub reason: String,
}

const SELECT: &str = "SELECT t.id, t.parent_id, t.name, t.status, (o.tenant_id IS NOT NULL), \
 to_jsonb(t.suspended_at)#>>'{}', t.suspension_reason \
 FROM tenants t LEFT JOIN platform_operator o ON o.singleton AND o.tenant_id = t.id \
 WHERE t.deleted_at IS NULL";

/// Live tenants ordered by name then id, on the caller's connection or
/// transaction. Soft-deleted tenants are omitted.
pub async fn list_tenants(
    client: &impl GenericClient,
    _proof: &OperatorProof,
) -> Result<Vec<TenantIdentity>> {
    Ok(client
        .query(&format!("{SELECT} ORDER BY t.name, t.id"), &[])
        .await?
        .iter()
        .map(TenantIdentity::from_row)
        .collect())
}

/// Checks `change` for `tenant` as it stands now: the only place the rules of
/// the move live, shared by the preview and the write.
fn plan_move(
    tenant: Option<TenantIdentity>,
    change: Move,
    reason: &str,
) -> std::result::Result<MovePlan, MoveRefusal> {
    let tenant = tenant.ok_or(MoveRefusal::NotFound)?;
    if change == Move::Suspend && tenant.is_platform_operator {
        return Err(MoveRefusal::Operator);
    }
    let to = tenant
        .status
        .after(change)
        .ok_or(MoveRefusal::NotAllowed(tenant.status))?;
    let reason = reason.trim().to_owned();
    if reason.is_empty() {
        return Err(MoveRefusal::Reason("can't be blank".into()));
    }
    if reason.chars().count() > REASON_MAX {
        return Err(MoveRefusal::Reason(format!(
            "should be at most {REASON_MAX} character(s)"
        )));
    }
    Ok(MovePlan { tenant, to, reason })
}

/// Validation the source's changeset performs, before any SQL. The parent's
/// existence is proven on the write's transaction, not here.
fn validate(new: &NewTenant) -> (NewTenant, TenantErrors) {
    let mut errors = TenantErrors::default();
    let name = new.name.trim().to_owned();
    if name.is_empty() {
        errors.name.push("can't be blank".into());
    } else if name.chars().count() > NAME_MAX {
        errors
            .name
            .push(format!("should be at most {NAME_MAX} character(s)"));
    }
    if new.parent_id.is_some_and(|id| id <= 0) {
        errors.parent_id.push("is invalid".into());
    }
    (
        NewTenant {
            name,
            parent_id: new.parent_id,
        },
        errors,
    )
}

/// Deployment seams for Core's provisioning commands. They write on the caller's transaction with no operator proof, so
/// only trusted deployment code calls them; a request path uses
/// `Tenants::create_in`. The name is validated as a create validates it and an
/// invalid one is `Error::Invalid`.
pub async fn provision_tenant(client: &impl GenericClient, name: &str) -> Result<TenantIdentity> {
    let (new, errors) = validate(&NewTenant {
        name: name.to_owned(),
        parent_id: None,
    });
    if !errors.is_empty() {
        return Err(bilibi_database::Error::Invalid);
    }
    let row = client
        .query_one(
            "INSERT INTO tenants(name, parent_id, status, created_at, updated_at) \
             VALUES($1, NULL, 'active', now(), now()) RETURNING id",
            &[&new.name],
        )
        .await?;
    Ok(TenantIdentity {
        id: row.get(0),
        parent_id: None,
        name: new.name,
        status: TenantStatus::Active,
        is_platform_operator: false,
        suspension: None,
    })
}

/// The explicitly marked platform-operator tenant, or `None` until
/// `platform-provision`, `create-owner` or the development seed marks one.
pub async fn platform_operator(client: &impl GenericClient) -> Result<Option<TenantIdentity>> {
    Ok(client
        .query_opt(&format!("{SELECT} AND o.tenant_id IS NOT NULL"), &[])
        .await?
        .map(|row| TenantIdentity::from_row(&row)))
}

/// The explicit platform-operator tenant: resolved when the marker exists
/// (`false`), created and marked otherwise (`true`). The marker table is
/// locked for the transaction, so a second operator is never created.
pub async fn provision_platform_operator(
    client: &impl GenericClient,
    name: &str,
) -> Result<(TenantIdentity, bool)> {
    client
        .execute(
            "LOCK TABLE platform_operator IN SHARE ROW EXCLUSIVE MODE",
            &[],
        )
        .await?;
    if let Some(row) = client
        .query_opt(&format!("{SELECT} AND o.tenant_id IS NOT NULL"), &[])
        .await?
    {
        return Ok((TenantIdentity::from_row(&row), false));
    }
    let mut tenant = provision_tenant(client, name).await?;
    client
        .execute(
            "INSERT INTO platform_operator(singleton, tenant_id) VALUES(true, $1)",
            &[&tenant.id],
        )
        .await?;
    tenant.is_platform_operator = true;
    Ok((tenant, true))
}

/// The most tenants one [`Tenants::principal_page`] returns.
pub const PRINCIPAL_PAGE_MAX: i64 = 200;

/// One page of the directory for a named system principal. `through_id` is the
/// high-water mark the page was read against: pass it back for every later page
/// so a tenant created meanwhile is never part of the walk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenantPage {
    pub through_id: i64,
    pub tenants: Vec<TenantIdentity>,
}

/// The registry service `base/tenancy.tenants`.
pub struct Tenants {
    db: Db,
    authorization: Arc<Authorization>,
}
impl Tenants {
    pub fn new(db: Db, authorization: Arc<Authorization>) -> Self {
        Self { db, authorization }
    }
    pub async fn list(&self, proof: &OperatorProof) -> Result<Vec<TenantIdentity>> {
        let client = self.db.get().await?;
        let proof = prove_operator(&**client, proof.scope()).await?;
        list_tenants(&**client, &proof).await
    }
    /// Live tenants in id order for a worker running as a named system
    /// principal in the operator tenant (per-tenant fan-out). A system
    /// principal cannot hold an `OperatorProof`, so this decides `LIST` on the
    /// principal's own sealed scope: the operator marker, the principal's
    /// declaration and its durable platform grant are all required, on this
    /// call, and the decision is logged. `after_id` is exclusive. `through_id`
    /// `None` reads the current high-water mark; pass the returned value back
    /// on later pages. `limit` is `1..=200`.
    pub async fn principal_page(
        &self,
        scope: &Scope,
        after_id: i64,
        through_id: Option<i64>,
        limit: i64,
    ) -> Result<TenantPage> {
        if scope.principal().is_none() {
            return Err(bilibi_database::Error::Forbidden);
        }
        if !(1..=PRINCIPAL_PAGE_MAX).contains(&limit)
            || after_id < 0
            || through_id.is_some_and(|id| id <= 0)
        {
            return Err(bilibi_database::Error::Invalid);
        }
        let client = self.db.get().await?;
        require_operator_marker(&**client, scope.tenant()).await?;
        self.authorization
            .authorize_scope(&**client, scope, LIST)
            .await?;
        let through_id = match through_id {
            Some(id) => id,
            None => client
                .query_one(
                    "SELECT COALESCE(max(id),0)::bigint FROM tenants WHERE deleted_at IS NULL",
                    &[],
                )
                .await?
                .get(0),
        };
        let rows = client
            .query(
                &format!("{SELECT} AND t.id > $1 AND t.id <= $2 ORDER BY t.id LIMIT $3"),
                &[&after_id, &through_id, &limit],
            )
            .await?;
        Ok(TenantPage {
            through_id,
            tenants: rows.iter().map(TenantIdentity::from_row).collect(),
        })
    }
    pub async fn count(&self, proof: &OperatorProof) -> Result<u64> {
        let client = self.db.get().await?;
        prove_operator(&**client, proof.scope()).await?;
        let count: i64 = client
            .query_one("SELECT count(*) FROM tenants WHERE deleted_at IS NULL", &[])
            .await?
            .get(0);
        Ok(count.unsigned_abs())
    }
    pub async fn fetch(&self, proof: &OperatorProof, id: i64) -> Result<Option<TenantIdentity>> {
        if id <= 0 {
            return Ok(None);
        }
        let client = self.db.get().await?;
        prove_operator(&**client, proof.scope()).await?;
        Ok(client
            .query_opt(&format!("{SELECT} AND t.id = $1"), &[&id])
            .await?
            .as_ref()
            .map(TenantIdentity::from_row))
    }
    /// Validates the draft and parent without a business write.
    pub async fn preview(
        &self,
        proof: &OperatorProof,
        new: &NewTenant,
    ) -> Result<std::result::Result<NewTenant, TenantErrors>> {
        let client = self.db.get().await?;
        prove_operator(&**client, proof.scope()).await?;
        validate_on(&**client, new, false).await
    }
    /// Creates only a fresh tenant on the caller's audited transaction. The
    /// capability and live operator identity are rechecked here. Its sealed
    /// receipt permits Company to provision that new tenant on this transaction;
    /// it cannot target an existing tenant or change the person's Scope.
    pub async fn create_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        new: &NewTenant,
    ) -> Result<CreateOutcome> {
        self.authorization
            .authorize_operator_in(tx.sql(), scope, CREATE)
            .await?;
        let new = match validate_on(tx.sql(), new, true).await? {
            Ok(new) => new,
            Err(errors) => return Ok(CreateOutcome::Refused(errors)),
        };
        let created = tx.mutate(move |sql| Box::pin(async move {
            let row = sql.query_one(
                "INSERT INTO tenants(name,parent_id,status,created_at,updated_at) VALUES($1,$2,'active',now(),now()) RETURNING id",
                &[&new.name, &new.parent_id],
            ).await?;
            let id = row.get(0);
            let identity = TenantIdentity { id, parent_id:new.parent_id, name:new.name.clone(), status:TenantStatus::Active, is_platform_operator:false, suspension:None };
            Ok((identity, vec![Change { kind:"tenancy.tenant".into(), id, old:serde_json::Value::Null,
                new:json!({"name":new.name,"parent_id":new.parent_id,"status":TenantStatus::Active.as_str()}) }]))
        })).await?;
        let transaction: String = tx
            .sql()
            .query_one("SELECT txid_current()::text", &[])
            .await?
            .get(0);
        Ok(CreateOutcome::Created(CreatedTenant {
            identity: created,
            transaction,
            tenant: scope.tenant(),
            actor: scope.user(),
        }))
    }
    /// Checks a suspend or reactivate against the tenant as it stands, with no
    /// write: what the review shows before the person confirms.
    pub async fn preview_move(
        &self,
        proof: &OperatorProof,
        id: i64,
        change: Move,
        reason: &str,
    ) -> Result<std::result::Result<MovePlan, MoveRefusal>> {
        let client = self.db.get().await?;
        prove_operator(&**client, proof.scope()).await?;
        let tenant = if id > 0 {
            client
                .query_opt(&format!("{SELECT} AND t.id = $1"), &[&id])
                .await?
                .as_ref()
                .map(TenantIdentity::from_row)
        } else {
            None
        };
        Ok(plan_move(tenant, change, reason))
    }
    /// Suspends or reactivates one tenant on the caller's audited transaction.
    /// The move's own capability and the live operator identity are rechecked
    /// here, the tenant row is locked, and the reason is required. A suspended
    /// tenant refuses every sign-in, session, agent connection and queued job
    /// from the moment this commits; this write does not end sessions (Core
    /// Tenant does, in the same transaction, through Sessions).
    pub async fn move_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        id: i64,
        change: Move,
        reason: &str,
    ) -> Result<std::result::Result<TenantIdentity, MoveRefusal>> {
        self.authorization
            .authorize_operator_in(tx.sql(), scope, change.capability())
            .await?;
        let tenant = if id > 0 {
            tx.sql()
                .query_opt(&format!("{SELECT} AND t.id = $1 FOR UPDATE OF t"), &[&id])
                .await?
                .as_ref()
                .map(TenantIdentity::from_row)
        } else {
            None
        };
        let plan = match plan_move(tenant, change, reason) {
            Ok(plan) => plan,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let moved = tx.mutate(move |sql| Box::pin(async move {
            sql.execute(
                "UPDATE tenants SET status=$2, \
                 suspended_at=CASE WHEN $2='suspended' THEN now() END, \
                 suspension_reason=CASE WHEN $2='suspended' THEN $3 END, \
                 updated_at=now() WHERE id=$1",
                &[&plan.tenant.id, &plan.to.as_str(), &plan.reason],
            ).await?;
            let moved = TenantIdentity::from_row(
                &sql.query_one(&format!("{SELECT} AND t.id = $1"), &[&plan.tenant.id]).await?,
            );
            let old = match &plan.tenant.suspension {
                Some(suspension) => json!({"status":plan.tenant.status.as_str(),"suspended_at":suspension.at,"suspension_reason":suspension.reason}),
                None => json!({"status":plan.tenant.status.as_str()}),
            };
            Ok((moved, vec![Change { kind:"tenancy.tenant".into(), id:plan.tenant.id, old,
                new:json!({"status":plan.to.as_str(),"reason":plan.reason}) }]))
        })).await?;
        Ok(Ok(moved))
    }
    /// Bounded search of the platform tenant directory. IDs returned here
    /// are data, not authority to enter that tenant.
    pub async fn search(
        &self,
        proof: &OperatorProof,
        search: &str,
        page: i64,
        page_size: i64,
    ) -> Result<(Vec<TenantIdentity>, i64)> {
        let client = self.db.get().await?;
        prove_operator(&**client, proof.scope()).await?;
        let page_size = page_size.clamp(1, 100);
        let offset = (page.clamp(1, 1_000_000) - 1) * page_size;
        let rows = client.query(&format!("{SELECT} AND ($1='' OR strpos(lower(t.name),lower($1))>0) ORDER BY t.name,t.id LIMIT $2 OFFSET $3"), &[&search,&page_size,&offset]).await?;
        let total = client.query_one("SELECT count(*) FROM tenants WHERE deleted_at IS NULL AND ($1='' OR strpos(lower(name),lower($1))>0)", &[&search]).await?.get(0);
        Ok((rows.iter().map(TenantIdentity::from_row).collect(), total))
    }
}

async fn validate_on(
    client: &impl GenericClient,
    new: &NewTenant,
    lock: bool,
) -> Result<std::result::Result<NewTenant, TenantErrors>> {
    let (new, mut errors) = validate(new);
    if let Some(parent) = new.parent_id.filter(|id| *id > 0) {
        let sql = if lock {
            "SELECT id FROM tenants WHERE id=$1 AND deleted_at IS NULL FOR SHARE"
        } else {
            "SELECT id FROM tenants WHERE id=$1 AND deleted_at IS NULL"
        };
        if client.query_opt(sql, &[&parent]).await?.is_none() {
            errors.parent_id.push("does not exist".into());
        }
    }
    Ok(if errors.is_empty() {
        Ok(new)
    } else {
        Err(errors)
    })
}

/// Sealed receipt for a tenant inserted on this transaction. It is not a
/// retargeted Scope: actor and origin tenant remain credential-derived.
#[derive(Debug)]
pub struct CreatedTenant {
    identity: TenantIdentity,
    transaction: String,
    tenant: i64,
    actor: i64,
}
impl CreatedTenant {
    pub fn identity(&self) -> &TenantIdentity {
        &self.identity
    }
    /// Company must call this before provisioning: a saved receipt from any
    /// earlier request or another person's transaction confers no authority.
    pub async fn verify(&self, tx: &MutationTransaction<'_>, scope: &Scope) -> Result<i64> {
        let transaction: String = tx
            .sql()
            .query_one("SELECT txid_current()::text", &[])
            .await?
            .get(0);
        if transaction != self.transaction
            || scope.tenant() != self.tenant
            || scope.user() != self.actor
            || tx.context().tenant != scope.tenant()
            || tx.context().actor.kind != "user"
            || tx.context().actor.id != scope.user().to_string()
        {
            return Err(bilibi_database::Error::Forbidden);
        }
        prove_operator(tx.sql(), scope).await?;
        Ok(self.identity.id)
    }
}
