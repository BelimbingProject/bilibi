//! Customer sharing proof (issue 524), on a fresh disposable database.
//!
//! Staff share files with portal customers through the real `Documents`
//! facade; each customer then reads through the portal's own entry points
//! (`shared_with_me`, `read_shared_file`) with a `PortalScope` from a real
//! portal sign-in. It fails unless:
//!
//! - an individual customer lists exactly the documents shared with their
//!   Person, and two business customers of one company both list the
//!   company's share while only the right one lists a Person share;
//! - a clean file downloads, also for a business customer whose company is
//!   not the filing company, and the read is audited as the customer;
//! - a pending and an infected file are refused, as are a document nobody
//!   shared, another customer's, a revoked share, an ended share and a
//!   document past its expiry date, and every refusal is audited as the
//!   customer with its reason;
//! - a customer of another tenant and a customer without the portal role see
//!   nothing and are refused;
//! - a link cannot be shared with a customer, and staff without document
//!   management cannot share.
//!
//! Files need private storage and a scanner: the example points the settings
//! at folders under `BILIBI_DOCUMENT_CUSTOMER_ROOT`, binds a fixture scanner
//! (bytes starting `pending` get no verdict, `infected` are infected, the rest
//! are clean; it exists only in this executable) and runs Queue's executor in
//! process. Run after `bilibi-web migrate`, `bilibi-walkthrough-fixture` and
//! `bilibi-web seed-production` on a database used for nothing else:
//!
//! `BILIBI_DOCUMENT_CUSTOMER=disposable BILIBI_DOCUMENT_CUSTOMER_ROOT=/an/empty/folder
//! DATABASE_URL=… cargo run -p bilibi-document --example customer-share-walkthrough --locked`
use bilibi_authz::grants::Grants;
use bilibi_database::pool;
use bilibi_document::{
    CustomerShareInput, Documents, Draft, PortalFilter, Recipient, Source, Upload, WriteError,
    SERVICE,
};
use bilibi_media::{Media, ScanFacts, ScanFuture, Scanner, Verdict};
use bilibi_party_ids::{AffiliationId, PersonId};
use bilibi_queue::{Config, Executor};
use bilibi_tenancy::{Authentication, PortalScope, Scope};
use bilibi_walkthrough_fixture::PASSWORD;
use serde_json::json;
use std::{collections::BTreeSet, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Fixture scanner, not a production adapter.
struct FixtureScanner;
impl Scanner for FixtureScanner {
    fn name(&self) -> &str {
        "customer-share-walkthrough"
    }
    fn version(&self) -> String {
        "1 (example)".into()
    }
    fn scan<'a>(&'a self, bytes: &'a [u8], _facts: ScanFacts<'a>) -> ScanFuture<'a> {
        Box::pin(async move {
            if bytes.starts_with(b"pending") {
                Verdict::Unavailable
            } else if bytes.starts_with(b"infected") {
                Verdict::Infected {
                    signature: "fixture".into(),
                }
            } else {
                Verdict::Clean
            }
        })
    }
}

fn check(ok: bool, what: &str) -> Result<()> {
    if ok {
        println!("PASS {what}");
        Ok(())
    } else {
        Err(format!("FAIL {what}").into())
    }
}

