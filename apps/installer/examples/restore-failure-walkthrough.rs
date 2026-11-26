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

/// `pg_restore` that records each call and fails. With `HOLD` set it first
/// opens a transaction that creates a table in `public` and stays open, so
/// the installer's cleanup waits to drop that schema: the window in which
/// the failure is noted and not yet recorded.
const PG_RESTORE: &str = r#"#!/bin/sh
echo "pg_restore $*" >> "$WALK/pg_restore.calls"
if [ -n "$HOLD" ]; then
  PGAPPNAME=restore-failure-hold psql --no-psqlrc --quiet >/dev/null 2>&1 <<SQL &
BEGIN;
CREATE TABLE public.restore_failure_hold ();
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
    let output = walk.installer(false)?.wait_with_output()?;
    let log = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{log}");
    assert_eq!(walk.restore_calls(), 1, "{log}");
    walk.assert_failed_once(&client).await?;
    println!("PASS: a failed restore is recorded once, names the safety backup to restore by hand, and leaves no failure note");

    // 2. The installer exits during the cleanup after the failure.
    walk.prepare(&client).await?;
    let mut child = walk.installer(true)?;
    let blocked = walk.wait_for_cleanup(&client, &mut child).await;
    let _ = child.kill();
    child.wait()?;
    let blocked = match blocked {
        Ok(pid) => pid,
        Err(why) => {
            client
                .execute(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                     WHERE datname = current_database() AND application_name = 'restore-failure-hold'",
                    &[],
                )
                .await?;
            return Err(why);
        }
    };
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
    assert_eq!(walk.restore_calls(), 1);
    assert!(walk.history().is_file());
    let noted = walk.note().is_file();
    assert!(
        client
            .query_one("SELECT to_regclass('base_software_jobs') IS NULL", &[])
            .await?
            .get::<_, bool>(0),
        "the installer was stopped before the job tables were made again"
    );
    println!("PASS: the installer exited during the cleanup after the failure, with the job tables gone and the restore step not recorded");

    let output = walk.installer(false)?.wait_with_output()?;
    let log = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{log}");
    assert_eq!(
        walk.restore_calls(),
        1,
        "the restart ran pg_restore again: {log}"
    );
    assert!(noted, "the failure was not noted before the cleanup");
    assert_eq!(log.matches("recording the failure").count(), 1, "{log}");
    assert!(!log.contains("finishing it from"), "{log}");
    let finished = walk.assert_failed_once(&client).await?;
    println!("PASS: the restart recorded the failure once with the manual recovery dump and ran no restore");

    let output = walk.installer(false)?.wait_with_output()?;
    let log = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{log}");
    assert_eq!(walk.restore_calls(), 1, "{log}");
    assert_eq!(walk.assert_failed_once(&client).await?, finished);
    println!("PASS: a later start finds nothing to do and leaves the recorded outcome as it is");
    Ok(())
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
    /// Starts the installer for the instance, as systemd would.
    fn installer(&self, hold: bool) -> Result<Child, Box<dyn Error>> {
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
            .stdout(Stdio::piped());
        if hold {
            command.env("HOLD", "1");
        }
        Ok(command.spawn()?)
    }
    /// Waits until the cleanup after the failure is held dropping `public`
    /// (the clear before `pg_restore` ran before the hold existed); returns
    /// that session's process ID.
    async fn wait_for_cleanup(
        &self,
        client: &Client,
        child: &mut Child,
    ) -> Result<i32, Box<dyn Error>> {
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                return Err(format!("the installer ended before its cleanup: {status}").into());
            }
            let waiting = client
                .query_opt(
                    "SELECT pid FROM pg_stat_activity
                     WHERE datname = current_database() AND wait_event_type = 'Lock'
                       AND query LIKE 'DROP SCHEMA%'",
                    &[],
                )
                .await?;
            if let Some(row) = waiting {
                return Ok(row.get(0));
            }
            if started.elapsed() > Duration::from_secs(60) {
                return Err("the installer's cleanup was not seen within 60 seconds".into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    /// The Undo ended `failed` exactly once: its restore step failed, its
    /// message names the backup it tried and the safety backup to restore by
    /// hand, and the history file and failure note are gone. Returns when
    /// the job finished.
    async fn assert_failed_once(&self, client: &Client) -> Result<String, Box<dyn Error>> {
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
            )) && message.contains("pg_restore exited with")
                && message.contains("The service remains stopped."),
            "{message}"
        );
        assert_eq!(result["restored"], json!(false));
        assert!(!self.history().exists() && !self.note().exists());
        Ok(finished)
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
