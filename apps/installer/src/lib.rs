//! The installer: the service beside every Bilibi instance that puts the
//! releases a server owner asks for into service, and takes them back.
//!
//! It is the worker side of `base/software` (`apps/base/software/README.md`,
//! "How a worker plugs in"): the app records a reviewed, 2FA-checked request
//! as a job with pending steps; this program listens for it, claims it and
//! reports every step while it works. It runs as `bilibi-build`, outside the
//! program it installs, because it has to stop and start that program.
//! `README.md` beside this file says what each step does and why.
mod backup;
mod composition;
mod flow;
mod jobs;
mod layout;
mod service;

use backup::PgEnv;
use layout::Layout;
use service::Service;
use std::{path::Path, time::Duration};
use tokio::sync::mpsc;
use tokio_postgres::{AsyncMessage, Client, NoTls};

/// The installer scans for jobs at least this often, in case a wake-up was
/// sent while it was down or the connection was lost.
const SCAN_EVERY: Duration = Duration::from_secs(30);
/// After a lost database connection or a failed scan, before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// How often a job being worked on is marked alive (`heartbeat_at`), well
/// inside the shortest silence the app accepts.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);

pub struct Installer {
    layout: Layout,
    service: Service,
    pg: PgEnv,
    database_url: String,
    /// `installer@<instance>`: the name the job rows show as the worker.
    worker: String,
}
impl Installer {
    /// For the instance whose folder this is, with its env file's
    /// `DATABASE_URL` and `BIND_ADDR` in the environment.
    pub fn open(dir: &Path) -> Result<Self, String> {
        let layout = Layout::open(dir)?;
        let service = Service::of(&layout)?;
        let database_url = std::env::var("DATABASE_URL")
            .map_err(|_| "DATABASE_URL is not set; load the instance's env file".to_owned())?;
        let pg = PgEnv::from_url(&database_url)?;
        Ok(Self {
            worker: format!("installer@{}", layout.name),
            layout,
            service,
            pg,
            database_url,
        })
    }
    /// Serves the instance until stopped: settles a job cut off by a restart,
    /// then claims each install or undo as it is queued.
    pub async fn run(&self) -> Result<(), String> {
        println!(
            "bilibi-installer: serving {} ({}) as {}",
            self.layout.name,
            self.layout.dir.display(),
            self.worker
        );
        let mut reported = String::new();
        loop {
            let (mut client, _listener, mut wake) = match self.connect().await {
                Ok(connected) => connected,
                Err(error) => {
                    eprintln!(
                        "bilibi-installer: could not connect to the database: {}; retrying",
                        jobs::short(&error)
                    );
                    tokio::time::sleep(RETRY_AFTER).await;
                    continue;
                }
            };
            loop {
                match self.scan(&mut client).await {
                    Ok(true) => {
                        reported.clear();
                        continue;
                    }
                    Ok(false) => reported.clear(),
                    Err(error) => {
                        let text = jobs::short(&error);
                        if text != reported {
                            eprintln!("bilibi-installer: database error: {text}; reconnecting");
                            reported = text;
                        }
                        tokio::time::sleep(RETRY_AFTER).await;
                        break;
                    }
                }
                tokio::select! {
                    woken = wake.recv() => {
                        if woken.is_none() {
                            eprintln!("bilibi-installer: the listening connection closed; reconnecting");
                            break;
                        }
                    }
                    _ = tokio::time::sleep(SCAN_EVERY) => {}
                }
            }
        }
    }
    /// Two connections: one that listens for wake-ups, one that works. The
    /// listening client must be kept for the connection to deliver; dropping
    /// it ends that connection.
    async fn connect(
        &self,
    ) -> Result<(Client, Client, mpsc::UnboundedReceiver<String>), tokio_postgres::Error> {
        let (listener, mut connection) = tokio_postgres::connect(&self.database_url, NoTls).await?;
        let (send, receive) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                match std::future::poll_fn(|cx| connection.poll_message(cx)).await {
                    Some(Ok(AsyncMessage::Notification(notice))) => {
                        if send.send(notice.payload().to_owned()).is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        });
        listener
            .batch_execute(&format!("LISTEN {}", jobs::CHANNEL))
            .await?;
        let (client, connection) = tokio_postgres::connect(&self.database_url, NoTls).await?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("bilibi-installer: connection closed: {error}");
            }
        });
        Ok((client, listener, receive))
    }
    /// One job, if there is one for this worker: first the one it was running
    /// when last stopped, then the oldest waiting. True when it did some work.
    async fn scan(&self, client: &mut Client) -> Result<bool, tokio_postgres::Error> {
        if self.finish_cut_off_restores(&*client).await? {
            return Ok(true);
        }
        let running = jobs::mine_running(&*client, &self.worker).await?;
        let (job, resumed) = match running {
            Some(job) => {
                println!(
                    "job {}: the installer restarted while it was running this {}",
                    job.id, job.kind
                );
                (job, true)
            }
            None => match jobs::next_waiting(&*client).await? {
                Some(job) => {
                    if !jobs::claim(&*client, &job, &self.worker).await? {
                        return Ok(true);
                    }
                    println!("job {}: claimed ({})", job.id, job.kind);
                    (job, false)
                }
                None => return Ok(false),
            },
        };
        // Beside the work, on a connection of its own: a step can run for
        // minutes, and the app ends a running job whose installer has been
        // silent too long. Dropped, it stops with the job.
        let _alive = Heartbeat(self.heartbeat(job.tenant, job.id));
        let mut run =
            flow::Run::new(client, &self.layout, &self.service, &self.pg, job, resumed).await?;
        if resumed && run.resume().await? {
            return Ok(true);
        }
        match run.job().kind.as_str() {
            "install" => run.install().await?,
            "undo" => run.undo().await?,
            other => {
                eprintln!(
                    "job {}: an installer cannot carry out a {other}; leaving it",
                    run.job().id
                );
                return Ok(false);
            }
        }
        Ok(true)
    }
    /// Marks the job alive every [`HEARTBEAT_EVERY`] until stopped. A write
    /// that fails (the database is restarting, or a restore has the job
    /// tables empty) is tried again with a fresh connection next time.
    fn heartbeat(&self, tenant: i64, id: i64) -> tokio::task::JoinHandle<()> {
        let url = self.database_url.clone();
        tokio::spawn(async move {
            loop {
                if let Ok((client, connection)) = tokio_postgres::connect(&url, NoTls).await {
                    let connection = tokio::spawn(async move {
                        let _ = connection.await;
                    });
                    while client
                        .execute(
                            "UPDATE base_software_jobs SET heartbeat_at=now() WHERE tenant_id=$1 AND id=$2 AND state='running'",
                            &[&tenant, &id],
                        )
                        .await
                        .is_ok()
                    {
                        tokio::time::sleep(HEARTBEAT_EVERY).await;
                    }
                    connection.abort();
                }
                tokio::time::sleep(HEARTBEAT_EVERY).await;
            }
        })
    }
    /// A restore cut off by a restart may have left the job tables empty,
    /// holding only the backup's rows, or gone, so the job cannot be found
    /// by looking for it. The history saved beside its backup says a restore
    /// was begun and not finished: for each such file whose job is not
    /// recorded as finished, the service is stopped and the restore is
    /// finished from the dump. The job is then found again and settled like
    /// any other. A file whose job is recorded as finished is stale and is
    /// removed. True when a restore was finished.
    async fn finish_cut_off_restores(
        &self,
        client: &Client,
    ) -> Result<bool, tokio_postgres::Error> {
        let mut finished = false;
        for (id, history, dump) in backup::unfinished_restores(&self.layout.backups()) {
            if jobs::is_finished(client, id).await? {
                let _ = std::fs::remove_file(&history);
                continue;
            }
            println!(
                "job {id}: a restore was cut off by a restart; finishing it from {}",
                dump.display()
            );
            let _ = self.service.stop();
            if let Err(why) = backup::restore(client, &self.pg, &dump, "", &history).await {
                eprintln!("bilibi-installer: job {id}: the cut-off restore failed: {why}");
                backup::set_aside(&history);
            }
            finished = true;
        }
        Ok(finished)
    }
}

/// Stops a job's heartbeat when the job's work returns, however it returns.
struct Heartbeat(tokio::task::JoinHandle<()>);
impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.0.abort();
    }
}
