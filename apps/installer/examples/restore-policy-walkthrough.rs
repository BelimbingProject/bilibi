#![allow(
    dead_code,
    reason = "the walkthrough imports installer modules whose complete production surface is not exercised"
)]
#[path = "../src/backup.rs"]
mod backup;
#[path = "../src/composition.rs"]
mod composition;
#[path = "../src/jobs.rs"]
mod jobs;
#[path = "../src/layout.rs"]
mod layout;

use serde_json::json;
use std::{error::Error, os::unix::fs::PermissionsExt, path::PathBuf};
use tokio_postgres::NoTls;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let url = std::env::var("DATABASE_URL")?;
    let root = PathBuf::from(std::env::var("RESTORE_POLICY_ROOT")?);
    std::fs::create_dir_all(&root)?;
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await?;
    tokio::spawn(async move { connection.await.unwrap() });
    assert!(
        client
            .query_one("SELECT to_regclass('base_settings') IS NULL", &[])
            .await?
            .get::<_, bool>(0),
        "use an empty disposable database"
    );
    client
        .batch_execute(include_str!(
            "../../base/settings/migrations/001-settings.sql"
        ))
        .await?;
    client
        .batch_execute(
            "CREATE TABLE base_software_jobs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY);
         CREATE TABLE base_software_steps (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY);
         CREATE TABLE base_software_plugins (tenant_id bigint, plugin_id text);
         CREATE TABLE base_audit_actions (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY);
         CREATE TABLE base_audit_mutations (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY);
         INSERT INTO base_settings (key,tenant_id,scope_type,scope_id,value,is_encrypted)
         VALUES ('installation.mode',0,'global',0,'\"isolated\"',false),
                ('unrelated',0,'global',0,'\"keep\"',false);",
        )
        .await?;
    let customer = root.join("customer");
    let reviewed = root.join("reviewed");
    for (dir, extensions) in [
        (
            &customer,
            "Extensions:\n  local unpinned, code: customer, modules: custom",
        ),
        (&reviewed, "Extensions: none mounted"),
    ] {
        std::fs::create_dir_all(dir)?;
        let program = dir.join("bilibi-web");
        std::fs::write(&program, format!("#!/bin/sh\n[ \"$1\" = status ] || exit 2\nprintf '%s\\n' 'Bilibi 0.1.0\nDomains: none mounted\n{extensions}'\n"))?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
    }
    let pg = backup::PgEnv::from_url(&url)?;
    let dump = root.join("isolated.dump");
    jobs::lock_installation(&client).await?;
    composition::admit(
        &composition::status(&customer)?,
        &jobs::installation_mode(&client).await?,
    )?;
    backup::dump(&pg, &dump)?;
    composition::admit(&composition::status(&reviewed)?, "isolated")?;
    client.batch_execute("CREATE TABLE release_migration (id bigint); INSERT INTO release_migration VALUES (731);
        UPDATE base_settings SET value='\"shared\"' WHERE key='installation.mode'").await?;
    let refused = backup::restore(
        &client,
        &pg,
        &dump,
        "",
        &root.join("history-undo.json"),
        Some(&customer),
    )
    .await;
    let message = refused
        .err()
        .expect("Undo must refuse customer code under the current Shared policy");
    assert!(
        message.contains("Restore refused") && message.contains("local"),
        "{message}"
    );
    assert_eq!(jobs::installation_mode(&client).await?, "shared");
    assert_eq!(
        client
            .query_one("SELECT id FROM release_migration", &[])
            .await?
            .get::<_, i64>(0),
        731
    );
    assert!(!root.join("history-undo.json").exists());
    println!("PASS: isolated customer snapshot, migrated code-free release, Shared policy, Undo refused without changing policy or schema");

    backup::restore(
        &client,
        &pg,
        &dump,
        "",
        &root.join("history-rollback.json"),
        Some(&reviewed),
    )
    .await?;
    assert_eq!(jobs::installation_mode(&client).await?, "shared");
    assert!(client
        .query_one("SELECT to_regclass('release_migration') IS NULL", &[])
        .await?
        .get::<_, bool>(0));
    assert_eq!(
        client
            .query_one(
                "SELECT value #>> '{}' FROM base_settings WHERE key='unrelated'",
                &[]
            )
            .await?
            .get::<_, String>(0),
        "keep"
    );
    println!("PASS: admitted rollback restores data while preserving Shared policy");

    let shared_dump = root.join("shared.dump");
    backup::dump(&pg, &shared_dump)?;
    client
        .batch_execute(
            "UPDATE base_settings SET value='\"isolated\"' WHERE key='installation.mode'",
        )
        .await?;
    backup::restore(
        &client,
        &pg,
        &shared_dump,
        "",
        &root.join("history-isolated.json"),
        Some(&customer),
    )
    .await?;
    assert_eq!(jobs::installation_mode(&client).await?, "isolated");
    println!("PASS: admitted customer restore preserves Isolated policy over a Shared dump");

    let mut history = jobs::history(&client, "").await?;
    history["target_composition"] = json!(composition::status(&customer)?);
    let interrupted = root.join("history-interrupted.json");
    std::fs::write(
        &interrupted,
        json!({"dump": shared_dump, "history": history}).to_string(),
    )?;
    client
        .batch_execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public")
        .await?;
    backup::restore(&client, &pg, &shared_dump, "", &interrupted, None).await?;
    assert_eq!(jobs::installation_mode(&client).await?, "isolated");
    println!("PASS: interrupted recovery uses the durable pre-restore policy");
    Ok(())
}
