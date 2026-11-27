//! A default contact code costs the same however many people share its base
//! (#591). On a fresh disposable database: 20,000 contacts already named
//! "Muhammad Hafiz bin Ismail", then the owner's preview and create. Each line
//! prints `ok` or `FAIL`; any `FAIL` exits 1. The existing contacts are
//! inserted with plain SQL because only their codes matter here; every write
//! path is proven by the other walkthroughs.
use bilibi_database::pool;
use bilibi_party::{default_code, CodeKind};
use bilibi_person::{PersonInput, PersonKind, PersonService};
use bilibi_tenancy::Authentication;
use std::error::Error as Failure;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const SHARED: u64 = 20_000;
const MUHAMMAD: &str = "Muhammad Hafiz bin Ismail";

/// Rows of `persons` read so far by this database, through its table and
/// every one of its indexes (an index-only scan reads no table row).
async fn rows_read(client: &tokio_postgres::Client) -> Result<i64, Box<dyn Failure>> {
    client
        .execute("SELECT pg_stat_force_next_flush()", &[])
        .await?;
    let row = client
        .query_one(
            "SELECT coalesce(t.seq_tup_read,0) + coalesce((SELECT sum(idx_tup_read) FROM pg_stat_user_indexes i WHERE i.relid=t.relid),0)::bigint FROM pg_stat_user_tables t WHERE t.relname='persons'",
            &[],
        )
        .await?;
    Ok(row.get(0))
}
fn input(name: &str) -> PersonInput {
    PersonInput {
        name: name.into(),
        kind: PersonKind::Human,
        email: None,
        phone: None,
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

    let (setup, connection) =
        tokio_postgres::connect(&std::env::var("DATABASE_URL")?, tokio_postgres::NoTls).await?;
    tokio::spawn(connection);
    let mut names = Vec::new();
    let mut codes = Vec::new();
    for suffix in 1..=SHARED {
        names.push(MUHAMMAD.to_owned());
        codes.push(default_code(MUHAMMAD, CodeKind::Person, suffix));
    }
    // "Kam Pui Yue" has used every one-digit suffix; an explicit upper-case
    // code and a zero-padded one look like suffixes and are not.
    for suffix in 1..=9 {
        names.push("Kam Pui Yue".into());
        codes.push(default_code("Kam Pui Yue", CodeKind::Person, suffix));
    }
    names.extend(["Kam Pui Yue".into(), "Kam Pui Yue".into()]);
    codes.extend(["KAM_PUI_10".into(), "kam_pui_05".into()]);
    setup
        .execute(
            "INSERT INTO persons(tenant_id,name,kind,code) SELECT 1, n, 'human', c FROM unnest($1::text[], $2::text[]) AS t(n, c)",
            &[&names, &codes],
        )
        .await?;
    setup.batch_execute("ANALYZE persons").await?;

    let mut failures = 0;
    let mut check = |ok: bool, what: &str| {
        println!("{}: {what}", if ok { "ok" } else { "FAIL" });
        failures += usize::from(!ok);
    };

    // One pooled connection, so the preview runs on the backend whose
    // statistics are flushed (when it next goes idle) and read below.
    db.resize(1);
    let client = db.get().await?;
    client
        .execute("SELECT pg_stat_force_next_flush()", &[])
        .await?;
    let before = rows_read(&setup).await?;
    let preview = bilibi_person::preview_person(&**client, &manager, &input(MUHAMMAD))
        .await
        .map_err(|e| format!("{e:?}"))?;
    client
        .execute("SELECT pg_stat_force_next_flush()", &[])
        .await?;
    drop(client);
    let read = rows_read(&setup).await? - before;
    check(
        preview.code == "muhamm_20001",
        &format!(
            "the preview suggests the code after the {SHARED} taken ones ({})",
            preview.code
        ),
    );
    check(
        read < 100,
        &format!("finding it read {read} contact rows, with {SHARED} sharing the base (under 100)"),
    );

    let created = people
        .create(&manager, &input(MUHAMMAD))
        .await
        .map_err(|e| format!("{e:?}"))?;
    check(
        created.code == "muhamm_20001",
        &format!("the create stores that code ({})", created.code),
    );
    let created = people
        .create(&manager, &input(MUHAMMAD))
        .await
        .map_err(|e| format!("{e:?}"))?;
    check(
        created.code == "muhamm_20002",
        &format!(
            "the next create takes the number after it ({})",
            created.code
        ),
    );
    let created = people
        .create(&manager, &input("Kam Pui Yue"))
        .await
        .map_err(|e| format!("{e:?}"))?;
    check(
        created.code == "kam_pui_10",
        &format!(
            "after the last one-digit suffix comes the first two-digit one ({})",
            created.code
        ),
    );
    let created = people
        .create(&manager, &input("Siti Aminah"))
        .await
        .map_err(|e| format!("{e:?}"))?;
    check(
        created.code == "siti_aminah",
        &format!(
            "a name nobody has keeps its unsuffixed code ({})",
            created.code
        ),
    );
    let created = people
        .create(&manager, &input("Siti Aminah"))
        .await
        .map_err(|e| format!("{e:?}"))?;
    check(
        created.code == "siti_2",
        &format!("the second one gets `_2` ({})", created.code),
    );

    if failures > 0 {
        return Err(format!("{failures} check(s) failed").into());
    }
    Ok(())
}
