//! A failed restore is recorded once, even when the installer exits during
//! the cleanup after the failure (#914). It runs the real installer, as a
//! child process, against a disposable instance folder and database: an Undo
//! is running its restore step, and `pg_restore` (a stand-in on `PATH`,
//! beside stand-ins for `sudo` and `systemctl`) fails.
//!
//! 1. Without a restart, the installer records the failed restore itself,
//!    names the safety backup to restore by hand, and keeps no failure note.
//! 2. The installer is killed after the failure is noted and while its
//!    cleanup is clearing the database, before the failed step is recorded.
//!    The next start records that failure once, with the same manual
//!    recovery dump, and never runs `pg_restore` again; a third start finds
//!    nothing to do.
//! 3. The failure note cannot be written (its partial file's path is taken
//!    by a folder). The installer clears nothing more, holds the saved
//!    history, and records the failure with the dump to restore by hand.
//! 4. The same, but the installer is killed after it held the history and
//!    before the failed step is recorded. The next start records the failure
//!    and never runs `pg_restore` again, and what the failed restore left is
//!    still not cleared.
//!
//! Set `DATABASE_URL` to an empty disposable database and
//! `RESTORE_FAILURE_ROOT` to a private scratch directory.
#![expect(
    dead_code,
    reason = "the walkthrough imports the job rows module whose complete production surface is not exercised"
)]
#[path = "../src/jobs.rs"]
mod jobs;

