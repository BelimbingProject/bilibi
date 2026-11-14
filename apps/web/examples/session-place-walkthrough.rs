//! Executable proof that Session records a city and country from a local MMDB
//! file and quietly leaves the place empty for private, unknown and missing
//! database cases.
use bilibi_base::session::{session_id, Sessions, PLACE_DATABASE_PATH_KEY};
use bilibi_database::{pool, Db};
use bilibi_module_registry::{verify_composition, Services};
use mmdb_writer::Writer;
use serde_json::json;
use std::{error::Error, path::PathBuf, sync::Arc};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

async fn add_session(db: &Db, tenant: i64, user: i64) -> Result<(String, String)> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let id = session_id(&token);
    let csrf = uuid::Uuid::new_v4().simple().to_string();
    db.get()
        .await?
        .execute(
            "INSERT INTO sessions(id,tenant_id,user_id,csrf,expires_at) VALUES($1,$2,$3,$4,now()+interval '24 hours')",
            &[&id, &tenant, &user, &csrf],
        )
        .await?;
    Ok((token, id))
}

async fn recorded_place(db: &Db, id: &str) -> Result<(Option<String>, Option<String>)> {
    let client = db.get().await?;
    let session: Option<String> = client
        .query_one("SELECT place FROM sessions WHERE id=$1", &[&id])
        .await?
        .get(0);
    let history: Option<String> = client
        .query_one(
            "SELECT place FROM session_sign_ins WHERE signed_in_at=(SELECT signed_in_at FROM sessions WHERE id=$1) ORDER BY id DESC LIMIT 1",
            &[&id],
        )
        .await?
        .get(0);
    Ok((session, history))
}

fn check(label: &str, condition: bool, detail: impl std::fmt::Debug) -> Result<()> {
    println!(
        "{} {label}: {detail:?}",
        if condition { "ok" } else { "FAIL" }
    );
    condition
        .then_some(())
        .ok_or_else(|| format!("walkthrough check failed: {label}").into())
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_SESSION_PLACE_WALKTHROUGH").as_deref() != Ok("disposable") {
        return Err("set BILIBI_SESSION_PLACE_WALKTHROUGH=disposable".into());
    }
    let root = PathBuf::from(std::env::var("BILIBI_SESSION_PLACE_ROOT")?);
    if !root.is_absolute() {
        return Err("BILIBI_SESSION_PLACE_ROOT must be an absolute path".into());
    }
    std::fs::create_dir_all(&root)?;
    let database_path = root.join("session-place.mmdb");
    let mut writer = Writer::new("Bilibi-Session-Place-Walkthrough");
    writer.insert(
        "81.2.69.160/32".parse::<mmdb_writer::ipnet::IpNet>()?,
        &json!({
            "city": {"names": {"en": "London"}},
            "country": {"names": {"en": "United Kingdom"}}
        }),
    )?;
    std::fs::write(&database_path, writer.to_bytes()?)?;

    bilibi_base::link();
    bilibi_core::link();
    bilibi_domains::link();
    bilibi_extensions::link();
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let modules = bilibi_module_registry::discover()?;
    verify_composition(
        &modules,
        bilibi_domains::MODULES,
        bilibi_extensions::MODULES,
    )?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    db.get().await?.execute(
        "INSERT INTO base_settings(key,tenant_id,scope_type,scope_id,value,is_encrypted) VALUES($1,0,'global',0,$2,false) ON CONFLICT(key,tenant_id,scope_type,scope_id) DO UPDATE SET value=excluded.value",
        &[
            &PLACE_DATABASE_PATH_KEY,
            &json!(database_path.to_string_lossy()),
        ],
    ).await?;
    let mut services = Services::default();
    services.provide("bootstrap.database", Arc::new(db.clone()))?;
    services.provide("bootstrap.composition", Arc::new(bilibi_web::composition()))?;
    services.install(&modules)?;
    let sessions = services.get::<Sessions>("base/session.sessions")?;

    let (token, id) = add_session(&db, 1, 1).await?;
    sessions
        .describe(
            &**db.get().await?,
            &token,
            Some("81.2.69.160"),
            Some("Walkthrough browser"),
        )
        .await?;
    let found = recorded_place(&db, &id).await?;
    check(
        "the city and country are stored with the session and sign-in history",
        found.0.as_deref() == Some("London, United Kingdom") && found.1 == found.0,
        found,
    )?;

    for (label, address) in [
        ("a private address has no place", "10.0.0.1"),
        ("an unknown public address has no place", "8.8.8.8"),
    ] {
        let (token, id) = add_session(&db, 1, 1).await?;
        sessions
            .describe(
                &**db.get().await?,
                &token,
                Some(address),
                Some("Walkthrough browser"),
            )
            .await?;
        let found = recorded_place(&db, &id).await?;
        check(label, found == (None, None), found)?;
    }

    let missing = root.join("missing.mmdb");
    db.get()
        .await?
        .execute(
            "UPDATE base_settings SET value=$2 WHERE key=$1 AND tenant_id=0 AND scope_type='global' AND scope_id=0",
            &[
                &PLACE_DATABASE_PATH_KEY,
                &json!(missing.to_string_lossy()),
            ],
        )
        .await?;
    let (token, id) = add_session(&db, 1, 1).await?;
    sessions
        .describe(
            &**db.get().await?,
            &token,
            Some("81.2.69.160"),
            Some("Walkthrough browser"),
        )
        .await?;
    let found = recorded_place(&db, &id).await?;
    check(
        "a missing database leaves the place empty and the sign-in succeeds",
        found == (None, None),
        found,
    )?;
    Ok(())
}
