//! Signing in with the OAuth device grant, keeping the access token fresh,
//! and the one authenticated call every command makes.
use crate::{
    now, option,
    refusal::{failed, settled},
    store::{Pending, Saved},
    toon::Doc,
    wire::{self, Answer},
    Cli, Done, Problem, Reply, DEFAULT_WAIT, DEVICE_GRANT, REFRESH_MARGIN,
};
use serde_json::{json, Value};
use std::time::Duration;

impl Cli {
    // ---- sign-in ----------------------------------------------------------------

    pub(crate) fn name(&self) -> Option<String> {
        self.server.clone().or_else(|| self.store.current())
    }

    pub(crate) fn not_signed_in() -> Problem {
        Problem::new("NOT_SIGNED_IN", "This machine is not signed in to a Bilibi")
            .help("Run `bilibi-axi login <address>` with the Bilibi's address, such as https://bilibi.example.com")
    }

    /// Rotates the refresh token. The caller holds the store's lock, so the
    /// single-use token is spent once.
    pub(crate) async fn refresh(&self, saved: &mut Saved) -> Result<(), Problem> {
        let Some(refresh_token) = saved.refresh_token.clone() else {
            return Err(Self::not_signed_in());
        };
        let body = json!({
            "grant_type": "refresh_token",
            "client_id": saved.client_id,
            "refresh_token": refresh_token,
        });
        let answer = self
            .wire
            .post(&saved.origin, "/oauth/token", None, &body)
            .await
            .map_err(|wire::Unreachable(message)| Problem::new("NO_CONNECTION", message))?;
        if answer.status == 200 && self.accept(saved, &answer.body) {
            return Ok(self.store.save(saved)?);
        }
        if answer.body.get("error").and_then(Value::as_str) == Some("invalid_grant") {
            // Ended in Bilibi (revoked, or the account changed): forget it.
            saved.access_token = None;
            saved.refresh_token = None;
            self.store.save(saved)?;
            return Err(
                Problem::new("SIGN_IN_NEEDED", "This sign-in has ended in Bilibi").help(format!(
                    "Run `bilibi-axi login {}` to sign in again",
                    saved.origin
                )),
            );
        }
        Err(failed(&saved.origin, &answer))
    }

    /// Takes the tokens of a token response; false when it is not one.
    pub(crate) fn accept(&self, saved: &mut Saved, body: &Value) -> bool {
        let text = |name: &str| body.get(name).and_then(Value::as_str).map(str::to_owned);
        let (Some(access), Some(refresh), Some(seconds)) = (
            text("access_token"),
            text("refresh_token"),
            body.get("expires_in").and_then(Value::as_u64),
        ) else {
            return false;
        };
        saved.access_token = Some(access);
        saved.refresh_token = Some(refresh);
        saved.access_expires_at = now() + seconds;
        saved.pending = None;
        true
    }

    /// The saved sign-in with an access token good for another minute.
    pub(crate) async fn signed_in(&self) -> Result<Saved, Problem> {
        let name = self.name().ok_or_else(Self::not_signed_in)?;
        let _lock = self.store.lock()?;
        let mut saved = self.store.load(&name)?.ok_or_else(Self::not_signed_in)?;
        if saved.refresh_token.is_none() {
            return Err(match &saved.pending {
                Some(pending) if pending.expires_at > now() => Problem::new(
                    "AUTHORIZATION_PENDING",
                    "Sign-in is waiting for the person's approval",
                )
                .with("code", json!(pending.user_code))
                .with("open", json!(pending.link))
                .help("Run `bilibi-axi login --wait` once the person approved"),
                _ => Self::not_signed_in(),
            });
        }
        if saved.access_token.is_none() || saved.access_expires_at <= now() + REFRESH_MARGIN {
            self.refresh(&mut saved).await?;
        }
        Ok(saved)
    }

    /// One API call as the signed-in connection. A refused token is refreshed
    /// once, unless another command already did.
    pub(crate) async fn api(&self, path: &str, body: Option<&Value>) -> Result<Value, Problem> {
        let mut saved = self.signed_in().await?;
        let mut answer = self.call(&saved, path, body).await?;
        if answer.status == 401 {
            {
                let _lock = self.store.lock()?;
                let latest = self
                    .store
                    .load(&saved.server_name)?
                    .ok_or_else(Self::not_signed_in)?;
                if latest.access_token == saved.access_token {
                    saved = latest;
                    self.refresh(&mut saved).await?;
                } else {
                    saved = latest;
                }
            }
            answer = self.call(&saved, path, body).await?;
        }
        settled(&saved.origin, answer).map_err(|mut problem| {
            if problem.code == "SIGN_IN_NEEDED" {
                problem.help = vec![format!(
                    "Run `bilibi-axi login {}` to sign in again",
                    saved.origin
                )];
            }
            problem
        })
    }

