//! Contacts search reads the contacts that match, not the whole tenant (#589).
//! On a fresh disposable database: 100,000 contacts in one tenant, half of them
//! with a current work position, then the owner's `search` with a term, which
//! also matches work email, phone, role and company through Grid. Each line
//! prints `ok` or `FAIL`; any `FAIL` exits 1. Contacts and positions are
//! inserted with plain SQL because only their number matters here; every
//! write path is proven by the other walkthroughs.
use bilibi_database::pool;
use bilibi_person::{PersonService, Search};
use bilibi_tenancy::Authentication;
use std::error::Error as Failure;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const CONTACTS: i64 = 100_000;

/// Rows the search can read, so far: of `persons`, `person_affiliations`
/// and `companies`, through each table and every one of its indexes.
async fn rows_read(client: &tokio_postgres::Client) -> Result<i64, Box<dyn Failure>> {
    client
        .execute("SELECT pg_stat_force_next_flush()", &[])
        .await?;
    client
        .execute("SELECT pg_stat_clear_snapshot()", &[])
        .await?;
    let row = client
        .query_one(
            "SELECT coalesce(sum(coalesce(t.seq_tup_read,0) + coalesce((SELECT sum(idx_tup_read) FROM pg_stat_user_indexes i WHERE i.relid=t.relid),0)),0)::bigint FROM pg_stat_user_tables t WHERE t.relname IN ('persons','person_affiliations','companies')",
            &[],
        )
        .await?;
    Ok(row.get(0))
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
    setup
        .batch_execute(&format!(
            "INSERT INTO persons(tenant_id,name,kind,email,phone,code)
               SELECT 1, 'Contact ' || g, 'human', 'contact' || g || '@example.test', '+6012' || lpad(g::text, 7, '0'), 'contact_' || g
               FROM generate_series(1, {CONTACTS}) g;
             INSERT INTO persons(tenant_id,name,kind,code) VALUES
               (1, 'Wanda Needleman', 'human', 'wanda'),
               (1, 'Peter Sam', 'human', 'peter'),
               (1, 'Under_score Person', 'human', 'under'),
               (1, 'Percent 100% Person', 'human', 'percent'),
               (2, 'Wanda Needleman', 'human', 'wanda');
             INSERT INTO person_affiliations(tenant_id,person_id,organisation_id,title,work_email,work_phone)
               SELECT 1, id, 4 + (n % 50), 'Officer', 'w' || n || '@corp.test', '+603' || lpad(n::text, 8, '0')
               FROM (SELECT id, split_part(name, ' ', 2)::int AS n FROM persons WHERE tenant_id=1 AND name LIKE 'Contact %') c
               WHERE n % 2 = 0;
             UPDATE companies SET name='Needlework Trading' WHERE tenant_id=1 AND id=3;
             INSERT INTO person_affiliations(tenant_id,person_id,organisation_id,title)
               SELECT 1, id, 3, 'Clerk' FROM persons WHERE tenant_id=1 AND name='Contact 8888';
             UPDATE person_affiliations SET title='Chief Haystack Officer'
               WHERE tenant_id=1 AND person_id=(SELECT id FROM persons WHERE tenant_id=1 AND name='Contact 7778');
"
        ))
        .await?;
    // As autovacuum would have left a tenant this size: statistics, and the
    // GIN pending lists merged into the trigram indexes.
    for table in ["persons", "person_affiliations", "companies"] {
        setup
            .batch_execute(&format!("VACUUM ANALYZE {table}"))
            .await?;
    }

    let mut failures = 0;
    let mut check = |ok: bool, what: &str| {
        println!("{}: {what}", if ok { "ok" } else { "FAIL" });
        failures += usize::from(!ok);
    };
    let search = |term: &str| Search {
        search: term.into(),
        page_size: 25,
        ..Search::default()
    };
    let names = |page: &bilibi_person::Page| {
        page.entries
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>()
    };

    // One pooled connection, so the search runs on the backend whose
    // statistics are flushed (when it next goes idle) and read below.
    db.resize(1);
    let flush = || async {
        let client = db.get().await?;
        client
            .execute("SELECT pg_stat_force_next_flush()", &[])
            .await?;
        Ok::<_, Box<dyn Failure>>(())
    };
    flush().await?;
    let before = rows_read(&setup).await?;
    let page = people.search(&manager, &search("NEEDLE")).await?;
    flush().await?;
    let read = rows_read(&setup).await? - before;
    check(
        read < CONTACTS / 10,
        &format!("a search with a term read {read} rows, with {CONTACTS} contacts and half as many positions (under {})", CONTACTS / 10),
    );
    check(
        names(&page) == ["Contact 8888", "Wanda Needleman"],
        &format!(
            "it found this tenant's contact named so and the one at Needlework Trading ({:?})",
            names(&page)
        ),
    );

    let page = people.search(&manager, &search("haystack")).await?;
    check(
        names(&page) == ["Contact 7778"],
        &format!("a term matches part of a work role ({:?})", names(&page)),
    );
    let page = people.search(&manager, &search("w7778@corp")).await?;
    check(
        names(&page) == ["Contact 7778"],
        &format!("a term matches part of a work email ({:?})", names(&page)),
    );

    let page = people.search(&manager, &search("r_s")).await?;
    check(
        names(&page) == ["Under_score Person"],
        &format!(
            "`_` in a term matches only an underscore ({:?})",
            names(&page)
        ),
    );
    let page = people.search(&manager, &search("0%")).await?;
    check(
        names(&page) == ["Percent 100% Person"],
        &format!(
            "`%` in a term matches only a percent sign ({:?})",
            names(&page)
        ),
    );
    let page = people.search(&manager, &search("contact19999@")).await?;
    check(
        names(&page) == ["Contact 19999"],
        &format!("a term still matches a personal email ({:?})", names(&page)),
    );
    let page = people.search(&manager, &search("6012001999")).await?;
    check(
        page.total == 10,
        &format!(
            "a term still matches part of a phone ({} contacts)",
            page.total
        ),
    );
    let page = people.search(&manager, &search("contact_1999")).await?;
    check(
        page.total == 11,
        &format!(
            "a term still matches part of a code ({} contacts)",
            page.total
        ),
    );
    let all: i64 = setup
        .query_one(
            "SELECT count(*) FROM persons WHERE tenant_id=1 AND retired_at IS NULL",
            &[],
        )
        .await?
        .get(0);
    let page = people.search(&manager, &search("")).await?;
    check(
        page.total == all,
        &format!(
            "no term lists every contact of the tenant ({} of {all})",
            page.total
        ),
    );

    if failures > 0 {
        return Err(format!("{failures} check(s) failed").into());
    }
    Ok(())
}