use serde_json::{json, Value};
use std::{
    error::Error,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tokio_postgres::{Client, NoTls};

/// The instance folder's name, and so the worker name the job rows carry.
const INSTANCE: &str = "walk";
const UNDO_JOB: i64 = 2;
/// A release composition the Shared installation policy admits.
const TARGET: &str = "Bilibi 0.1.0\nDomains: none mounted\nExtensions: none mounted\n";

/// `pg_restore` that records each call and fails. With `NO_NOTE` set it
/// leaves a table behind, as a restore that failed part way would, and makes
/// a folder at `NO_NOTE`, the failure note's partial file, so the note cannot
/// be written. With `HOLD` set it then opens a transaction that creates the
/// table `HOLD` in `public` and stays open, so the installer waits for it
/// (dropping that schema, or making that job table again): the window in
/// which the failure is decided and not yet recorded.
const PG_RESTORE: &str = r#"#!/bin/sh
echo "pg_restore $*" >> "$WALK/pg_restore.calls"
if [ -n "$NO_NOTE" ]; then
  psql --no-psqlrc --quiet -c 'CREATE TABLE public.restore_failure_partial ()' >/dev/null 2>&1
  mkdir -p "$NO_NOTE"
fi
if [ -n "$HOLD" ]; then
  PGAPPNAME=restore-failure-hold psql --no-psqlrc --quiet >/dev/null 2>&1 <<SQL &
BEGIN;
CREATE TABLE public.$HOLD ();
\! touch "$WALK/held"
SELECT pg_sleep(300);
SQL
  tries=0
  while [ ! -e "$WALK/held" ] && [ "$tries" -lt 300 ]; do sleep 0.1; tries=$((tries + 1)); done
fi
echo "pg_restore: error: the walkthrough archive cannot be read" >&2
exit 1
"#;
/// `sudo -n systemctl …` and `systemctl …`: the service is a stand-in.
const SUDO: &str = "#!/bin/sh\necho \"sudo $*\" >> \"$WALK/service.calls\"\n";
const SYSTEMCTL: &str = "#!/bin/sh\necho inactive\n";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [mode, dir] = args.as_slice() {
        if mode == "installer" {
            // The child: the installer itself, as `bilibi-installer <dir>`.
            let installer = bilibi_installer::Installer::open(Path::new(dir))?;
            return installer.run().await.map_err(Into::into);
        }
    }
    let url = std::env::var("DATABASE_URL")?;
    let root = PathBuf::from(std::env::var("RESTORE_FAILURE_ROOT")?);
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await?;
    tokio::spawn(async move { connection.await.unwrap() });
    assert!(
        client
            .query_one("SELECT to_regclass('base_software_jobs') IS NULL", &[])
            .await?
            .get::<_, bool>(0),
        "use an empty disposable database"
    );
    let dbname: String = client
        .query_one("SELECT current_database()", &[])
        .await?
        .get(0);
    let walk = Walk::new(&root, &url, &dbname)?;

    // 1. A failure the installer records without a restart.
    walk.prepare(&client).await?;
    let output = walk.installer(&[])?.wait_with_output()?;
    let log = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{log}");
    assert_eq!(walk.restore_calls(), 1, "{log}");
    walk.assert_failed_once(&client, PG_RESTORE_FAILED).await?;
    println!("PASS: a failed restore is recorded once, names the safety backup to restore by hand, and leaves no failure note");

    // 2. The installer exits during the cleanup after the failure.
    walk.prepare(&client).await?;
    walk.cut_off(&client, &[("HOLD", "restore_failure_hold")], "DROP SCHEMA%")
        .await?;
    assert_eq!(walk.restore_calls(), 1);
    assert!(walk.history().is_file());
    let noted = walk.note().is_file();
    assert!(
        !table_exists(&client, "base_software_jobs").await?,
        "the installer was stopped before the job tables were made again"
    );
    println!("PASS: the installer exited during the cleanup after the failure, with the job tables gone and the restore step not recorded");

    assert!(noted, "the failure was not noted before the cleanup");
    walk.restart_records_failure().await?;
    let finished = walk.assert_failed_once(&client, PG_RESTORE_FAILED).await?;
    println!("PASS: the restart recorded the failure once with the manual recovery dump and ran no restore");

    let output = walk.installer(&[])?.wait_with_output()?;
    let log = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{log}");
    assert_eq!(walk.restore_calls(), 1, "{log}");
    assert_eq!(
        walk.assert_failed_once(&client, PG_RESTORE_FAILED).await?,
        finished
    );
    println!("PASS: a later start finds nothing to do and leaves the recorded outcome as it is");

    // 3. The failure note cannot be written.
    let no_note = walk.note().with_extension("failure.partial");
    let no_note = no_note.to_str().ok_or("the instance path is not UTF-8")?;
    walk.prepare(&client).await?;
    let output = walk
        .installer(&[("NO_NOTE", no_note)])?
        .wait_with_output()?;
    let log = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{log}");
    assert_eq!(walk.restore_calls(), 1, "{log}");
    walk.assert_failed_once(&client, "Nothing more was cleared.")
        .await?;
    walk.assert_failed_once(&client, PG_RESTORE_FAILED).await?;
    assert!(table_exists(&client, "restore_failure_partial").await?);
    println!("PASS: when the failure cannot be noted, nothing more is cleared and the failure is recorded with the dump to restore by hand");

    // 4. The failure note cannot be written, and the installer exits before
    //    the failed step is recorded.
    walk.prepare(&client).await?;
    walk.cut_off(
        &client,
        &[("NO_NOTE", no_note), ("HOLD", "base_settings")],
        "CREATE TABLE IF NOT EXISTS base_settings%",
    )
    .await?;
    assert_eq!(walk.restore_calls(), 1);
    assert!(
        !walk.history().exists() && walk.held().is_file() && !walk.note().exists(),
        "the saved history was not held, or a note was written"
    );
    assert!(table_exists(&client, "restore_failure_partial").await?);
    assert!(!table_exists(&client, "base_software_jobs").await?);
    println!("PASS: the installer exited after holding the saved history, with no failure note, nothing more cleared and the restore step not recorded");

    walk.restart_records_failure().await?;
    walk.assert_failed_once(&client, "its failure could not be noted")
        .await?;
    assert!(table_exists(&client, "restore_failure_partial").await?);
    println!("PASS: the restart recorded the held failure with the manual recovery dump, ran no restore and cleared nothing");
    Ok(())
}

/// What the stand-in `pg_restore`'s failure reads as in the job's message.
const PG_RESTORE_FAILED: &str = "pg_restore exited with";

async fn table_exists(client: &Client, table: &str) -> Result<bool, Box<dyn Error>> {
    Ok(client
        .query_one("SELECT to_regclass($1) IS NOT NULL", &[&table])
        .await?
        .get(0))
}

