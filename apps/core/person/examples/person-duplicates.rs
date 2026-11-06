//! Native owner/registered-handler proof on a fresh disposable database.
//! No test runner: real services, audited transactions and an exhausted pool.
use bilibi_agent_api::{Definitions, Handler, OperationClient, DEFINITIONS};
use bilibi_database::{pool, Error, MutationBoundary};
use bilibi_person::{DuplicateQuery, PersonInput, PersonKind, PersonService};
use bilibi_tenancy::{Authentication, ProofClient, ProofFuture, ReadClient, Scope};
use serde_json::{json, Value};
use std::{error::Error as Failure, time::Duration};
use tokio_postgres::{types::ToSql, Row, Transaction};
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

// The same provenance contract as the reviewed-operation transaction adapter.
// A bare GenericClient adapter intentionally does not expose transaction().
struct Caller<'a, 'b>(&'a Transaction<'b>);
impl ProofClient for Caller<'_, '_> {
    fn query_opt<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> ProofFuture<'a, Option<Row>> {
        Box::pin(async move { Ok(self.0.query_opt(sql, params).await?) })
    }
}
impl ReadClient for Caller<'_, '_> {
    fn query<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> ProofFuture<'a, Vec<Row>> {
        Box::pin(async move { Ok(self.0.query(sql, params).await?) })
    }
}
impl OperationClient for Caller<'_, '_> {
    fn transaction(&self) -> Option<&Transaction<'_>> {
        Some(self.0)
    }
}
async fn registered(
    app: &bilibi_web::App,
    tx: &Transaction<'_>,
    scope: &Scope,
    key: &str,
    input: &Value,
) -> Result<Value, Box<dyn Failure>> {
    for entry in app
        .services
        .catalog()?
        .entries::<Definitions>(DEFINITIONS)?
    {
        for operation in entry.get::<Definitions>()?.operations {
            if operation.key == key {
                let Handler::Read(handler) = operation.handler else {
                    return Err("expected registered read".into());
                };
                bilibi_authz::authorize_key(tx, scope, operation.capability).await?;
                let output = tokio::time::timeout(
                    Duration::from_secs(5),
                    handler.call(&Caller(tx), scope, input),
                )
                .await??;
                return Ok(output.envelope(key));
            }
        }
    }
    Err("missing registered operation".into())
}
fn query(name: &str) -> DuplicateQuery {
    DuplicateQuery {
        name: Some(name.into()),
        ..DuplicateQuery::default()
    }
}
fn input(name: &str) -> PersonInput {
    PersonInput {
        name: name.into(),
        kind: PersonKind::Human,
        email: Some("candidate@example.test".into()),
        phone: Some("+60123456789".into()),
    }
}
async fn counts(client: &tokio_postgres::Client) -> Result<(i64, i64, i64), Error> {
    let row = client.query_one("SELECT (SELECT count(*) FROM persons), (SELECT count(*) FROM base_audit_mutations), (SELECT count(*) FROM base_audit_actions)", &[]).await?;
    Ok((row.get(0), row.get(1), row.get(2)))
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn Failure>> {
    if std::env::var("BILIBI_PERSON_PROOF").as_deref() != Ok("disposable") {
        return Err(
            "BILIBI_PERSON_PROOF=disposable and a fresh disposable DATABASE_URL required".into(),
        );
    }
    println!("compiled composition: {:?}", bilibi_web::composition());
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_database::migrate(&db, &bilibi_module_registry::discover()?).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:45483")?;
    let grants = app
        .services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let auth = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let client = db.get().await?;
    let manager = auth.authenticate(&**client, 1, 1, String::new()).await?;
    let viewer = auth.authenticate(&**client, 1, 2, String::new()).await?;
    let other = auth.authenticate(&**client, 2, 5, String::new()).await?;
    drop(client);
    let people = app.services.get::<PersonService>(bilibi_person::SERVICE)?;
    let retired = people.create(&manager, &input("Retired Candidate")).await?;
    people.retire(&manager, retired.id, retired.version).await?;
    let foreign = people
        .create(&other, &input("Transaction Candidate"))
        .await?;
    assert_ne!(foreign.id, retired.id);
    println!("ok: committed owner setup including retired and other-tenant candidates");

    db.resize(1);
    let mut client = db.get().await?;
    let before = counts(&client).await?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let mut changes = people.subscribe();
    let mut tx = boundary
        .begin(
            &mut client,
            manager.mutation_context("person.duplicates.proof"),
        )
        .await?;
    let created = people
        .create_in(&mut tx, &manager, &input("Transaction Candidate"))
        .await?;
    let q = query("  TRANSACTION CANDIDATE  ");
    let matches = tokio::time::timeout(
        Duration::from_secs(5),
        people.duplicates_on(tx.sql(), &manager, &q),
    )
    .await??;
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].id, created.id);
    assert_eq!(db.status().available, 0);
    assert_eq!(db.status().size, 1);
    println!("ok: uncommitted owner creation visible on caller transaction with exhausted single-connection pool");
    let value = registered(
        &app,
        tx.sql(),
        &manager,
        "core.person.duplicates",
        &json!({"name":"TRANSACTION CANDIDATE"}),
    )
    .await?;
    assert_eq!(value["result"][0]["id"], json!(created.id));
    let value = registered(
        &app,
        tx.sql(),
        &manager,
        "core.person.get",
        &json!({"person_id":created.id}),
    )
    .await?;
    assert_eq!(value["result"]["id"], json!(created.id));
    let value = registered(
        &app,
        tx.sql(),
        &manager,
        "core.person.list",
        &json!({"search":"Transaction Candidate"}),
    )
    .await?;
    assert_eq!(value["result"][0]["id"], json!(created.id));
    println!("ok: actual registered duplicate/get/list handlers and projections use supplied transaction without pool acquisition");
    assert!(people
        .duplicates_on(tx.sql(), &other, &q)
        .await?
        .iter()
        .all(|p| p.id == foreign.id));
    assert_eq!(people.duplicates_on(tx.sql(), &other, &q).await?.len(), 1);
    println!("ok: same-name candidate tenant separation");

    for criterion in [
        DuplicateQuery {
            email: Some(" CANDIDATE@EXAMPLE.TEST ".into()),
            ..DuplicateQuery::default()
        },
        DuplicateQuery {
            phone: Some(" +60123456789 ".into()),
            ..DuplicateQuery::default()
        },
    ] {
        let found = people.duplicates_on(tx.sql(), &manager, &criterion).await?;
        assert_eq!(found.len(), 2);
        assert!(found
            .iter()
            .any(|p| p.id == retired.id && p.retired_at.is_some()));
        assert!(found.iter().any(|p| p.id == created.id));
    }
    let excluded = DuplicateQuery {
        except_id: Some(created.id),
        ..q.clone()
    };
    assert!(people
        .duplicates_on(tx.sql(), &manager, &excluded)
        .await?
        .is_empty());
    let limited = DuplicateQuery {
        email: Some("candidate@example.test".into()),
        limit: 1,
        ..DuplicateQuery::default()
    };
    assert_eq!(
        people.duplicates_on(tx.sql(), &manager, &limited).await?[0].id,
        retired.id
    );
    for invalid in [
        DuplicateQuery::default(),
        DuplicateQuery {
            limit: 0,
            ..q.clone()
        },
        DuplicateQuery {
            limit: 101,
            ..q.clone()
        },
        query(&"x".repeat(161)),
        query("bad\0name"),
    ] {
        assert!(matches!(
            people.duplicates_on(tx.sql(), &manager, &invalid).await,
            Err(Error::Invalid)
        ));
    }
    println!("ok: original normalized OR criteria, retired inclusion, exclusion, ordering and invalid/limit policy");

    // Fixture-only policy changes, all rolled back with this transaction.
    tx.sql().execute("INSERT INTO field_restrictions(tenant_id,role_id,table_id,field_id,mode) SELECT $1,id,'persons',f,'withhold' FROM roles CROSS JOIN unnest(ARRAY['email','phone']) f WHERE tenant_id=$1 AND code='viewer' ON CONFLICT DO NOTHING", &[&manager.tenant()]).await?;
    let redacted = people.duplicates_on(tx.sql(), &viewer, &q).await?;
    assert_eq!(redacted.len(), 1);
    assert!(redacted[0].email.is_none() && redacted[0].phone.is_none());
    assert!(redacted[0].restricted.iter().any(|f| f == "email"));
    let value = registered(
        &app,
        tx.sql(),
        &viewer,
        "core.person.duplicates",
        &json!({"name":"Transaction Candidate"}),
    )
    .await?;
    assert_eq!(value["result"][0]["email"], json!({"restricted":true}));
    for criterion in [
        DuplicateQuery {
            email: Some("candidate@example.test".into()),
            ..DuplicateQuery::default()
        },
        DuplicateQuery {
            phone: Some("+60123456789".into()),
            ..DuplicateQuery::default()
        },
    ] {
        assert!(matches!(
            people.duplicates_on(tx.sql(), &viewer, &criterion).await,
            Err(Error::Forbidden)
        ));
    }
    assert!(registered(
        &app,
        tx.sql(),
        &viewer,
        "core.person.duplicates",
        &json!({"email":"candidate@example.test"})
    )
    .await
    .is_err());
    tx.sql()
        .execute(
            "UPDATE field_restrictions SET mode='mask' WHERE tenant_id=$1 AND table_id='persons'",
            &[&manager.tenant()],
        )
        .await?;
    let masked = people.duplicates_on(tx.sql(), &viewer, &q).await?;
    assert!(masked[0].masked.iter().any(|f| f == "email"));
    assert_ne!(masked[0].email, created.email);
    assert!(masked[0].email.is_some());
    println!("ok: fresh restrictions refuse contact probes; native and registered redaction; partial masking retained");

    tx.sql().execute("INSERT INTO principal_capabilities(tenant_id,user_id,capability,is_allowed) VALUES($1,$2,$3,false) ON CONFLICT(tenant_id,user_id,capability) DO UPDATE SET is_allowed=false", &[&manager.tenant(), &viewer.user(), &bilibi_person::LIST]).await?;
    assert!(matches!(
        people.duplicates_on(tx.sql(), &viewer, &q).await,
        Err(Error::Forbidden)
    ));
    assert!(registered(
        &app,
        tx.sql(),
        &viewer,
        "core.person.duplicates",
        &json!({"name":"Transaction Candidate"})
    )
    .await
    .is_err());
    tx.sql()
        .execute(
            "UPDATE users SET status='suspended' WHERE tenant_id=$1 AND id=$2",
            &[&manager.tenant(), &manager.user()],
        )
        .await?;
    assert!(matches!(
        people.duplicates_on(tx.sql(), &manager, &q).await,
        Err(Error::Unauthorized)
    ));
    assert!(registered(
        &app,
        tx.sql(),
        &manager,
        "core.person.duplicates",
        &json!({"name":"Transaction Candidate"})
    )
    .await
    .is_err());
    println!("ok: direct denial and fresh sealed-Scope reproof see uncommitted policy/account revocation");
    tx.rollback().await?;
    assert_eq!(counts(&client).await?, before);
    assert!(changes.try_recv().is_err());
    assert!(people
        .duplicates_on(&**client, &manager, &q)
        .await?
        .is_empty());
    drop(client);
    assert!(people.duplicates(&manager, &q).await?.is_empty());
    assert_eq!(
        people
            .duplicates(&manager, &query("Retired Candidate"))
            .await?[0]
            .id,
        retired.id
    );
    println!("ok: rollback removes Person and audit effects, publishes nothing, restores policy; pooled convenience retained");
    println!("PASS: native supplied-client duplicate owner and registered transaction proof");
    Ok(())
}