    pub(crate) async fn call(
        &self,
        saved: &Saved,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Answer, Problem> {
        let bearer = saved.access_token.as_deref();
        let path = format!("/api/v1{path}");
        match body {
            Some(body) => self.wire.post(&saved.origin, &path, bearer, body).await,
            None => self.wire.get(&saved.origin, &path, bearer).await,
        }
        .map_err(|wire::Unreachable(message)| {
            Problem::new("NO_CONNECTION", message)
                .help("No answer came back, so this is the network or the address, not the sign-in")
        })
    }

    pub(crate) async fn login(&mut self, mut args: Vec<String>) -> Done {
        let wait = match args.iter().position(|arg| arg == "--wait") {
            None => None,
            Some(at) => {
                args.remove(at);
                // An optional number of seconds follows.
                match args.get(at).map(|next| next.parse::<u64>()) {
                    Some(Ok(seconds)) => {
                        args.remove(at);
                        Some(seconds)
                    }
                    _ => Some(DEFAULT_WAIT),
                }
            }
        };
        let name = option(&mut args, "--name")?;
        match (args.as_slice(), wait) {
            ([address], wait) => {
                let started = self.start(address, name.as_deref()).await?;
                match wait {
                    Some(seconds) => self.finish(seconds).await,
                    None => Ok(started),
                }
            }
            ([], Some(seconds)) => self.finish(seconds).await,
            _ => Err(Problem::usage(
                "give the Bilibi's address to start, or --wait to finish: `bilibi-axi login <address>`",
            )),
        }
    }

    /// Starts the device grant and saves the code it must poll with.
    pub(crate) async fn start(&mut self, address: &str, name: Option<&str>) -> Done {
        let origin = wire::origin(address).map_err(Problem::usage)?;
        let unreachable = |wire::Unreachable(message)| {
            Problem::new("NO_CONNECTION", message)
                .help("No answer came back, so this is the network or the address")
        };
        let found = self
            .wire
            .get(&origin, "/.well-known/bilibi", None)
            .await
            .map_err(unreachable)?;
        let field = |name: &str| found.body.get(name).and_then(Value::as_str);
        let (Some(server_name), Some(client_id)) = (field("server_name"), field("client_id"))
        else {
            return Err(Problem::new(
                "NOT_BILIBI",
                format!("{origin} did not answer as a Bilibi"),
            ));
        };
        // A Bilibi names its own canonical address; tokens are only ever
        // sent to the address typed here, so the two must be the same.
        if field("api_base") != Some(&format!("{origin}/api/v1")) {
            return Err(Problem::new(
                "ADDRESS_MISMATCH",
                format!("This Bilibi says its address is not {origin}"),
            )
            .with(
                "api_base",
                found.body.get("api_base").cloned().unwrap_or_default(),
            )
            .help("Sign in with the address the Bilibi itself states"));
        }
        let body = json!({"client_id": client_id, "agent_id": name.unwrap_or("bilibi-axi")});
        let answer = self
            .wire
            .post(&origin, "/oauth/device_authorization", None, &body)
            .await
            .map_err(unreachable)?;
        if answer.status != 200 {
            return Err(failed(&origin, &answer));
        }
        let text = |name: &str| {
            answer
                .body
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let number = |name: &str| answer.body.get(name).and_then(Value::as_u64);
        let (Some(device_code), Some(user_code), Some(link), Some(seconds)) = (
            text("device_code"),
            text("user_code"),
            text("verification_uri_complete"),
            number("expires_in"),
        ) else {
            return Err(Problem::new(
                "NOT_BILIBI",
                "The sign-in answer was not understood",
            ));
        };
        let pending = Pending {
            device_code,
            user_code: user_code.clone(),
            link: link.clone(),
            expires_at: now() + seconds,
            interval: number("interval").unwrap_or(5),
        };
        let _lock = self.store.lock()?;
        // An earlier sign-in to the same Bilibi stays in use until this one is approved.
        let mut saved = self
            .store
            .load(server_name)?
            .filter(|saved| saved.origin == origin)
            .unwrap_or_else(|| Saved {
                origin: origin.clone(),
                server_name: server_name.to_owned(),
                client_id: client_id.to_owned(),
                access_token: None,
                access_expires_at: 0,
                refresh_token: None,
                pending: None,
            });
        saved.client_id = client_id.to_owned();
        saved.pending = Some(pending);
        self.store.save(&saved)?;
        self.server = Some(saved.server_name.clone());
        let page = text("verification_uri").unwrap_or_default();
        Ok(Reply {
            doc: Doc::default()
                .put("login", "waiting for approval")
                .put("server", saved.server_name.clone())
                .put("code", user_code.clone())
                .put("open", link.clone())
                .put("expires_in", seconds)
                .help(&[
                    format!("Ask the person to open the link, check that the page shows code {user_code}, and approve"),
                    "Run `bilibi-axi login --wait` to finish signing in".to_owned(),
                ]),
            raw: json!({"login": "waiting for approval", "server": saved.server_name, "code": user_code, "open": link, "page": page, "expires_in": seconds}),
        })
    }

    /// Polls the token endpoint until the person decided, the code ran out
    /// or `seconds` passed.
    pub(crate) async fn finish(&mut self, seconds: u64) -> Done {
        let name = self.name().ok_or_else(Self::not_signed_in)?;
        let saved = self.store.load(&name)?.ok_or_else(Self::not_signed_in)?;
        let Some(pending) = saved.pending.clone() else {
            return if saved.refresh_token.is_some() {
                self.server = Some(name);
                self.signed_in_reply().await
            } else {
                Err(Self::not_signed_in())
            };
        };
        let deadline = (now() + seconds).min(pending.expires_at);
        let mut interval = pending.interval.max(1);
        let body = json!({
            "grant_type": DEVICE_GRANT,
            "client_id": saved.client_id,
            "device_code": pending.device_code,
        });
        loop {
            let answer = self
                .wire
                .post(&saved.origin, "/oauth/token", None, &body)
                .await
                .map_err(|wire::Unreachable(message)| Problem::new("NO_CONNECTION", message))?;
            let error = answer
                .body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match (answer.status, error) {
                (200, _) => {
                    let _lock = self.store.lock()?;
                    let mut latest = self.store.load(&name)?.unwrap_or(saved);
                    if !self.accept(&mut latest, &answer.body) {
                        return Err(Problem::new(
                            "NOT_BILIBI",
                            "The sign-in answer was not understood",
                        ));
                    }
                    self.store.save(&latest)?;
                    drop(_lock);
                    self.server = Some(name);
                    return self.signed_in_reply().await;
                }
                (_, "authorization_pending") => {}
                (_, "slow_down") => interval += 5,
                (_, "access_denied" | "expired_token" | "invalid_grant") => {
                    let _lock = self.store.lock()?;
                    if let Some(mut latest) = self.store.load(&name)? {
                        // Another `login --wait` may have signed in with this code.
                        if error == "invalid_grant"
                            && latest.pending.is_none()
                            && latest.refresh_token.is_some()
                        {
                            drop(_lock);
                            self.server = Some(name);
                            return self.signed_in_reply().await;
                        }
                        latest.pending = None;
                        self.store.save(&latest)?;
                    }
                    return Err(match error {
                        "access_denied" => {
                            Problem::new("ACCESS_DENIED", "The person refused the connection")
                        }
                        _ => Problem::new("EXPIRED", "The code ran out before it was approved"),
                    }
                    .help(format!(
                        "Run `bilibi-axi login {}` for a new code",
                        saved.origin
                    )));
                }
                _ => return Err(failed(&saved.origin, &answer)),
            }
            if now() + interval > deadline {
                let left = pending.expires_at.saturating_sub(now());
                if left == 0 {
                    continue;
                }
                return Ok(Reply {
                    doc: Doc::default()
                        .put("login", "waiting for approval")
                        .put("code", pending.user_code.clone())
                        .put("open", pending.link.clone())
                        .put("expires_in", left)
                        .help(&[
                            "Run `bilibi-axi login --wait` again once the person approved"
                                .to_owned(),
                        ]),
                    raw: json!({"login": "waiting for approval", "code": pending.user_code, "open": pending.link, "expires_in": left}),
                });
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    }

    pub(crate) async fn signed_in_reply(&self) -> Done {
        let hello = self.api("/hello", None).await?;
        Ok(Reply {
            doc: Doc::default()
                .put("login", "signed in")
                .put_some("server", hello.get("server_name").cloned())
                .put_some("instance", hello.pointer("/instance/notice").cloned())
                .help(&[
                    "Run `bilibi-axi search <words>` to find operations, guides and pages"
                        .to_owned(),
                ]),
            raw: json!({"login": "signed in", "hello": hello}),
        })
    }

    pub(crate) fn logout(&self, args: &[String]) -> Done {
        if !args.is_empty() {
            return Err(Problem::usage("logout takes no argument"));
        }
        let name = self.name().ok_or_else(Self::not_signed_in)?;
        let _lock = self.store.lock()?;
        let saved = self.store.load(&name)?;
        let forgotten = self.store.forget(&name)?;
        let mut help = Vec::new();
        if let Some(saved) = saved.filter(|saved| saved.refresh_token.is_some()) {
            help.push(format!(
                "The connection still exists in Bilibi until the person revokes it at {}/settings/ai-connections",
                saved.origin
            ));
        }
        Ok(Reply {
            doc: Doc::default()
                .put(
                    "logout",
                    if forgotten {
                        "forgotten"
                    } else {
                        "nothing saved"
                    },
                )
                .put("server", name.clone())
                .help(&help),
            raw: json!({"logout": forgotten, "server": name}),
        })
    }
}