struct Walk {
    root: PathBuf,
    instance: PathBuf,
    url: String,
    /// The safety backup the Undo took, which the failure names for a
    /// restore by hand.
    safety: PathBuf,
    /// The backup taken before the install, which the Undo restores.
    original: PathBuf,
}
impl Walk {
    fn new(root: &Path, url: &str, dbname: &str) -> Result<Self, Box<dyn Error>> {
        let instance = root.join(INSTANCE);
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin)?;
        for (name, body) in [
            ("pg_restore", PG_RESTORE),
            ("sudo", SUDO),
            ("systemctl", SYSTEMCTL),
        ] {
            std::fs::write(bin.join(name), body)?;
            std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            safety: instance
                .join("backups")
                .join(format!("job-{UNDO_JOB}"))
                .join(format!("{dbname}.dump")),
            original: instance
                .join("backups")
                .join("job-1")
                .join(format!("{dbname}.dump")),
            root: root.to_path_buf(),
            instance,
            url: url.to_owned(),
        })
    }
    fn history(&self) -> PathBuf {
        self.safety.with_file_name("history-restore.json")
    }
    fn note(&self) -> PathBuf {
        self.safety.with_file_name("history-restore.failure")
    }
    fn held(&self) -> PathBuf {
        self.safety.with_file_name("history-restore.held")
    }
    fn restore_calls(&self) -> usize {
        std::fs::read_to_string(self.root.join("pg_restore.calls"))
            .map_or(0, |calls| calls.lines().count())
    }
    /// A fresh database and instance folder: an install (job 1) that changed
    /// the database, and its Undo (job 2) cut off by a restart during its
    /// restore, with the history file that restore saved first.
    async fn prepare(&self, client: &Client) -> Result<(), Box<dyn Error>> {
        let _ = std::fs::remove_dir_all(&self.instance);
        for file in ["pg_restore.calls", "service.calls", "held"] {
            let _ = std::fs::remove_file(self.root.join(file));
        }
        std::fs::create_dir_all(self.safety.parent().unwrap())?;
        std::fs::create_dir_all(self.original.parent().unwrap())?;
        std::fs::write(
            self.instance.join("env"),
            "BIND_ADDR=127.0.0.1:9\nAPP_ORIGIN=http://127.0.0.1:9\n",
        )?;
        // The stand-in pg_restore never reads either dump.
        std::fs::write(&self.safety, "safety backup")?;
        std::fs::write(&self.original, "backup before the install")?;
        client
            .batch_execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public")
            .await?;
        for migration in [
            include_str!("../../base/settings/migrations/001-settings.sql"),
            include_str!("../../base/software/migrations/001-jobs.sql"),
            include_str!("../../base/software/migrations/002-settled.sql"),
            include_str!("../../base/software/migrations/003-local-builds.sql"),
            include_str!("../../base/software/migrations/004-github-and-upload.sql"),
            include_str!("../../base/software/migrations/005-paused.sql"),
            include_str!("../../base/software/migrations/006-installer-heartbeat.sql"),
            include_str!("../../base/software/migrations/007-release-trust.sql"),
        ] {
            client.batch_execute(migration).await?;
        }
        let worker = format!("installer@{INSTANCE}");
        client
            .execute(
                "INSERT INTO base_software_jobs
                     (tenant_id, kind, state, request, review, result, requested_by, worker, started_at, finished_at, settled_at)
                 VALUES (1, 'install', 'done', '{}', '{}', $1, 1, $2, now(), now(), now())",
                &[
                    &json!({"release": 2, "previous_release": 1, "migrated": true,
                            "backup": self.original.display().to_string(),
                            "backup_at": "2026-11-26T01:00:00Z"}),
                    &worker,
                ],
            )
            .await?;
        client
            .execute(
                "INSERT INTO base_software_jobs
                     (tenant_id, kind, state, request, review, undo_of, requested_by, worker, started_at, paused_at)
                 VALUES (1, 'undo', 'running', '{\"job\": 1}', '{}', 1, 1, $1, now(), now())",
                &[&worker],
            )
            .await?;
        let backup = json!({"message": "Backed up the database.",
                            "backup": self.safety.display().to_string(),
                            "backup_at": "2026-11-26T02:00:00Z"});
        let restore = json!({"message": "Restoring the backup taken at 2026-11-26T01:00:00Z"});
        client
            .execute(
                "INSERT INTO base_software_steps (tenant_id, job_id, seq, key, label, pauses, state, detail, started_at, finished_at)
                 VALUES (1, $1, 1, 'backup', 'Back up the database', true, 'done', $2, now(), now()),
                        (1, $1, 2, 'restore', 'Restore the backup', false, 'running', $3, now(), NULL),
                        (1, $1, 3, 'switch', 'Switch back', false, 'pending', NULL, NULL, NULL),
                        (1, $1, 4, 'verify', 'Check that it works', false, 'pending', NULL, NULL, NULL)",
                &[&UNDO_JOB, &backup, &restore],
            )
            .await?;
        // What the Undo's restore saved before it began clearing the database.
        let mut history = jobs::history(client, "").await?;
        history["target_composition"] = json!(TARGET);
        std::fs::write(
            self.history(),
            json!({"dump": self.original.display().to_string(), "history": history}).to_string(),
        )?;
        Ok(())
    }
    /// Starts the installer for the instance, as systemd would, with `env`
    /// for the stand-in `pg_restore`.
    fn installer(&self, env: &[(&str, &str)]) -> Result<Child, Box<dyn Error>> {
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("installer")
            .arg(&self.instance)
            .env("DATABASE_URL", &self.url)
            .env("PATH", path)
            .env("WALK", &self.root)
            .env_remove("HOLD")
            .env_remove("NO_NOTE")
            .envs(env.iter().copied())
            .stdout(Stdio::piped());
        Ok(command.spawn()?)
    }
    /// Starts the installer with `env`, waits until it is held running
    /// `waiting` after the failure, kills it there, and ends the held
    /// sessions.
    async fn cut_off(
        &self,
        client: &Client,
        env: &[(&str, &str)],
        waiting: &str,
    ) -> Result<(), Box<dyn Error>> {
        let mut child = self.installer(env)?;
        let held = wait_for_held(client, &mut child, waiting).await;
        let _ = child.kill();
        child.wait()?;
        let blocked = *held.as_ref().unwrap_or(&0);
        client
            .execute(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                 WHERE pid = $1 OR (datname = current_database() AND application_name = 'restore-failure-hold')",
                &[&blocked],
            )
            .await?;
        wait_until("the stopped sessions to end", || async {
            Ok(client
                .query_one(
                    "SELECT count(*) = 0 FROM pg_stat_activity WHERE pid = $1
                     OR (datname = current_database() AND application_name = 'restore-failure-hold')",
                    &[&blocked],
                )
                .await?
                .get::<_, bool>(0))
        })
        .await?;
        held.map(|_| ())
    }
    /// Starts the installer after a cut-off failure: it records the failure
    /// once and never runs `pg_restore` again.
    async fn restart_records_failure(&self) -> Result<(), Box<dyn Error>> {
        let output = self.installer(&[])?.wait_with_output()?;
        let log = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{log}");
        assert_eq!(
            self.restore_calls(),
            1,
            "the restart ran pg_restore again: {log}"
        );
        assert_eq!(log.matches("recording the failure").count(), 1, "{log}");
        assert!(!log.contains("finishing it from"), "{log}");
        Ok(())
    }
    /// The Undo ended `failed` exactly once: its restore step failed, its
    /// message names the backup it tried and the safety backup to restore by
    /// hand, and the history file and failure note are gone. Returns when
    /// the job finished.
    async fn assert_failed_once(
        &self,
        client: &Client,
        reason: &str,
    ) -> Result<String, Box<dyn Error>> {
        let row = client
            .query_one(
                "SELECT state, result, finished_at::text,
                        (SELECT state FROM base_software_steps WHERE job_id = j.id AND key = 'restore'),
                        (SELECT count(*) FROM base_software_jobs WHERE state IN ('queued', 'running'))
                 FROM base_software_jobs j WHERE id = $1",
                &[&UNDO_JOB],
            )
            .await?;
        let (state, result, finished, step, active): (String, Value, String, String, i64) =
            (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4));
        let message = result["message"].as_str().unwrap_or_default();
        assert_eq!(
            (state.as_str(), step.as_str(), active),
            ("failed", "failed", 0)
        );
        assert!(
            message.contains(&format!(
                "Restore {} by hand before starting the service.",
                self.safety.display()
            )) && message.contains(&format!(
                "The restore step failed for {}",
                self.original.display()
            )) && message.contains(reason)
                && message.contains("The service remains stopped."),
            "{message}"
        );
        assert_eq!(result["restored"], json!(false));
        assert!(!self.history().exists() && !self.note().exists() && !self.held().exists());
        Ok(finished)
    }
}

/// Waits until the installer is held by a lock running a statement like
/// `waiting`; returns that session's process ID.
async fn wait_for_held(
    client: &Client,
    child: &mut Child,
    waiting: &str,
) -> Result<i32, Box<dyn Error>> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(format!("the installer ended before it was held: {status}").into());
        }
        let held = client
            .query_opt(
                "SELECT pid FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'
                   AND query LIKE $1",
                &[&waiting],
            )
            .await?;
        if let Some(row) = held {
            return Ok(row.get(0));
        }
        if started.elapsed() > Duration::from_secs(60) {
            return Err(format!(
                "the installer was not seen waiting on {waiting} within 60 seconds"
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_until<F, Fut>(what: &str, mut check: F) -> Result<(), Box<dyn Error>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool, Box<dyn Error>>>,
{
    let started = Instant::now();
    while !check().await? {
        if started.elapsed() > Duration::from_secs(30) {
            return Err(format!("timed out waiting for {what}").into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}
