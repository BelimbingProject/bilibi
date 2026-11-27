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

/// Leave enough time for a queue commit and its systemd activation marker to
/// become visible, then stop consuming resources when no job is ready.
const IDLE_AFTER: Duration = Duration::from_secs(10);
/// After a lost database connection or a failed scan, before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// How often a job being worked on is marked alive (`heartbeat_at`), well
/// inside the shortest silence the app accepts.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);

enum Scan {
    Worked,
    Empty,
    Deferred,
}

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
    /// Settles work cut off by a restart, carries every ready job in order,
    /// then exits after a short idle window. systemd starts it at boot and
    /// whenever the application writes a ready marker.
    pub async fn run(&self) -> Result<(), String> {
        println!(
            "bilibi-installer: checking {} ({}) as {}",
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
            let mut idle_markers = self.layout.activation_markers();
            loop {
                match self.scan(&mut client).await {
                    Ok(Scan::Worked) => {
                        reported.clear();
                        idle_markers = self.layout.activation_markers();
                        continue;
                    }
                    Ok(Scan::Empty | Scan::Deferred) => reported.clear(),
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
                        idle_markers = self.layout.activation_markers();
                    }
                    _ = tokio::time::sleep(IDLE_AFTER) => {
                        match self.scan(&mut client).await {
                            Ok(Scan::Worked) => {
                                idle_markers = self.layout.activation_markers();
                            }
                            Ok(Scan::Deferred) => {}
                            Ok(Scan::Empty) => {
                                self.layout.remove_activation_markers(&idle_markers);
                                println!("bilibi-installer: no work ready; exiting");
                                return Ok(());
                            }
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
                    }
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
    /// when last stopped, then the oldest waiting.
    async fn scan(&self, client: &mut Client) -> Result<Scan, tokio_postgres::Error> {
        let lock = match std::fs::File::open(&self.layout.dir) {
            Ok(lock) => lock,
            Err(error) => {
                eprintln!("bilibi-installer: could not open the instance folder: {error}");
                return Ok(Scan::Deferred);
            }
        };
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(Scan::Deferred),
            Err(error) => {
                eprintln!("bilibi-installer: could not lock the instance: {error}");
                return Ok(Scan::Deferred);
            }
        }
        let result = self.scan_job(client).await;
        client
            .batch_execute("SELECT pg_advisory_unlock_all()")
            .await?;
        result.map(|worked| if worked { Scan::Worked } else { Scan::Empty })
    }
    async fn scan_job(&self, client: &mut Client) -> Result<bool, tokio_postgres::Error> {
        if self.finish_cut_off_restores(client).await? {
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
        if resumed && run.job().kind != "trust" && run.resume().await? {
            return Ok(true);
        }
        match run.job().kind.as_str() {
            "install" => run.install().await?,
            "undo" => run.undo().await?,
            "trust" => run.trust().await?,
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
    async fn finish_cut_off_restores(
        &self,
        client: &mut Client,
    ) -> Result<bool, tokio_postgres::Error> {
        let mut finished = false;
        for (id, history, dump) in backup::unfinished_restores(&self.layout.backups()) {
            let key = match history.file_name().and_then(|name| name.to_str()) {
                Some("history-restore.json") => "restore",
                Some("history-switch_back.json") => "switch_back",
                _ => continue,
            };
            let recorded = jobs::restore_job(client, id).await?;
            if recorded
                .as_ref()
                .is_some_and(|(_, state)| !matches!(state.as_str(), "queued" | "running"))
            {
                let _ = std::fs::remove_file(&history);
                continue;
            }
            jobs::lock_installation(client).await?;
            if let Err(why) = self.service.stop() {
                eprintln!("bilibi-installer: job {id}: {why}");
                return Ok(true);
            }
            if let Some((job, _)) = recorded {
                match flow::Run::new(client, &self.layout, &self.service, &self.pg, job, true).await
                {
                    Ok(mut run) => {
                        if run.settle_failed_restore(key).await? {
                            finished = true;
                            continue;
                        }
                    }
                    Err(error) if jobs::table_missing(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            println!(
                "job {id}: a restore was cut off by a restart; finishing it from {}",
                dump.display()
            );
            if let Err(why) = backup::restore(client, &self.pg, &dump, "", &history, None).await {
                if let Some((job, _)) = jobs::restore_job(client, id).await? {
                    let mut run =
                        flow::Run::new(client, &self.layout, &self.service, &self.pg, job, true)
                            .await?;
                    run.fail_restore(key, &dump, &why).await?;
                } else {
                    eprintln!(
                        "bilibi-installer: job {id}: the cut-off restore from {} failed: {why}; the service remains stopped",
                        dump.display()
                    );
                    backup::set_aside(&history);
                }
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