/// A Person, optionally affiliated with `company`, given a customer account
/// (with the portal role when `role`), signed in to the portal.
#[allow(clippy::too_many_arguments)]
async fn customer(
    app: &bilibi_web::App,
    grants: &Grants,
    authentication: &Authentication,
    staff: &Scope,
    name: &str,
    email: &str,
    company: Option<i64>,
    role: bool,
) -> Result<(i64, PortalScope)> {
    let tenant = staff.tenant();
    let client = app.db.get().await?;
    let code = email.replace(['@', '.', '-'], "_");
    let person: i64 = client
        .query_one(
            "INSERT INTO persons(tenant_id,code,name,kind) VALUES($1,$2,$3,'human') RETURNING id",
            &[&tenant, &code, &name],
        )
        .await?
        .get(0);
    let affiliation: Option<i64> = match company {
        Some(company) => Some(
            client
                .query_one(
                    "INSERT INTO person_affiliations(tenant_id,person_id,organisation_id) VALUES($1,$2,$3) RETURNING id",
                    &[&tenant, &person, &company],
                )
                .await?
                .get(0),
        ),
        None => None,
    };
    drop(client);
    let mut client = app.db.get().await?;
    let mut tx = app
        .mutations
        .begin(
            &mut client,
            staff.mutation_context("customer_share_walkthrough.customer"),
        )
        .await?;
    let user = app
        .user
        .create_customer_in(
            &mut tx,
            &bilibi_user::NewCustomer {
                name,
                email,
                password: PASSWORD,
                person: PersonId::new(person).map_err(|_| "person id")?,
                affiliation: affiliation
                    .map(AffiliationId::new)
                    .transpose()
                    .map_err(|_| "affiliation id")?,
            },
        )
        .await
        .map_err(|error| format!("customer {email}: {error:?}"))?;
    if role {
        let sealed = PortalScope::seal(
            authentication
                .authenticate(tx.sql(), tenant, user, String::new())
                .await?,
        )?;
        grants
            .assign_portal_role_in(&mut tx, &sealed, bilibi_portal::PORTAL_ROLE)
            .await?;
    }
    tx.commit().await?;
    let login = app
        .user
        .portal_login(tenant, email, PASSWORD)
        .await
        .map_err(|error| format!("portal sign-in {email}: {error:?}"))?;
    Ok((person, app.security.guard_portal(&login.token).await?))
}

/// A staff account signed in.
async fn staff(app: &bilibi_web::App, email: &str) -> Result<Scope> {
    let login = app.user.login(email, PASSWORD).await?;
    Ok(app.security.guard(&login.token).await?)
}

/// Shares document `id` with `recipient` as `admin`, with no end.
async fn share(
    app: &bilibi_web::App,
    documents: &Documents,
    admin: &Scope,
    id: i64,
    recipient: Recipient,
) -> Result<i64> {
    let mut client = app.db.get().await?;
    let mut tx = app
        .mutations
        .begin(
            &mut client,
            admin.mutation_context("core.document.customer_share"),
        )
        .await?;
    let share = documents
        .share_customer(
            &mut tx,
            admin,
            id,
            &CustomerShareInput {
                recipient,
                until: None,
            },
        )
        .await?;
    tx.commit().await?;
    Ok(share.id)
}

/// The documents a customer lists, by id.
async fn listed(documents: &Documents, portal: &PortalScope) -> Result<BTreeSet<i64>> {
    let page = documents
        .shared_with_me(portal, &PortalFilter::default())
        .await?;
    if page.total != page.entries.len() as i64 {
        return Err(format!(
            "the list counts {} but shows {}",
            page.total,
            page.entries.len()
        )
        .into());
    }
    Ok(page.entries.iter().map(|d| d.id).collect())
}

