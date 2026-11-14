//! Three contact findings of #603, through the registered `core.person.*`
//! operations on a fresh disposable database. Each line prints `ok` or
//! `FAIL`; any `FAIL` exits 1.
//!
//! 1. `core.person.list` reads the tenant's custom field definitions once per
//!    answer, not once per contact (CSV export and bulk update read through it).
//! 2. `core.person.duplicates` matches a phone by its canonical international
//!    key, so `+60 12-345 6789` finds `0060123456789`.
//! 3. A `core.person.update` built while a contact was hidden keeps that
//!    contact when the restriction is lifted before the plan commits.
use bilibi_agent_api::{Definitions, Handler, OperationClient, DEFINITIONS};
use bilibi_database::{pool, MutationBoundary};
use bilibi_person::{PersonInput, PersonKind, PersonService};
use bilibi_tenancy::{Authentication, ProofClient, ProofFuture, ReadClient, Scope};
use serde_json::{json, Value};
use std::error::Error as Failure;
use tokio_postgres::{types::ToSql, Row, Transaction};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

/// The plan's connection, as the reviewed-operation adapter hands it over.
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

fn handler(app: &bilibi_web::App, key: &str) -> Result<&'static Handler, Box<dyn Failure>> {
    for entry in app
        .services
        .catalog()?
        .entries::<Definitions>(DEFINITIONS)?
    {
        for operation in entry.get::<Definitions>()?.operations {
            if operation.key == key {
                return Ok(&operation.handler);
            }
        }
    }
    Err(format!("missing registered operation {key}").into())
}
async fn read(
    app: &bilibi_web::App,
    tx: &Transaction<'_>,
    scope: &Scope,
    key: &str,
    input: Value,
) -> Result<Value, Box<dyn Failure>> {
    let Handler::Read(read) = handler(app, key)? else {
        return Err("expected a read".into());
    };
    let output = read
        .call(&Caller(tx), scope, &input)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(output.envelope(key))
}
/// Rows of `table` read so far by this database, through the table and every
/// one of its indexes.
async fn rows_read(client: &tokio_postgres::Client, table: &str) -> Result<i64, Box<dyn Failure>> {
    client
        .execute("SELECT pg_stat_force_next_flush()", &[])
        .await?;
    let row = client
        .query_one(
            "SELECT coalesce(t.seq_tup_read,0) + coalesce((SELECT sum(idx_tup_read) FROM pg_stat_user_indexes i WHERE i.relid=t.relid),0)::bigint FROM pg_stat_user_tables t WHERE t.relname=$1",
            &[&table],
        )
        .await?;
    Ok(row.get(0))
}
fn contact(name: &str, email: &str, phone: &str) -> PersonInput {
    PersonInput {
        name: name.into(),
        kind: PersonKind::Human,
        email: Some(email.into()),
        phone: Some(phone.into()),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Failure>> {
    if std::env::var("BILIBI_PERSON_PROOF").as_deref() != Ok("disposable") {
        return Err(
            "BILIBI_PERSON_PROOF=disposable and a fresh disposable DATABASE_URL required".into(),
        );
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_database::migrate(&db, &bilibi_module_registry::discover()?).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:45483")?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let auth = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let client = db.get().await?;
    let manager = auth.authenticate(&**client, 1, 1, String::new()).await?;
    drop(client);
    let people = app.services.get::<PersonService>(bilibi_person::SERVICE)?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let (stats, connection) =
        tokio_postgres::connect(&std::env::var("DATABASE_URL")?, tokio_postgres::NoTls).await?;
    tokio::spawn(connection);

    let mut failures = 0;
    let mut check = |ok: bool, what: &str| {
        println!("{}: {what}", if ok { "ok" } else { "FAIL" });
        failures += usize::from(!ok);
    };

    // 1. Two active person fields and 30 contacts on one list page.
    stats
        .batch_execute(
            "INSERT INTO base_custom_fields(tenant_id,record_type,key,label,kind,position)
               VALUES (1,'person','segment','Segment','text',1), (1,'person','tier','Tier','text',2)",
        )
        .await?;
    for n in 1..=30 {
        people
            .create(
                &manager,
                &contact(
                    &format!("Listed {n:02}"),
                    &format!("listed{n}@example.test"),
                    "+60 3-1234 5678",
                ),
            )
            .await
            .map_err(|e| format!("{e:?}"))?;
    }
    db.resize(1);
    let mut client = db.get().await?;
    let before = rows_read(&stats, "base_custom_fields").await?;
    let tx = client.transaction().await?;
    let listed = read(
        &app,
        &tx,
        &manager,
        "core.person.list",
        json!({"search": "listed", "page_size": 50}),
    )
    .await?;
    tx.rollback().await?;
    client
        .execute("SELECT pg_stat_force_next_flush()", &[])
        .await?;
    let reads = rows_read(&stats, "base_custom_fields").await? - before;
    let entries = listed["result"].as_array().map_or(0, Vec::len);
    let custom = listed["result"][0]["custom"].as_array().map_or(0, Vec::len);
    check(
        entries == 30 && custom == 2,
        &format!(
            "the list returns 30 contacts, each with the 2 active fields ({entries}, {custom})"
        ),
    );
    check(
        reads <= 4,
        &format!("it read {reads} field-definition rows for 30 contacts (at most 4: once, not once per contact)"),
    );

    // 2. One number written two ways, and a national number that is not compared.
    people
        .create(
            &manager,
            &contact("Phone Owner", "phone.owner@example.test", "+60 12-345 6789"),
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
    let tx = client.transaction().await?;
    let found = read(
        &app,
        &tx,
        &manager,
        "core.person.duplicates",
        json!({"phone": "0060123456789"}),
    )
    .await?;
    let national = read(
        &app,
        &tx,
        &manager,
        "core.person.duplicates",
        json!({"phone": "012-345 6789"}),
    )
    .await?;
    let exact = read(
        &app,
        &tx,
        &manager,
        "core.person.duplicates",
        json!({"phone": "+60 12-345 6789"}),
    )
    .await?;
    tx.rollback().await?;
    let names = |v: &Value| {
        v["result"]
            .as_array()
            .map(|rows| rows.iter().map(|r| r["name"].clone()).collect::<Vec<_>>())
            .unwrap_or_default()
    };
    check(
        names(&found) == [json!("Phone Owner")],
        &format!(
            "0060123456789 finds the contact stored as +60 12-345 6789 ({:?})",
            names(&found)
        ),
    );
    check(
        names(&exact) == [json!("Phone Owner")],
        &format!("the number as stored still finds it ({:?})", names(&exact)),
    );
    check(
        names(&national).is_empty(),
        &format!(
            "a national number is not guessed into the country ({:?})",
            names(&national)
        ),
    );

    // 3. The manager's roles may not see personal email or phone while the
    // update is planned; the restriction is lifted before it commits.
    let person = people
        .create(
            &manager,
            &contact("Hidden Contact", "kept@example.test", "+60 19-876 5432"),
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
    stats
        .execute(
            "INSERT INTO field_restrictions(tenant_id,role_id,table_id,field_id,mode) SELECT 1,id,'persons',f,'withhold' FROM roles CROSS JOIN unnest(ARRAY['email','phone']) f WHERE tenant_id=1 ON CONFLICT DO NOTHING",
            &[],
        )
        .await?;
    let Handler::SafeWrite(update) = handler(&app, "core.person.update")? else {
        return Err("expected a write".into());
    };
    // What the review holds: the contact as the reader saw it, hidden
    // contacts omitted as the guide says, renamed.
    let tx = client.transaction().await?;
    let seen = read(
        &app,
        &tx,
        &manager,
        "core.person.get",
        json!({"person_id": person.id.get()}),
    )
    .await?;
    let input = json!({"person_id": person.id.get(), "version": person.version, "identity": {"name": "Renamed Contact", "kind": "human"}});
    let preview = update
        .preview(&Caller(&tx), &manager, &input)
        .await
        .map_err(|e| format!("{e:?}"))?
        .envelope("core.person.update");
    tx.rollback().await?;
    check(
        seen["result"]["email"] == json!({"restricted": true}),
        &format!(
            "while planned, the email is hidden from the planner ({})",
            seen["result"]["email"]
        ),
    );
    check(
        preview["result"]["identity"]["email"].is_null(),
        "the preview shows no hidden value",
    );
    stats
        .execute(
            "DELETE FROM field_restrictions WHERE tenant_id=1 AND table_id='persons'",
            &[],
        )
        .await?;
    let mut tx = boundary
        .begin(
            &mut client,
            manager.mutation_context("person.contact.proof"),
        )
        .await?;
    update
        .commit(&mut tx, &manager, &input)
        .await
        .map_err(|e| format!("{e:?}"))?;
    tx.commit().await?;
    drop(client);
    let after = people.read(&manager, person.id).await?;
    check(
        after.name == "Renamed Contact",
        &format!("the commit renamed the contact ({})", after.name),
    );
    check(
        after.email.as_deref() == Some("kept@example.test")
            && after.phone.as_deref() == Some("+60 19-876 5432"),
        &format!(
            "the email and phone hidden at planning time are kept ({:?}, {:?})",
            after.email, after.phone
        ),
    );
    // A visible contact still clears with an empty string.
    let cleared = json!({"person_id": person.id.get(), "version": after.version, "identity": {"name": "Renamed Contact", "kind": "human", "phone": ""}});
    let mut client = db.get().await?;
    let mut tx = boundary
        .begin(
            &mut client,
            manager.mutation_context("person.contact.proof"),
        )
        .await?;
    update
        .commit(&mut tx, &manager, &cleared)
        .await
        .map_err(|e| format!("{e:?}"))?;
    tx.commit().await?;
    drop(client);
    let after = people.read(&manager, person.id).await?;
    check(
        after.phone.is_none() && after.email.as_deref() == Some("kept@example.test"),
        &format!(
            "an empty phone clears it and the omitted email is kept ({:?}, {:?})",
            after.phone, after.email
        ),
    );

    if failures > 0 {
        return Err(format!("{failures} check(s) failed").into());
    }
    Ok(())
}
