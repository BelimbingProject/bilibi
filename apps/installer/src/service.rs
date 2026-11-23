//! The instance's systemd service and its health: stop and start through the
//! sudo rule `scripts/install-server.sh` gives `bilibi-build`, and the check
//! that the program answers on its own port.
use crate::layout::Layout;
use std::{process::Command, time::Duration};
use tokio::{
    io::AsyncWriteExt,
    time::{sleep, timeout, Instant},
};

pub struct Service {
    unit: String,
    layout: Layout,
}
impl Service {
    pub fn of(layout: &Layout) -> Result<Self, String> {
        if Listener::configured(layout, "BIND_ADDR", "APP_ORIGIN")?.is_none() {
            return Err(format!(
                "{} has no staff listener in its env file",
                layout.dir.display()
            ));
        }
        Listener::configured(layout, "PUBLIC_BIND_ADDR", "PUBLIC_ORIGIN")?;
        Ok(Self {
            unit: layout.unit(),
            layout: layout.clone(),
        })
    }
    /// Stops the program. From here the proxy shows the waiting page.
    pub fn stop(&self) -> Result<(), String> {
        self.control("stop")
    }
    pub fn start(&self) -> Result<(), String> {
        self.control("start")
    }
    fn control(&self, verb: &str) -> Result<(), String> {
        let out = Command::new("sudo")
            .args(["-n", "systemctl", verb, &self.unit])
            .output()
            .map_err(|error| format!("could not run systemctl: {error}"))?;
        if out.status.success() {
            return Ok(());
        }
        Err(format!(
            "systemctl {verb} {} failed: {}",
            self.unit,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
    /// `active`, `inactive`, `failed`, `activating`, or `unknown`.
    pub fn state(&self) -> String {
        Command::new("systemctl")
            .args(["is-active", &self.unit])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .ok()
            .filter(|state| !state.is_empty())
            .unwrap_or_else(|| "unknown".into())
    }
    /// Checks readiness on every configured listener.
    pub async fn answers(&self) -> bool {
        let Ok(Some(staff)) = Listener::configured(&self.layout, "BIND_ADDR", "APP_ORIGIN") else {
            return false;
        };
        if !staff.answers("/login").await {
            return false;
        }
        match Listener::configured(&self.layout, "PUBLIC_BIND_ADDR", "PUBLIC_ORIGIN") {
            Ok(Some(public)) => public.answers("/portal/sign-in").await,
            Ok(None) => true,
            Err(_) => false,
        }
    }
    /// How many times systemd has restarted the service since it was last
    /// started by hand: a program that exits right after starting shows here
    /// while `is-active` only says `activating`.
    fn restarts(&self) -> u32 {
        Command::new("systemctl")
            .args(["show", "-p", "NRestarts", "--value", &self.unit])
            .output()
            .ok()
            .and_then(|out| String::from_utf8_lossy(&out.stdout).trim().parse().ok())
            .unwrap_or(0)
    }
    /// Waits up to `limit` for the program to answer, giving up early when
    /// systemd reports the service failed or has had to restart it twice.
    /// Returns how long it took.
    pub async fn wait_healthy(&self, limit: Duration) -> Result<Duration, String> {
        let started = Instant::now();
        while started.elapsed() < limit {
            if self.answers().await {
                return Ok(started.elapsed());
            }
            if self.state() == "failed" {
                return Err(format!(
                    "the service {} failed to start (journalctl -u {} shows why)",
                    self.unit, self.unit
                ));
            }
            let restarts = self.restarts();
            if restarts >= 2 {
                return Err(format!(
                    "the program exited right after starting, {restarts} times in a row (journalctl -u {} shows why)",
                    self.unit
                ));
            }
            sleep(Duration::from_secs(1)).await;
        }
        Err(format!(
            "not all listeners became ready within {} seconds (service {})",
            limit.as_secs(),
            self.state()
        ))
    }
}

struct Listener {
    bind: String,
    authority: String,
}
impl Listener {
    fn configured(
        layout: &Layout,
        bind_key: &str,
        origin_key: &str,
    ) -> Result<Option<Self>, String> {
        match (layout.env_value(bind_key), layout.env_value(origin_key)) {
            (Some(bind), Some(origin)) => {
                let authority = origin
                    .split_once("://")
                    .map(|(_, rest)| rest.split('/').next().unwrap_or_default())
                    .filter(|host| {
                        !host.is_empty() && !host.bytes().any(|byte| byte.is_ascii_whitespace())
                    })
                    .ok_or_else(|| format!("Invalid {origin_key} in {}", layout.dir.display()))?;
                Ok(Some(Self {
                    bind,
                    authority: authority.to_owned(),
                }))
            }
            (None, None) => Ok(None),
            _ => Err(format!(
                "Configure {bind_key} and {origin_key} together in {}",
                layout.dir.display()
            )),
        }
    }

    async fn answers(&self, path: &str) -> bool {
        let request = async {
            let mut stream = tokio::net::TcpStream::connect(&self.bind).await.ok()?;
            let request = format!(
                "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                self.authority
            );
            stream.write_all(request.as_bytes()).await.ok()?;
            let mut line = String::new();
            tokio::io::AsyncBufReadExt::read_line(
                &mut tokio::io::BufReader::new(stream),
                &mut line,
            )
            .await
            .ok()?;
            (line.split_whitespace().nth(1)? == "200").then_some(())
        };
        timeout(Duration::from_secs(2), request)
            .await
            .ok()
            .flatten()
            .is_some()
    }
}
