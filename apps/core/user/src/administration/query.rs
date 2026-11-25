//! The one bounded read: tenant staff accounts, plus portal customers only
//! while they have an open 2FA reset request, filtered, counted, ordered and paged in one PostgreSQL
//! statement. Every physical
//! source is named here and nowhere else in this crate; nothing is written.
use super::{Entry, Membership, Options, Page, Role, Sort};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use tokio_postgres::{types::ToSql, GenericClient};

/// Fixed SQL for each sort; every ordering ends with the account id
/// descending so equal values page stably.
fn order(sort: Sort, descending: bool, prefix: &str) -> String {
    let column = match sort {
        Sort::Name => format!("lower({prefix}name)"),
        Sort::Email => format!("lower({prefix}email)"),
        Sort::Desks => format!("lower({prefix}desks)"),
        Sort::CreatedAt => format!("{prefix}created_at"),
    };
    let direction = if descending { "DESC" } else { "ASC" };
    format!("{column} {direction} NULLS LAST, {prefix}id DESC")
}

/// `%`, `_` and `\` in the typed text match themselves (Bilibi's lists
/// search for what was typed).
fn escape_like(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

const STATEMENT: &str = "WITH filtered_users AS (
  SELECT u.id,u.name,COALESCE(i.email,v.email) AS email,u.created_at,
    u.status AS membership,
    v.id AS invitation_id,
    (SELECT max(r.requested_at) FROM user_two_factor_reset_requests r WHERE r.tenant_id=u.tenant_id AND r.user_id=u.id AND r.closed_at IS NULL) AS reset_requested_at,
    EXISTS(SELECT 1 FROM user_two_factor f WHERE f.identity_id=u.identity_id AND f.confirmed_at IS NOT NULL) AS two_factor_enabled,
    EXISTS(SELECT 1 FROM users owner JOIN user_bootstrap b ON b.tenant_id=owner.tenant_id AND b.user_id=owner.id WHERE owner.identity_id=u.identity_id) AS installation_owner,
    u.kind='customer' AS reset_request_only,
    (SELECT string_agg(c.name || CASE WHEN c.status='archived' THEN ' (archived)' ELSE '' END,', ' ORDER BY c.name) FROM user_desks d JOIN companies c ON c.tenant_id=d.tenant_id AND c.id=d.company_id WHERE d.tenant_id=u.tenant_id AND d.person_id=u.person_id AND c.deleted_at IS NULL) AS desks
  FROM users u
  LEFT JOIN user_identities i ON i.id = u.identity_id AND i.status = 'active' AND i.deleted_at IS NULL
  LEFT JOIN user_invitations v ON v.tenant_id=u.tenant_id AND v.user_id=u.id AND v.accepted_at IS NULL AND v.revoked_at IS NULL
  WHERE u.tenant_id = $1 AND (u.kind = 'staff' OR (u.kind='customer' AND EXISTS(SELECT 1 FROM user_two_factor_reset_requests request WHERE request.tenant_id=u.tenant_id AND request.user_id=u.id AND request.closed_at IS NULL))) AND u.deleted_at IS NULL
    AND ((u.status IN ('active','disabled') AND i.id IS NOT NULL) OR (u.status='invited' AND u.identity_id IS NULL AND v.id IS NOT NULL))
    AND ($2::text IS NULL OR u.name ILIKE $2 OR COALESCE(i.email,v.email) ILIKE $2)
    AND (cardinality($3::bigint[]) = 0 OR EXISTS (
      SELECT 1 FROM effective_principal_roles pr WHERE pr.tenant_id = u.tenant_id AND pr.user_id = u.id AND pr.role_id = ANY($3)))
    AND ($7::text IS NULL OR u.status=$7)
    AND (NOT $8::bool OR EXISTS(SELECT 1 FROM user_two_factor_reset_requests r WHERE r.tenant_id=u.tenant_id AND r.user_id=u.id AND r.closed_at IS NULL))
), user_total AS (
  SELECT count(*) AS total FROM filtered_users
), page_users AS (
  SELECT * FROM filtered_users ORDER BY {order} LIMIT $4 OFFSET $5
), page_roles AS (
  SELECT pr.user_id,
         array_agg(r.id ORDER BY r.name, r.code, r.id) AS role_ids,
         array_agg(r.name ORDER BY r.name, r.code, r.id) AS role_names,
         array_agg(r.code ORDER BY r.name, r.code, r.id) AS role_codes,
         array_agg(r.is_system ORDER BY r.name, r.code, r.id) AS role_system
  FROM (SELECT DISTINCT user_id, tenant_id, role_id FROM effective_principal_roles
        WHERE tenant_id = $1 AND user_id IN (SELECT id FROM page_users)) pr
  JOIN roles r ON r.tenant_id = pr.tenant_id AND r.id = pr.role_id
  WHERE r.company_id IS NULL OR r.company_id = ANY($6)
  GROUP BY pr.user_id
)
SELECT t.total, p.id, p.name, p.email, p.created_at, p.desks,
       p.membership,p.invitation_id,p.reset_requested_at,p.two_factor_enabled,p.installation_owner,p.reset_request_only,
       g.role_ids, g.role_names, g.role_codes, g.role_system
