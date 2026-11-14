//! Guarded audit partition removal on a disposable migrated database.
//! Historical months simulate time passing by disabling only their INSERT
//! evidence guard while loading fixtures; production has no clock bypass.
//! Chain triggers remain enabled. Run once per fresh database.
use bilibi_audit::{advance_verification, request_verification, Attribution, AuditLedger};
use bilibi_audit_views::{Partitions, PARTITIONS_SERVICE, RETENTION_DAYS};
use bilibi_database::{pool, Error, MutationBoundary};
use bilibi_settings::{Settings, Target};
use serde_json::json;
use tokio_postgres::Client;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();
type Outcome<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn check(label: &str, holds: bool) -> Outcome {
    println!("{label}: {}", if holds { "ok" } else { "FAILED" });
    if holds {
        Ok(())
    } else {
        Err(format!("{label}: did not hold").into())
    }
}
async fn yes(client: &Client, sql: &str) -> Outcome<bool> {
    Ok(client.query_one(sql, &[]).await?.get(0))
}
async fn whole(client: &Client) -> Outcome {
    for head in client
        .query(
            "SELECT chain,tenant_id FROM base_audit_chain_heads ORDER BY chain,tenant_id",
            &[],
        )
        .await?
    {
        let chain: String = head.get(0);
        let tenant: i64 = head.get(1);
        let mut seq = 0_i64;
        let mut hash = Vec::<u8>::new();
        loop {
            let row = client
                .query_one(
                    "SELECT * FROM base_audit_verify_range($1,$2,$3,$4,2)",
                    &[&chain, &tenant, &seq, &hash],
                )
                .await?;
            if let Some(reason) = row.get::<_, Option<String>>(6) {
                return Err(format!("{chain} tenant {tenant}: {reason}").into());
            }
            seq = row.get(1);
            hash = row.get(2);
            if row.get::<_, bool>(3) {
                break;
            }
        }
    }
    Ok(())
}
async fn history(client: &mut Client, month: &str) -> Outcome {
    let suffix = month.replace('-', "");
    let suffix = &suffix[..6];
    for chain in ["actions", "mutations"] {
        client
            .execute(
                "SELECT base_audit_create_partition($1,$2::text::date)",
                &[&chain, &month],
            )
            .await?;
        let tx = client.transaction().await?;
        // Only fixture-generated identifiers, on this example's disposable DB.
        tx.batch_execute(&format!("ALTER TABLE base_audit_{chain}_{suffix} DISABLE TRIGGER base_audit_{chain}_evidence_guard")).await?;
        let columns = if chain == "actions" {
            "event,payload"
        } else {
            "event,old_values,new_values,auditable_type,auditable_id"
        };
        let values = if chain == "actions" {
            "'walkthrough.partition','{\"detail\":\"keep exactly\"}'::jsonb"
        } else {
            "'created','{}'::jsonb,'{\"detail\":\"keep exactly\"}'::jsonb,'walkthrough.partition',g::text"
        };
        tx.execute(&format!("INSERT INTO base_audit_{chain}(partition_month,recorded_at,occurred_at,tenant_id,actor_type,actor_id,is_retained,{columns}) \
            SELECT $1::text::date,($1::text::date+interval '2 days') AT TIME ZONE 'UTC',($1::text::date+interval '2 days') AT TIME ZONE 'UTC', \
            CASE WHEN g=5 THEN NULL WHEN g=6 THEN 2 ELSE 1 END,'user',1,g=2,{values} FROM generate_series(1,6) g"),&[&month]).await?;
        tx.batch_execute(&format!("SET CONSTRAINTS base_audit_{chain}_chain IMMEDIATE; ALTER TABLE base_audit_{chain}_{suffix} ENABLE TRIGGER base_audit_{chain}_evidence_guard")).await?;
        tx.commit().await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Outcome {
    if std::env::var("BILIBI_DISPOSABLE_AUDIT_FIXTURE").as_deref() != Ok("yes") {
        return Err("Set BILIBI_DISPOSABLE_AUDIT_FIXTURE=yes and DATABASE_URL to a disposable migrated database.".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    let app = bilibi_web::App::new(
        db.clone(),
        &std::env::var("APP_ORIGIN").unwrap_or_else(|_| "http://localhost:35579".into()),
    )?;
    app.services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let login = app
        .user
        .login("admin@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let admin = app.security.guard(&login.token).await?;
    let login = app
        .user
        .login("staff@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let staff = app.security.guard(&login.token).await?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let settings = app.services.get::<Settings>("base/settings.store")?;
    let partitions = app.services.get::<Partitions>(PARTITIONS_SERVICE)?;
    let mut client = db.get().await?;
    client.execute("INSERT INTO platform_operator(singleton,tenant_id) VALUES(true,1) ON CONFLICT DO NOTHING",&[]).await?;
    check("both ledgers are partitioned",yes(&client,"SELECT count(*)=2 FROM pg_class WHERE oid IN ('base_audit_actions'::regclass,'base_audit_mutations'::regclass) AND relkind='p'").await?)?;
    for month in ["2019-01-01", "2019-02-01", "2019-03-01", "2019-01-01"] {
        history(&mut client, month).await?;
    }
    // A bookmark overrides the recorded false bit; a false bookmark releases
    // a recorded true bit. The composite FK follows the preserved bookmark.
    client.batch_execute("INSERT INTO base_audit_action_metadata(partition_month,action_id,is_retained) \
        SELECT partition_month,min(id),true FROM base_audit_actions WHERE partition_month='2019-01-01' GROUP BY partition_month; \
        INSERT INTO base_audit_action_metadata(partition_month,action_id,is_retained) \
        SELECT partition_month,max(id),false FROM base_audit_actions WHERE partition_month='2019-01-01' AND is_retained GROUP BY partition_month; \
        CREATE TEMP TABLE kept_actions AS SELECT a.id,md5((to_jsonb(a)-'partition_month')::text) AS fingerprint \
        FROM base_audit_actions a LEFT JOIN base_audit_action_metadata b ON b.partition_month=a.partition_month AND b.action_id=a.id \
        WHERE a.partition_month='2019-01-01' AND coalesce(b.is_retained,a.is_retained); \
        CREATE TEMP TABLE kept_mutations AS SELECT id,md5((to_jsonb(m)-'partition_month')::text) AS fingerprint \
        FROM base_audit_mutations m WHERE partition_month='2019-01-01' AND is_retained;").await?;
    whole(&client).await?;
    check(
        "no removal without configured retention",
        matches!(
            partitions
                .preview(&**client, &admin, AuditLedger::Actions, "2019-01-01")
                .await,
            Err(Error::Invalid)
        ),
    )?;
    let mut tx = boundary
        .begin(
            &mut client,
            admin.mutation_context("walkthrough.partition.settings"),
        )
        .await?;
    settings
        .put(&mut tx, &admin, Target::Global, RETENTION_DAYS, json!(30))
        .await?;
    tx.commit().await?;
    check(
        "non-operator cannot preview removal",
        matches!(
            partitions
                .preview(&**client, &staff, AuditLedger::Actions, "2019-01-01")
                .await,
            Err(Error::Forbidden)
        ),
    )?;
    let current: String = client
        .query_one(
            "SELECT date_trunc('month',clock_timestamp() AT TIME ZONE 'UTC')::date::text",
            &[],
        )
        .await?
        .get(0);
    check(
        "current month cannot be removed",
        matches!(
            partitions
                .preview(&**client, &admin, AuditLedger::Actions, &current)
                .await,
            Err(Error::Invalid)
        ),
    )?;
    let mut tx = boundary
        .begin(
            &mut client,
            admin.mutation_context("walkthrough.partition.verify"),
        )
        .await?;
    let run = request_verification(&mut tx, &admin).await?;
    tx.commit().await?;
    check(
        "running verification blocks removal",
        matches!(
            partitions
                .preview(&**client, &admin, AuditLedger::Actions, "2019-01-01")
                .await,
            Err(Error::Conflict)
        ),
    )?;
    while advance_verification(&mut client, run, &Attribution::from_scope(&admin))
        .await?
        .running()
    {}
    let mut other = db.get().await?;
    let holding = other.transaction().await?;
    holding
        .query(
            "SELECT id FROM base_audit_actions WHERE partition_month='2019-01-01' FOR UPDATE",
            &[],
        )
        .await?;
    let mut blocked = boundary
        .begin(
            &mut client,
            admin.mutation_context("walkthrough.partition.locked"),
        )
        .await?;
    check(
        "an in-flight bookmark or writer blocks removal",
        partitions
            .remove(
                &mut blocked,
                &admin,
                AuditLedger::Actions,
                "2019-01-01",
                "Lock proof",
            )
            .await
            .is_err(),
    )?;
    check(
        "a refused removal poisons its transaction",
        blocked.commit().await.is_err(),
    )?;
    holding.rollback().await?;
    drop(other);
    let mut tx = boundary
        .begin(
            &mut client,
            admin.mutation_context("walkthrough.partition.rollback"),
        )
        .await?;
    partitions
        .remove(
            &mut tx,
            &admin,
            AuditLedger::Actions,
            "2019-01-01",
            "Rollback proof",
        )
        .await?;
    drop(tx);
    check("rollback restores partition and removes its ledger",yes(&client,"SELECT to_regclass('base_audit_actions_201901') IS NOT NULL AND NOT EXISTS(SELECT 1 FROM base_audit_partition_removals)").await?)?;
    for month in ["2019-01-01", "2019-02-01"] {
        for ledger in [AuditLedger::Actions, AuditLedger::Mutations] {
            let preview = partitions.preview(&**client, &admin, ledger, month).await?;
            let mut tx = boundary
                .begin(
                    &mut client,
                    admin.mutation_context("walkthrough.partition.remove"),
                )
                .await?;
            let result = partitions
                .remove(&mut tx, &admin, ledger, month, "Expired evidence month")
                .await?;
            tx.commit().await?;
            check(
                "preview matches committed counts",
                preview.removed == result.removed && preview.retained == result.retained,
            )?;
            println!(
                "{} {month}: removed {}, retained {}",
                ledger.as_str(),
                result.removed,
                result.retained
            );
            whole(&client).await?;
        }
    }
    check("January and February leaves are gone",yes(&client,"SELECT to_regclass('base_audit_actions_201901') IS NULL AND to_regclass('base_audit_mutations_201901') IS NULL AND to_regclass('base_audit_actions_201902') IS NULL AND to_regclass('base_audit_mutations_201902') IS NULL").await?)?;
    check("retained actions survive byte-for-byte",yes(&client,"SELECT count(*)=(SELECT count(*) FROM kept_actions) FROM kept_actions k JOIN base_audit_actions a ON a.id=k.id AND a.partition_month='infinity' WHERE md5((to_jsonb(a)-'partition_month')::text)=k.fingerprint").await?)?;
    check("retained mutations survive byte-for-byte",yes(&client,"SELECT count(*)=(SELECT count(*) FROM kept_mutations) FROM kept_mutations k JOIN base_audit_mutations m ON m.id=k.id AND m.partition_month='infinity' WHERE md5((to_jsonb(m)-'partition_month')::text)=k.fingerprint").await?)?;
    check("bookmarks follow retained actions",yes(&client,"SELECT count(*)=1 FROM base_audit_action_metadata WHERE partition_month='infinity' AND is_retained").await?)?;
    for sql in [
        "DELETE FROM base_audit_actions WHERE partition_month='infinity'",
        "DELETE FROM base_audit_mutations WHERE partition_month='infinity'",
        "TRUNCATE base_audit_actions",
        "TRUNCATE base_audit_mutations",
        "TRUNCATE base_audit_actions_201903",
        "TRUNCATE base_audit_mutations_retained",
        "UPDATE base_audit_actions SET event='tampered' WHERE partition_month='infinity'",
        "DELETE FROM base_audit_partition_removals",
        "DELETE FROM base_audit_partition_checkpoints",
        "SELECT base_audit_create_partition('actions','2019-01-01')",
    ] {
        check(
            "unguarded evidence change refused",
            client.batch_execute(sql).await.is_err(),
        )?;
    }
    // Corrupt a surviving fact behind the guard. Verification must still fail.
    client.batch_execute("SET session_replication_role=replica; UPDATE base_audit_actions SET event='tampered' WHERE id=(SELECT min(id) FROM kept_actions); SET session_replication_role=origin").await?;
    check(
        "surviving-row tampering is detected",
        whole(&client).await.is_err(),
    )?;
    println!("Partition retention walkthrough passed");
    Ok(())
}
