//! A `core.address.update` planned while the phone was hidden keeps the phone
//! when the restriction is lifted before the plan commits. The same gap #603
//! closed for people. On a fresh disposable database, through the registered
//! handler; each line prints `ok` or `FAIL`, and any `FAIL` exits 1.
use bilibi_address::{AddressInput, AddressService, SERVICE};
use bilibi_agent_api::{Definitions, Handler, OperationClient, DEFINITIONS};
use bilibi_database::{pool, MutationBoundary};
use bilibi_tenancy::{Authentication, ProofClient, ProofFuture, ReadClient};
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

fn update_handler(
    app: &bilibi_web::App,
) -> Result<&'static dyn bilibi_agent_api::WriteHandler, Box<dyn Failure>> {
    for entry in app
        .services
        .catalog()?
        .entries::<Definitions>(DEFINITIONS)?
    {
        for operation in entry.get::<Definitions>()?.operations {
            if operation.key == "core.address.update" {
                if let Handler::SafeWrite(handler) = operation.handler {
                    return Ok(handler);
                }
            }
        }
    }
    Err("missing core.address.update".into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Failure>> {
    if std::env::var("BILIBI_ADDRESS_PROOF").as_deref() != Ok("disposable") {
        return Err(
            "BILIBI_ADDRESS_PROOF=disposable and a fresh disposable DATABASE_URL required".into(),
        );
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_database::migrate(&db, &bilibi_module_registry::discover()?).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:45484")?;
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
    let addresses = app.services.get::<AddressService>(SERVICE)?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let update = update_handler(&app)?;
    let (admin, connection) =
        tokio_postgres::connect(&std::env::var("DATABASE_URL")?, tokio_postgres::NoTls).await?;
    tokio::spawn(connection);

    let mut failures = 0;
    let mut check = |ok: bool, what: &str| {
        println!("{}: {what}", if ok { "ok" } else { "FAIL" });
        failures += usize::from(!ok);
    };

    let created = addresses
        .create(
            &manager,
            &AddressInput {
                label: Some("Front desk".into()),
                phone: Some("+60 3 1234 5678".into()),
                line1: Some("1 Jalan Ampang".into()),
                ..AddressInput::default()
            },
        )
        .await
        .map_err(|e| format!("{e:?}"))?;

    // Every role of the tenant may not see the phone while the update is
    // planned. The plan leaves the phone out, as the guide says.
    admin
        .execute(
            "INSERT INTO field_restrictions(tenant_id,role_id,table_id,field_id,mode) SELECT 1,id,'addresses','phone','withhold' FROM roles WHERE tenant_id=1 ON CONFLICT DO NOTHING",
            &[],
        )
        .await?;
    let hidden = addresses.read(&manager, created.id).await?;
    check(
        hidden.address.phone.is_none() && hidden.restricted.iter().any(|f| f == "phone"),
        &format!(
            "while planned, the phone is hidden from the planner ({:?})",
            hidden.restricted
        ),
    );
    let relabel = json!({"address_id": created.id, "version": created.version, "address": {"label": "Reception", "line1": "1 Jalan Ampang"}});
    let mut client = db
        .get_detached("the walkthrough's own checking connection")
        .await?;
    let tx = client.transaction().await?;
    let preview = update
        .preview(&Caller(&tx), &manager, &relabel)
        .await
        .map_err(|e| format!("{e:?}"))?
        .envelope("core.address.update");
    tx.rollback().await?;
    check(
        preview["result"]["address"]["phone"] == json!({"restricted": true}),
        &format!(
            "the review shows the phone as kept and restricted ({})",
            preview["result"]["address"]["phone"]
        ),
    );

    admin
        .execute(
            "DELETE FROM field_restrictions WHERE tenant_id=1 AND table_id='addresses'",
            &[],
        )
        .await?;
    let commit = |input: Value| {
        let boundary = boundary.clone();
        let manager = manager.clone();
        let db = db.clone();
        async move {
            let mut client = db.get().await?;
            let mut tx = boundary
                .begin(&mut client, manager.mutation_context("address.phone.proof"))
                .await?;
            update
                .commit(&mut tx, &manager, &input)
                .await
                .map_err(|e| format!("{e:?}"))?;
            tx.commit().await?;
            Ok::<_, Box<dyn Failure>>(())
        }
    };
    drop(client);
    commit(relabel).await?;
    let after = addresses.read(&manager, created.id).await?;
    check(
        after.address.label.as_deref() == Some("Reception"),
        &format!(
            "the commit relabelled the address ({:?})",
            after.address.label
        ),
    );
    check(
        after.address.phone.as_deref() == Some("+60 3 1234 5678"),
        &format!(
            "the phone hidden at planning time is kept ({:?})",
            after.address.phone
        ),
    );

    // Visible now: a null phone keeps it too, and only "" clears it.
    commit(json!({"address_id": created.id, "version": after.version, "address": {"label": "Reception", "line1": "1 Jalan Ampang", "phone": null}})).await?;
    let after = addresses.read(&manager, created.id).await?;
    check(
        after.address.phone.as_deref() == Some("+60 3 1234 5678"),
        &format!("a null phone keeps it ({:?})", after.address.phone),
    );
    commit(json!({"address_id": created.id, "version": after.version, "address": {"label": "Reception", "phone": ""}})).await?;
    let after = addresses.read(&manager, created.id).await?;
    check(
        after.address.phone.is_none(),
        &format!("an empty phone clears it ({:?})", after.address.phone),
    );
    check(
        after.address.line1.is_none(),
        &format!(
            "another fact left out still clears, as before ({:?})",
            after.address.line1
        ),
    );

    if failures > 0 {
        return Err(format!("{failures} check(s) failed").into());
    }
    Ok(())
}