FROM user_total t
LEFT JOIN page_users p ON true
LEFT JOIN page_roles g ON g.user_id = p.id
ORDER BY {final_order}";

/// One page under the caller's already-proven `admin.user.list`; the scope's
/// tenant is the only account predicate, and a row's roles are those unowned
/// or owned by one of `companies` (`company_visibility`, the same
/// rule `bilibi_authz::roles::list` applies). An out-of-range page keeps the
/// real total.
pub async fn list(
    client: &impl GenericClient,
    scope: &Scope,
    companies: &[i64],
    options: &Options,
) -> Result<Page> {
    let tenant = scope.tenant();
    let pattern = options
        .search
        .as_deref()
        .map(|text| format!("%{}%", escape_like(text)));
    let limit = i64::try_from(options.page_size).map_err(|_| Error::Invalid)?;
    let offset = i64::try_from((options.page - 1).saturating_mul(options.page_size))
        .map_err(|_| Error::Invalid)?;
    let membership = options.membership.map(Membership::database_value);
    let params: [&(dyn ToSql + Sync); 8] = [
        &tenant,
        &pattern,
        &options.role_ids,
        &limit,
        &offset,
        &companies,
        &membership,
        &options.reset_requested,
    ];
    let statement = STATEMENT
        .replacen("{order}", &order(options.sort, options.descending, ""), 1)
        .replacen(
            "{final_order}",
            &order(options.sort, options.descending, "p."),
            1,
        );
    let rows = client.query(&statement, &params).await?;
    let total = rows
        .first()
        .map(|row| row.get::<_, i64>(0))
        .unwrap_or_default();
    let mut entries = Vec::with_capacity(rows.len());
    for row in &rows {
        let Some(id) = row.get::<_, Option<i64>>(1) else {
            continue;
        };
        let roles = match row.get::<_, Option<Vec<i64>>>(12) {
            None => Vec::new(),
            Some(ids) => {
                let names: Vec<String> = row.get(13);
                let codes: Vec<Option<String>> = row.get(14);
                let system: Vec<bool> = row.get(15);
                ids.into_iter()
                    .zip(names)
                    .zip(codes)
                    .zip(system)
                    .map(|(((id, name), code), is_system)| Role {
                        id,
                        name,
                        code,
                        is_system,
                    })
                    .collect()
            }
        };
        entries.push(Entry {
            id,
            name: row.get(2),
            email: row.get(3),
            created_at: row.get(4),
            desks: row.get(5),
            roles,
            membership: Membership::parse(row.get(6)).ok_or(Error::Internal)?,
            invitation: row.get(7),
            reset_requested_at: row.get(8),
            two_factor_enabled: row.get(9),
            installation_owner: row.get(10),
            reset_request_only: row.get(11),
        });
    }
    Ok(Page {
        entries,
        total_entries: u64::try_from(total).unwrap_or_default(),
    })
}
