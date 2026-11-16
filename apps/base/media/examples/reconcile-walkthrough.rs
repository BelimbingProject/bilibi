//! Walkthrough of disk reconciliation (issue 599) against an explicitly
//! supplied disposable DB (`BILIBI_DISPOSABLE_MEDIA_DEMO=yes`, `DATABASE_URL`,
//! `BILIBI_MEDIA_DEMO_ROOT`). It stores two files, removes one of them behind
//! Media's back and leaves files on the disk that no row names, then checks
//! that `Media::reconcile` (the `bilibi-web media-reconcile` command) lists
//! exactly the asset with no file and the old unnamed file, skips a file
//! still settling and a staging file, and changes nothing. Exits non-zero
//! when a check fails.
use bilibi_database::{pool, MutationBoundary, Result as DbResult};
use bilibi_media::{Media, MissingFile, Upload};
use bilibi_module_registry::Services;
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::Authentication;
use serde_json::json;
use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};

fn internal<T>(_: T) -> bilibi_database::Error {
    bilibi_database::Error::Internal
}
/// Writes `path` and dates it `age` back, as a file left behind earlier.
fn leave(path: &Path, age: Duration) -> DbResult<()> {
    std::fs::create_dir_all(path.parent().ok_or(bilibi_database::Error::Internal)?)
        .map_err(internal)?;
    std::fs::write(path, b"left behind\n").map_err(internal)?;
    date(path, age)
}
fn date(path: &Path, age: Duration) -> DbResult<()> {
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::now() - age))
        .map_err(internal)
}
fn check(ok: bool, what: &str) -> DbResult<()> {
    if ok {
        println!("  ok: {what}");
        Ok(())
    } else {
        println!("  FAIL {what}");
        Err(bilibi_database::Error::Internal)
    }
}

#[tokio::main]
async fn main() -> DbResult<()> {
    if std::env::var("BILIBI_DISPOSABLE_MEDIA_DEMO").as_deref() != Ok("yes") {
        return Err(bilibi_database::Error::Invalid);
    }
    bilibi_base::link();
    bilibi_core::link();
    let root =
        std::env::var("BILIBI_MEDIA_DEMO_ROOT").map_err(|_| bilibi_database::Error::Invalid)?;
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| bilibi_database::Error::Invalid)?)?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let mut services = Services::default();
    services.provide("bootstrap.database", Arc::new(db.clone()))?;
    services.provide("bootstrap.composition", Arc::new(bilibi_web::composition()))?;
    services.install(&modules)?;
    let media = services.get::<Media>("base/media.store")?;
    let settings = services.get::<Settings>("base/settings.store")?;
    let boundary = services.get::<MutationBoundary>("base/database.mutations")?;
    let mut client = db.get().await?;
    let actor = services
        .get::<Authentication>("base/tenancy.authentication")?
        .authenticate(&**client, 1, 1, "walkthrough".into())
        .await?;
    std::fs::create_dir_all(&root).map_err(internal)?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).map_err(internal)?;
    let mut tx = boundary
        .begin(
            &mut client,
            actor.mutation_context("media.walkthrough.configure"),
        )
        .await?;
    settings
        .put(
            &mut tx,
            &actor,
            Target::Global,
            "media.disks",
            json!({"local": root}),
        )
        .await?;
    tx.commit().await?;

    println!("== two stored files, one removed behind Media's back");
    let mut stored = Vec::new();
    for name in ["kept.txt", "lost.txt"] {
        let upload = Upload {
            filename: name.into(),
            bytes: format!("{name}\n").into_bytes(),
        };
        let asset = media
            .put_upload(&mut client, &actor, "local", "walkthrough", upload, None)
            .await
            .map_err(internal)?;
        let key: String = client
            .query_one(
                "SELECT storage_key FROM base_media_assets WHERE id=$1",
                &[&asset.id],
            )
            .await?
            .get(0);
        stored.push((asset.id, key));
    }
    let root = Path::new(&root);
    let hours = |n: u64| Duration::from_secs(n * 3600);
    // Old enough to be listed if nothing named it.
    date(&root.join(&stored[0].1), hours(2))?;
    std::fs::remove_file(root.join(&stored[1].1)).map_err(internal)?;
    println!("== files no row names");
    let stray = "tenants/1/walkthrough/stray";
    leave(&root.join(stray), hours(2))?;
    leave(&root.join("tenants/1/walkthrough/settling"), Duration::ZERO)?;
    leave(
        &root.join("staging/tenants/1/accounts/1/old-chunks"),
        hours(48),
    )?;

    println!("== reconcile");
    let found = media.reconcile(&**client).await.map_err(internal)?;
    println!("files no row needs: {:?}", found.unneeded_files);
    println!("assets with no file: {:?}", found.missing_files);
    println!("unreadable disks: {:?}", found.unreadable_disks);
    check(
        found.unneeded_files == [("local".to_owned(), stray.to_owned())],
        "the only file listed is the old one no row names (not the stored, settling or staging file)",
    )?;
    check(
        found.missing_files
            == [MissingFile {
                id: stored[1].0,
                tenant: 1,
                disk: "local".into(),
                key: stored[1].1.clone(),
            }],
        "the only asset listed is the one whose file was removed",
    )?;
    check(found.unreadable_disks.is_empty(), "every disk was read")?;
    check(
        root.join(stray).exists() && root.join(&stored[0].1).exists(),
        "nothing was removed",
    )?;
    Ok(())
}
