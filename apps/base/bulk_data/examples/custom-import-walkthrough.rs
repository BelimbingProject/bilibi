//! Custom-field CSV import through real operations and Queue, on a disposable
//! database. Fails on a wrong result; no source inspection or test harness.
use bilibi_agent_api::{
    plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans},
    Registry,
};
use bilibi_bulk_data::{BulkData, Run};
use bilibi_database::{pool, Db};
use bilibi_queue::{Config, Executor};
use bilibi_tenancy::{ProofFuture, Scope};
use serde_json::{json, Value};
use std::{os::unix::fs::DirBuilderExt, time::Duration};
use tokio_postgres::Transaction;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();
type Failure = Box<dyn std::error::Error>;

struct Browser<'a> {
    app: &'a bilibi_web::App,
    token: &'a str,
}
impl PlanAuthority for Browser<'_> {
    fn verify<'a>(&'a self, tx: &'a Transaction<'_>) -> ProofFuture<'a, PlanIdentity> {
        Box::pin(async move {
            Ok(PlanIdentity {
                scope: self.app.security.sessions.hold(tx, self.token).await?,
                credential_binding: format!("browser:{}", self.token),
            })
        })
    }
}
struct Person<'a> {
    app: &'a bilibi_web::App,
    token: &'a str,
    scope: Scope,
    db: &'a Db,
}
impl Person<'_> {
    async fn write(&self, key: &str, input: Value) -> Result<Value, Failure> {
        let plans = self.app.services.get::<Plans>("base/agent_api.plans")?;
        let browser = Browser {
            app: self.app,
            token: self.token,
        };
        let ops = [PlannedOperation {
            key: key.into(),
            input,
        }];
        let preview = plans.preview(&browser, &ops).await?;
        if preview["valid"] != true {
            return Err(format!("{key}: {preview}").into());
        }
        Ok(
            plans.commit(&self.scope, &browser, &ops).await?["results"][0]["result"]["result"]
                .clone(),
        )
    }
    async fn read(&self, key: &str, input: Value) -> Result<Value, Failure> {
        let registry = self
            .app
            .services
            .get::<Registry>("base/agent_api.registry")?;
        let client = self.db.get().await?;
        Ok(registry.call(&**client, &self.scope, key, &input).await?["result"].clone())
    }
    async fn run(&self, id: i64) -> Result<Run, Failure> {
        let bulk = self
            .app
            .services
            .get::<BulkData>(bilibi_bulk_data::SERVICE)?;
        let client = self.db.get().await?;
        Ok(bulk.run(&**client, &self.scope, id).await?)
    }
    async fn upload(&self, name: &str, csv: &str) -> Result<i64, Failure> {
        let bulk = self
            .app
            .services
            .get::<BulkData>(bilibi_bulk_data::SERVICE)?;
        bulk.upload(&self.scope, "core/person.contacts", name, csv.as_bytes())
            .await?
            .map_err(Into::into)
    }
    async fn pass(&self, id: i64, mapping: &Value, preview: bool) -> Result<Run, Failure> {
        self.write(
            if preview {
                "base.bulk_data.import.preview"
            } else {
                "base.bulk_data.import.start"
            },
            json!({"run_id":id,"version":self.run(id).await?.version,"mapping":mapping}),
        )
        .await?;
        self.settle(id).await
    }
    async fn settle(&self, id: i64) -> Result<Run, Failure> {
        for _ in 0..600 {
            let run = self.run(id).await?;
            if !run.state.working() || run.stopped {
                return Ok(run);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err("import did not settle in 60 seconds".into())
    }
}
fn check(label: &str, passed: bool) -> Result<(), Failure> {
    if !passed {
        return Err(format!("FAIL {label}").into());
    }
    println!("ok {label}");
    Ok(())
}
fn value<'a>(record: &'a Value, key: &str) -> Option<&'a Value> {
    record["custom"]
        .as_array()?
        .iter()
        .find(|field| field["key"] == key)
        .map(|field| &field["value"])
}
fn offered(types: &Value, name: &str) -> bool {
    types
        .as_array()
        .into_iter()
        .flatten()
        .find(|kind| kind["key"] == "core/person.contacts")
        .and_then(|kind| kind["fields"].as_array())
        .is_some_and(|fields| fields.iter().any(|field| field["name"] == name))
}
fn import_mapping(mut mapping: Value) -> Value {
    mapping["fields"]
        .as_array_mut()
        .expect("walkthrough mapping")
        .push(json!({"field":"kind","value":"human"}));
    mapping
}
#[tokio::main]
async fn main() -> Result<(), Failure> {
    if std::env::var("BILIBI_BULK_DATA_DEMO").as_deref() != Ok("disposable") {
        return Err(
            "set BILIBI_BULK_DATA_DEMO=disposable and use a disposable DATABASE_URL".into(),
        );
    }
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_web::migrate(&db).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "https://plat-685-rest.localhost")?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let mut logins = Vec::new();
    for email in [
        "admin@example.test",
        "manager@example.test",
        "manager@other.example.test",
    ] {
        logins.push(
            app.user
                .login(email, bilibi_walkthrough_fixture::PASSWORD)
                .await?,
        );
    }
    let person = |index: usize, scope| Person {
        app: &app,
        token: &logins[index].token,
        scope,
        db: &db,
    };
    let admin = person(0, app.security.guard(&logins[0].token).await?);
    let manager = person(1, app.security.guard(&logins[1].token).await?);
    let other = person(2, app.security.guard(&logins[2].token).await?);
    // Fixture-only storage location; no business rows are written by SQL.
    let root = std::env::current_dir()?.join("data/artifacts");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)?;
    {
        let client = db.get().await?;
        client.execute("INSERT INTO base_settings(key,tenant_id,scope_type,scope_id,value,is_encrypted) VALUES('artifacts.storage_root',0,'global',0,$1,false) ON CONFLICT (key,tenant_id,scope_type,scope_id) DO UPDATE SET value=excluded.value", &[&json!(root)]).await?;
    }
    // Provision the fixture tenant's governed lists as installation does.
    let fields = app
        .services
        .get::<bilibi_custom_fields::CustomFields>(bilibi_custom_fields::SERVICE)?;
    let list = {
        let client = db.get().await?;
        bilibi_picklists::provision_deployment(&**client, 1).await?;
        fields
            .choice_lists()
            .first()
            .ok_or("no standalone pick-list")?
            .0
    };
    let choice = admin
        .write(
            "base.picklists.value.create",
            json!({"list":list,"code":"custom_import_choice","label":"Import choice"}),
        )
        .await?;
    admin.write("base.custom_fields.define",json!({"record_type":"person","key":"category","label":"Category","kind":"choice","picklist":list})).await?;
    let mut definitions = Vec::new();
    for (key, kind) in [
        ("note", "text"),
        ("amount", "number"),
        ("day", "date"),
        ("vip", "boolean"),
        ("memo", "long_text"),
        ("site", "link"),
        ("telephone", "phone"),
        ("mailbox", "email"),
    ] {
        definitions.push(
            admin
                .write(
                    "base.custom_fields.define",
                    json!({"record_type":"person","key":key,"label":key,"kind":kind}),
                )
                .await?,
        );
    }
    check(
        "active fields discovered without restarting",
        offered(
            &manager.read("base.bulk_data.types", json!({})).await?,
            "cf_amount",
        ),
    )?;
    check(
        "other tenant does not discover these fields",
        !offered(
            &other.read("base.bulk_data.types", json!({})).await?,
            "cf_amount",
        ),
    )?;
    let mut mapping = import_mapping(json!({"fields":[
        {"field":"name","column":"Name"},{"field":"email","column":"Email"},
        {"field":"cf_note","column":"Note"},{"field":"cf_amount","column":"Amount"},
        {"field":"cf_day","value":"2026-11-16"},{"field":"cf_vip","column":"VIP"},
        {"field":"cf_memo","value":"A long note"},{"field":"cf_site","value":"https://example.test"},
        {"field":"cf_telephone","value":"+60 12 345 6789"},{"field":"cf_mailbox","value":"custom@example.test"}
    ]}));
    mapping["fields"]
        .as_array_mut()
        .ok_or("mapping fields")?
        .push(json!({"field":"cf_category","value":choice["code"]}));
    let id = manager.upload("custom.csv", "Name,Email,Note,Amount,VIP\nCustom Import,custom-import@example.test,Original,999999999999999999.123456,Yes\nBad Import,bad-import@example.test,Bad,1e99,maybe\n").await?;
    let executor = Executor::from_services(
        &app.services,
        Config {
            concurrency: 2,
            poll_interval: Duration::from_millis(50),
            lease_seconds: 10,
            max_runtime: Duration::from_secs(120),
        },
    )?;
    let (stop, receive) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(async move { executor.run(receive).await });
    let preview = manager.pass(id, &mapping, true).await?;
    check(
        "dry run validates exact custom values and rejects bad input",
        preview.preview.create == 1 && preview.preview.error == 1,
    )?;
    check(
        "dry run writes no contacts",
        manager
            .read(
                "core.person.duplicates",
                json!({"email":"custom-import@example.test"}),
            )
            .await?
            == json!([]),
    )?;
    let real = manager.pass(id, &mapping, false).await?;
    check(
        "one valid row committed and one refused",
        real.run.create == 1 && real.run.error == 1,
    )?;
    let rows = manager
        .read(
            "core.person.duplicates",
            json!({"email":"custom-import@example.test"}),
        )
        .await?;
    let record_id = rows[0]["id"].clone();
    let record = manager
        .read("core.person.get", json!({"person_id":record_id}))
        .await?;
    check(
        "decimal digits and entered scale survive CSV unchanged",
        value(&record, "amount") == Some(&json!("999999999999999999.123456")),
    )?;
    check(
        "yes/no accepted and stored in the owner dialect",
        value(&record, "vip") == Some(&json!("true")),
    )?;
    check(
        "all additional scalar types reach owner validation",
        value(&record, "mailbox") == Some(&json!("custom@example.test"))
            && value(&record, "telephone") == Some(&json!("+60 12 345 6789")),
    )?;
    check(
        "governed pick-list code reaches owner storage",
        value(&record, "category") == Some(&json!(choice["code"])),
    )?;
    manager
        .write(
            "base.bulk_data.mapping.save",
            json!({"record_type":"core/person.contacts","name":"Custom values","mapping":mapping}),
        )
        .await?;
    check(
        "saved mappings retain custom field names",
        manager
            .read(
                "base.bulk_data.mapping.list",
                json!({"record_type":"core/person.contacts"}),
            )
            .await?
            .to_string()
            .contains("cf_amount"),
    )?;
    let update = manager
        .upload(
            "update.csv",
            "Name,Email,Note\nCustom Import Updated,custom-import@example.test,Changed\n",
        )
        .await?;
    let update_mapping = import_mapping(
        json!({"duplicates":"update","fields":[{"field":"name","column":"Name"},{"field":"email","column":"Email"},{"field":"cf_note","column":"Note"}]}),
    );
    check(
        "duplicate update uses the owner update contract",
        manager
            .pass(update, &update_mapping, false)
            .await?
            .run
            .update
            == 1,
    )?;
    let updated = manager
        .read("core.person.get", json!({"person_id":record_id}))
        .await?;
    check(
        "mapped key changes and unmapped custom keys remain",
        value(&updated, "note") == Some(&json!("Changed"))
            && value(&updated, "amount") == value(&record, "amount"),
    )?;
    let invalid = manager
        .upload(
            "invalid.csv",
            "Name,Email,Site\nInvalid Link,invalid-link@example.test,javascript:alert(1)\n",
        )
        .await?;
    let invalid_mapping = import_mapping(
        json!({"fields":[{"field":"name","column":"Name"},{"field":"cf_site","column":"Site"}]}),
    );
    check(
        "owner semantic validation refuses an invalid link",
        manager
            .pass(invalid, &invalid_mapping, false)
            .await?
            .run
            .error
            == 1,
    )?;
    admin
        .write(
            "base.picklists.value.retire",
            json!({"list":list,"code":choice["code"],"version":choice["version"]}),
        )
        .await?;
    let retired = manager
        .upload("retired.csv", "Name\nRetired Choice\n")
        .await?;
    let retired_mapping = import_mapping(
        json!({"fields":[{"field":"name","column":"Name"},{"field":"cf_category","value":choice["code"]}]}),
    );
    check(
        "new record cannot choose a retired governed value",
        manager
            .pass(retired, &retired_mapping, false)
            .await?
            .run
            .error
            == 1,
    )?;
    stop.send(true)?;
    worker.await??;
    let queued = manager
        .upload(
            "queued.csv",
            "Name,Email,Note\nQueued Import,queued-import@example.test,Must refuse\n",
        )
        .await?;
    manager.write("base.bulk_data.import.start",json!({"run_id":queued,"version":manager.run(queued).await?.version,"mapping":update_mapping})).await?;
    let note = &definitions[0];
    admin
        .write(
            "base.custom_fields.archive",
            json!({"record_type":"person","key":"note","version":note["version"]}),
        )
        .await?;
    check(
        "archived field leaves the import catalog",
        !offered(
            &manager.read("base.bulk_data.types", json!({})).await?,
            "cf_note",
        ),
    )?;
    let stale = manager
        .upload(
            "stale.csv",
            "Name,Email,Note\nStale Import,stale-import@example.test,Must refuse\n",
        )
        .await?;
    check("saved mapping with archived key is refused before writing", manager.write("base.bulk_data.import.start",json!({"run_id":stale,"version":manager.run(stale).await?.version,"mapping":update_mapping})).await.is_err())?;
    check(
        "other tenant cannot reuse this custom mapping",
        other
            .write(
                "base.bulk_data.mapping.save",
                json!({"record_type":"core/person.contacts","name":"Foreign","mapping":mapping}),
            )
            .await
            .is_err(),
    )?;
    let executor = Executor::from_services(
        &app.services,
        Config {
            concurrency: 2,
            poll_interval: Duration::from_millis(50),
            lease_seconds: 10,
            max_runtime: Duration::from_secs(120),
        },
    )?;
    let (stop, receive) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(async move { executor.run(receive).await });
    check(
        "field archived after queueing refuses the row instead of dropping its value",
        manager.settle(queued).await?.run.error == 1,
    )?;
    stop.send(true)?;
    worker.await??;
    println!("Custom import walkthrough passed");
    Ok(())
}