/// The refusal actions audited as `user` for `document`, newest first.
async fn refusals(app: &bilibi_web::App, user: i64, document: i64) -> Result<Vec<String>> {
    Ok(app
        .db
        .get()
        .await?
        .query(
            "SELECT payload ->> 'reason' FROM base_audit_actions WHERE event = 'core.document.download_refused'
               AND actor_id = $1 AND actor_role = 'customer' AND payload ->> 'portal' = 'true' AND payload ->> 'document_id' = $2
             ORDER BY id DESC",
            &[&user, &document.to_string()],
        )
        .await?
        .iter()
        .map(|row| row.get::<_, Option<String>>(0).unwrap_or_default())
        .collect())
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DOCUMENT_CUSTOMER").as_deref() != Ok("disposable") {
        return Err(
            "set BILIBI_DOCUMENT_CUSTOMER=disposable and DATABASE_URL to a fresh disposable migrated database"
                .into(),
        );
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let root = std::path::PathBuf::from(std::env::var("BILIBI_DOCUMENT_CUSTOMER_ROOT")?);
    bilibi_walkthrough_fixture::install(&db).await?;
    // A workplace for the administrator: files are kept by someone signed in
    // to a company.
    db.get().await?.batch_execute(
        "WITH p AS (INSERT INTO persons(tenant_id,code,name,kind) SELECT 1,'customer_share_admin','Customer Share Admin','human' WHERE NOT EXISTS (SELECT 1 FROM users u JOIN user_identities i ON i.id=u.identity_id WHERE u.tenant_id=1 AND i.email='admin@example.test' AND u.person_id IS NOT NULL) RETURNING id),
              u AS (UPDATE users SET person_id=p.id FROM p, user_identities i WHERE users.identity_id=i.id AND users.tenant_id=1 AND i.email='admin@example.test' RETURNING users.person_id)
         INSERT INTO user_desks(tenant_id,person_id,company_id) SELECT 1,person_id,1 FROM u ON CONFLICT DO NOTHING",
    ).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35564")?;
    for (key, folder) in [
        ("artifacts.storage_root", "artifacts"),
        ("media.disks", "media"),
    ] {
        let path = root.join(folder);
        std::fs::DirBuilder::new().recursive(true).create(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        let value = if key == "media.disks" {
            json!({"local": path})
        } else {
            json!(path)
        };
        db.get().await?.execute(
            "INSERT INTO base_settings(key,tenant_id,scope_type,scope_id,value,is_encrypted) VALUES($1,0,'global',0,$2,false) ON CONFLICT (key,tenant_id,scope_type,scope_id) DO UPDATE SET value = excluded.value",
            &[&key, &value],
        ).await?;
    }
    app.services
        .get::<Media>("base/media.store")?
        .bind_scanner(Arc::new(FixtureScanner))?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    // The portal customer role now holds `portal.document.view`.
    grants.reconcile().await?;
    let authentication = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(
        Executor::from_services(
            &app.services,
            Config {
                concurrency: 2,
                poll_interval: Duration::from_millis(200),
                ..Config::default()
            },
        )?
        .run(stopped),
    );
    let documents = app.services.get::<Documents>(SERVICE)?;
    let admin = staff(&app, "admin@example.test").await?;
    let viewer = staff(&app, "viewer@example.test").await?;
    let other_tenant = staff(&app, "manager@other.example.test").await?;

    // Customers: Ann on her own, Ben and Cai for company 2 (not the filing
    // company), Dee of tenant 2, and Eve without the portal role.
    let (ann_person, ann) = customer(
        &app,
        &grants,
        &authentication,
        &admin,
        "Ann Individual",
        "ann-customer@example.test",
        None,
        true,
    )
    .await?;
    let (ben_person, ben) = customer(
        &app,
        &grants,
        &authentication,
        &admin,
        "Ben Business",
        "ben-customer@example.test",
        Some(2),
        true,
    )
    .await?;
    let (_, cai) = customer(
        &app,
        &grants,
        &authentication,
        &admin,
        "Cai Business",
        "cai-customer@example.test",
        Some(2),
        true,
    )
    .await?;
    let (_, dee) = customer(
        &app,
        &grants,
        &authentication,
        &other_tenant,
        "Dee Other Tenant",
        "dee-customer@example.test",
        None,
        true,
    )
    .await?;
    let (eve_person, eve) = customer(
        &app,
        &grants,
        &authentication,
        &admin,
        "Eve No Role",
        "eve-customer@example.test",
        None,
        false,
    )
    .await?;

    // Files, each its own document.
    let mut ids = std::collections::BTreeMap::new();
    for (key, bytes) in [
        ("ann", &b"clean: for Ann"[..]),
        ("company", &b"clean: for company 2"[..]),
        ("ben", &b"clean: for Ben alone"[..]),
        ("pending", &b"pending: never settles here"[..]),
        ("infected", &b"infected: fixture"[..]),
        ("private", &b"clean: shared with nobody"[..]),
        ("revoked", &b"clean: shared, then revoked"[..]),
        ("ended", &b"clean: shared, then ended"[..]),
        ("expired", &b"clean: past its expiry date"[..]),
    ] {
        let stored = documents
            .store_file(
                &admin,
                Upload {
                    bytes,
                    file_name: &format!("customer-share-{key}.txt"),
                },
            )
            .await?;
        let mut client = app.db.get().await?;
        let mut tx = app
            .mutations
            .begin(&mut client, admin.mutation_context("core.document.create"))
            .await?;
        let document = documents
            .create(
                &mut tx,
                &admin,
                &Draft {
                    name: Some(format!("Customer share {key}")),
                    source: Source::Internal,
                    artifact_id: Some(stored.artifact_id),
                    ..Draft::default()
                },
            )
            .await?;
        tx.commit().await?;
        ids.insert(key, document.id);
    }
    // Wait for the verdicts: the infected file is quarantined, the clean ones
    // are published, the pending one stays pending.
    for _ in 0..100 {
        let settled: i64 = db
            .get()
            .await?
            .query_one(
                "SELECT count(*) FROM core_document_revisions WHERE tenant_id=1 AND document_id = ANY($1) AND scan_status IN ('clean','quarantined')",
                &[&ids.values().copied().collect::<Vec<i64>>()],
            )
            .await?
            .get(0);
        if settled == ids.len() as i64 - 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    for key in ["ann", "pending", "infected", "revoked", "ended", "expired"] {
        share(
            &app,
            &documents,
            &admin,
            ids[key],
            Recipient::Person(ann_person),
        )
        .await?;
    }
    share(
        &app,
        &documents,
        &admin,
        ids["company"],
        Recipient::Company(2),
    )
    .await?;
    share(
        &app,
        &documents,
        &admin,
        ids["ben"],
        Recipient::Person(ben_person),
    )
    .await?;
    share(
        &app,
        &documents,
        &admin,
        ids["ann"],
        Recipient::Person(eve_person),
    )
    .await?;
    db.get()
        .await?
        .execute(
            "UPDATE core_documents SET expires_on = (now() AT TIME ZONE 'UTC')::date - 1 WHERE tenant_id=1 AND id=$1",
            &[&ids["expired"]],
        )
        .await?;

    // Who sees what.
    let set = |keys: &[&str]| keys.iter().map(|key| ids[*key]).collect::<BTreeSet<i64>>();
    check(
        listed(&documents, &ann).await?
            == set(&["ann", "pending", "infected", "revoked", "ended"]),
        "Ann lists exactly what was shared with her (not the private, company, Ben's or expired document)",
    )?;
    check(
        listed(&documents, &ben).await? == set(&["company", "ben"]),
        "Ben lists the company share and his own",
    )?;
    check(
        listed(&documents, &cai).await? == set(&["company"]),
        "Cai lists the company share but not Ben's",
    )?;
    check(
        listed(&documents, &dee).await?.is_empty(),
        "a customer of another tenant lists nothing",
    )?;
    check(
        documents
            .shared_with_me(&eve, &PortalFilter::default())
            .await
            .is_err(),
        "a customer without the portal role is refused the list",
    )?;
    let search = documents
        .shared_with_me(
            &ann,
            &PortalFilter {
                search: "pending".into(),
                ..PortalFilter::default()
            },
        )
        .await?;
    check(
        search.entries.iter().map(|d| d.id).collect::<Vec<_>>() == vec![ids["pending"]]
            && !search.entries[0].downloadable(),
        "search narrows the shared set, and a pending file is not offered",
    )?;

    // Downloads.
    let ann_user = ann.scope().user();
    let file = documents.read_shared_file(&ann, ids["ann"]).await?;
    check(
        file.bytes == b"clean: for Ann",
        "Ann downloads her clean file",
    )?;
    let audited: i64 = db
        .get()
        .await?
        .query_one(
            "SELECT count(*) FROM base_audit_actions WHERE event = 'artifacts.read' AND actor_id = $1 AND actor_role = 'customer' AND payload ->> 'owner_id' = 'core.document'",
            &[&ann_user],
        )
        .await?
        .get(0);
    check(audited == 1, "the download is audited as Ann, a customer")?;
    check(
        documents
            .read_shared_file(&ben, ids["company"])
            .await?
            .bytes
            == b"clean: for company 2",
        "Ben, a customer of company 2, downloads a file filed under company 1",
    )?;
    for (who, portal, key, refused) in [
        ("Ann", &ann, "pending", "scan_pending"),
        ("Ann", &ann, "infected", "not_clean"),
        ("Ann", &ann, "private", "not_shared"),
        ("Ann", &ann, "company", "not_shared"),
        ("Ann", &ann, "expired", "document_expired"),
        ("Cai", &cai, "ben", "not_shared"),
        ("Dee", &dee, "ann", "not_shared"),
        ("Eve", &eve, "ann", "no_portal_capability"),
    ] {
        let result = documents.read_shared_file(portal, ids[key]).await;
        let expected = match refused {
            "scan_pending" => matches!(result, Err(WriteError::ScanPending)),
            "not_clean" => matches!(result, Err(WriteError::Quarantined)),
            "no_portal_capability" => matches!(
                result,
                Err(WriteError::Access(bilibi_database::Error::Forbidden))
            ),
            _ => matches!(result, Err(WriteError::NotFound)),
        };
        let trail = refusals(&app, portal.scope().user(), ids[key]).await?;
        check(
            expected && trail.first().map(String::as_str) == Some(refused),
            &format!("{who} is refused the {key} document, audited as {refused}"),
        )?;
    }

    // A revoked and an ended share disappear at once.
    let shares = documents.customer_shares(&admin, ids["revoked"]).await?;
    {
        let mut client = app.db.get().await?;
        let mut tx = app
            .mutations
            .begin(
                &mut client,
                admin.mutation_context("core.document.customer_unshare"),
            )
            .await?;
        documents
            .unshare_customer(&mut tx, &admin, ids["revoked"], shares[0].id)
            .await?;
        tx.commit().await?;
    }
    db.get()
        .await?
        .execute(
            "UPDATE core_document_customer_shares SET expires_at = now() - interval '1 minute' WHERE tenant_id=1 AND document_id=$1",
            &[&ids["ended"]],
        )
        .await?;
    check(
        listed(&documents, &ann).await? == set(&["ann", "pending", "infected"]),
        "a revoked and an ended share leave Ann's list",
    )?;
    for (key, refused) in [("revoked", "not_shared"), ("ended", "share_ended")] {
        let result = documents.read_shared_file(&ann, ids[key]).await;
        let trail = refusals(&app, ann_user, ids[key]).await?;
        check(
            matches!(result, Err(WriteError::NotFound))
                && trail.first().map(String::as_str) == Some(refused),
            &format!("Ann is refused the {key} document, audited as {refused}"),
        )?;
    }
    let mutations: i64 = db
        .get()
        .await?
        .query_one(
            "SELECT count(*) FROM base_audit_mutations WHERE tenant_id=1 AND auditable_type='core.document.customer_share' AND (new_values ->> 'document_id' = $1::text OR old_values ->> 'document_id' = $1::text)",
            &[&ids["revoked"].to_string()],
        )
        .await?
        .get(0);
    check(
        mutations == 2,
        "the share and its revocation are audited changes the per-document query finds",
    )?;

    // Sharing itself is guarded.
    let link = {
        let mut client = app.db.get().await?;
        let mut tx = app
            .mutations
            .begin(&mut client, admin.mutation_context("core.document.create"))
            .await?;
        let document = documents
            .create(
                &mut tx,
                &admin,
                &Draft {
                    name: Some("Customer share link".into()),
                    source: Source::External,
                    external_url: Some("https://example.test/shared".into()),
                    ..Draft::default()
                },
            )
            .await?;
        tx.commit().await?;
        document.id
    };
    for (who, scope, id) in [
        ("a link", &admin, link),
        (
            "a viewer without document management",
            &viewer,
            ids["private"],
        ),
    ] {
        let mut client = app.db.get().await?;
        let mut tx = app
            .mutations
            .begin(
                &mut client,
                scope.mutation_context("core.document.customer_share"),
            )
            .await?;
        let refused = documents
            .share_customer(
                &mut tx,
                scope,
                id,
                &CustomerShareInput {
                    recipient: Recipient::Person(ann_person),
                    until: None,
                },
            )
            .await
            .is_err();
        tx.rollback().await?;
        check(refused, &format!("sharing is refused for {who}"))?;
    }

    let _ = stop.send(true);
    let _ = worker.await;
    println!("\nevery customer saw only what was shared with them, and every download and refusal was audited");
    Ok(())
}
