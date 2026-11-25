//! `bilibi-web software-update`: the installed server command's unprivileged
//! half. Root's `bilibi update` wrapper selects the instance and loads its
//! environment; this command re-proves the bootstrap owner, uses the same
//! operation plans as Software > Releases, consumes a fresh 2FA code and
//! follows the queued job to its terminal result.
use bilibi_base::agent_api::{
    plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans},
    Error as ContractError, Registry,
};
use bilibi_database::{Db, Error, Result};
use bilibi_tenancy::{Authentication, ProofFuture, Scope};
use serde_json::{json, Value};
use std::{
    io::{self, IsTerminal, Write},
    process::Command,
    sync::Arc,
    time::Duration,
};
use tokio_postgres::Transaction;

const USAGE: &str = "usage: bilibi update <instance> [--to <version> | --undo]
Run this through the root-only `bilibi` server command. It lists a release from the
pinned Release trust, shows the same review as Software > Releases, asks for the
installation owner's current 2FA code, then follows the installer job to the end.";

#[derive(Default)]
struct Options {
    to: Option<String>,
    undo: bool,
}

fn parse(args: &[String]) -> Option<Options> {
    let mut options = Options::default();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        match flag.as_str() {
            "--to" if options.to.is_none() && !options.undo => {
                let value = rest.next()?.trim();
                if value.is_empty() || value.starts_with("--") {
                    return None;
                }
                options.to = Some(value.to_owned());
            }
            "--undo" if !options.undo && options.to.is_none() => options.undo = true,
            _ => return None,
        }
    }
    Some(options)
}

struct TerminalAuthority {
    authentication: Arc<Authentication>,
    tenant: i64,
    user: i64,
    binding: String,
}
impl TerminalAuthority {
    async fn scope(&self, client: &(impl tokio_postgres::GenericClient + Sync)) -> Result<Scope> {
        self.authentication
            .authenticate(client, self.tenant, self.user, self.binding.clone())
            .await
    }
}
impl PlanAuthority for TerminalAuthority {
    fn verify<'a>(&'a self, tx: &'a Transaction<'_>) -> ProofFuture<'a, PlanIdentity> {
        Box::pin(async move {
            Ok(PlanIdentity {
                scope: self.scope(tx).await?,
                credential_binding: self.binding.clone(),
            })
        })
    }
}

pub async fn software_update(db: &Db, args: &[String]) -> Result<()> {
    let Some(options) = parse(args) else {
        eprintln!("{USAGE}");
        return Err(Error::Invalid);
    };
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        eprintln!(
            "The update command needs an interactive terminal so the 2FA code is not echoed."
        );
        return Err(Error::Invalid);
    }
    let origin = std::env::var("APP_ORIGIN").map_err(|_| Error::Invalid)?;
    let app = crate::App::new(db.clone(), &origin)?;
    let owner = app.user.installation_owner().await?.ok_or_else(|| {
        eprintln!(
            "This instance has no installation owner. Run create-owner and finish its claim first."
        );
        Error::Invalid
    })?;
    let authentication = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let authority = TerminalAuthority {
        authentication,
        tenant: owner.tenant,
        user: owner.user,
        binding: format!("server-terminal:{}:{}", owner.tenant, owner.user),
    };
    let client = db.get().await?;
    let scope = authority.scope(&**client).await.map_err(|_| {
        eprintln!(
            "The installation owner account ({}) is not active.",
            owner.email
        );
        Error::Unauthorized
    })?;
    drop(client);
    let registry = app.services.get::<Registry>("base/agent_api.registry")?;
    let plans = app.services.get::<Plans>("base/agent_api.plans")?;

    let operation = if options.undo {
        PlannedOperation {
            key: "base.software.undo".into(),
            input: json!({}),
        }
    } else {
        PlannedOperation {
            key: "base.software.install".into(),
            input: release_input(&registry, db, &scope, options.to.as_deref()).await?,
        }
    };
    let operations = [operation];
    let preview = plans
        .preview(&authority, &operations)
        .await
        .map_err(report_contract)?;
    if preview["valid"] != true || preview["results"][0]["valid"] != true {
        eprintln!(
            "The software operation was not reviewable: {}",
            preview["results"][0]["error"]
        );
        return Err(Error::Invalid);
    }
    let facts = &preview["results"][0]["preview"]["result"];
    print_review(&preview["instance"], facts);

    let code = prompt_code()?;
    let presented = scope.presenting_step_up(&code);
    let committed = plans
        .commit(&presented, &authority, &operations)
        .await
        .map_err(report_contract)?;
    let job = committed["results"][0]["result"]["result"]["id"]
        .as_i64()
        .ok_or_else(|| {
            eprintln!("The operation committed without returning its installer job number.");
            Error::Internal
        })?;
    println!("Queued installer job {job}. Following its steps:");
    follow(&registry, db, &presented, job).await
}

async fn release_input(
    registry: &Registry,
    db: &Db,
    scope: &Scope,
    wanted: Option<&str>,
) -> Result<Value> {
    let reply = registry
        .read(
            db,
            scope,
            "base.software.releases",
            &json!({"refresh": true, "prerelease": wanted.is_some()}),
        )
        .await
        .map_err(report_contract)?;
    let releases = &reply["result"]["github"];
    let repository = releases["repository"]
        .as_str()
        .unwrap_or("the pinned repository");
    let repository_id = releases["repository_id"].as_i64().unwrap_or(0);
    let rows = releases["releases"].as_array().ok_or_else(|| {
        eprintln!(
            "No release is available from pinned Release trust{}.",
            releases["message"]
                .as_str()
                .map(|message| format!(": {message}"))
                .unwrap_or_default()
        );
        Error::NotFound
    })?;
    println!("Available releases from pinned Release trust ({repository}):");
    for row in rows.iter().filter(|row| row["installed"] != true) {
        println!(
            "  {}  {}  {}{}",
            row["tag"].as_str().unwrap_or("?"),
            row["target"].as_str().unwrap_or("?"),
            row["name"].as_str().unwrap_or("?"),
            if row["prerelease"] == true {
                " (pre-release)"
            } else {
                ""
            }
        );
    }
    let selected = rows
        .iter()
        .filter(|row| row["installed"] != true)
        .find(|row| {
            wanted.is_none_or(|wanted| {
                row["tag"]
                    .as_str()
                    .is_some_and(|tag| version_matches(tag, wanted))
            })
        })
        .ok_or_else(|| {
            match wanted {
                Some(version) => eprintln!("No available release matches {version}."),
                None => eprintln!("There is no newer release to install."),
            }
            Error::NotFound
        })?;
    println!("Selected: {}", selected["tag"].as_str().unwrap_or("?"));
    Ok(json!({
        "asset": selected["asset_id"],
        "repository_id": repository_id,
        "release_id": selected["release_id"],
        "tag": selected["tag"],
        "target": selected["target"],
        "digest": selected["digest"],
        "switch": "auto",
    }))
}

fn version_matches(tag: &str, wanted: &str) -> bool {
    fn version(value: &str) -> &str {
        value
            .strip_prefix("setup-v")
            .or_else(|| value.strip_prefix('v'))
            .unwrap_or(value)
    }
    tag == wanted || version(tag) == version(wanted)
}

fn print_review(instance: &Value, facts: &Value) {
    println!(
        "\n{} · {} · {}",
        instance["label"].as_str().unwrap_or("Bilibi"),
        instance["role"].as_str().unwrap_or("server"),
        instance["origin"].as_str().unwrap_or("")
    );
    println!(
        "Review: {}",
        facts["summary"].as_str().unwrap_or("Software change")
    );
    if let Some(running) = facts["running"]["platform"]["version"].as_str() {
        println!("From: Bilibi {running}");
    } else if let Some(release) = facts["release"].as_i64() {
        println!("From: release {release}");
    }
    if let Some(target) = facts["release_facts"]["platform"]["version"].as_str() {
        println!("To: Bilibi {target}");
    } else if let Some(previous) = facts["previous_release"].as_i64() {
        println!("To: release {previous}");
    }
    println!(
        "Composition: {}",
        facts["change_list"].as_str().unwrap_or(
            "The release composition is checked from the archive before the installer switches."
        )
    );
    if let Some(loss) = facts["loss"].as_str() {
        println!("Backup: {loss}");
    }
    if let Some(what) = facts["what_happens"].as_str() {
        println!("Backup and migration: {what}");
    }
    for warning in facts["warnings"].as_array().into_iter().flatten() {
        if let Some(warning) = warning.as_str() {
            println!("Warning: {warning}");
        }
    }
    println!("Steps:");
    for step in facts["steps"].as_array().into_iter().flatten() {
        println!(
            "  {}. {}",
            step["seq"].as_i64().unwrap_or(0),
            step["label"].as_str().unwrap_or("?")
        );
    }
}

fn prompt_code() -> Result<String> {
    eprint!("Current 2FA code: ");
    io::stderr().flush().map_err(|_| Error::Internal)?;
    let hidden = Command::new("stty")
        .arg("-echo")
        .status()
        .is_ok_and(|status| status.success());
    let mut code = String::new();
    let read = io::stdin().read_line(&mut code);
    if hidden {
        let _ = Command::new("stty").arg("echo").status();
    }
    eprintln!();
    read.map_err(|_| Error::Invalid)?;
    let code = code.trim().to_owned();
    if code.is_empty() || code.len() > 256 {
        eprintln!("Enter the current code from the owner's enrolled second factor.");
        return Err(Error::Invalid);
    }
    Ok(code)
}

async fn follow(registry: &Registry, db: &Db, scope: &Scope, job_id: i64) -> Result<()> {
    let mut seen = std::collections::BTreeMap::<String, String>::new();
    loop {
        let reply = match registry
            .read(db, scope, "base.software.status", &json!({}))
            .await
        {
            Ok(reply) => reply,
            Err(error) => {
                eprintln!("Waiting for the database after the switch: {error}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let status = &reply["result"];
        let job = find_job(status, job_id).ok_or_else(|| {
            eprintln!("Installer job {job_id} is no longer in Software status.");
            Error::NotFound
        })?;
        for step in job["steps"].as_array().into_iter().flatten() {
            let key = step["key"].as_str().unwrap_or("step");
            let state = step["state"].as_str().unwrap_or("pending");
            let message = step["detail"]["message"].as_str().unwrap_or("");
            let signature = format!("{state}\0{message}");
            if seen.get(key) != Some(&signature) {
                println!(
                    "  {:<24} {:<10} {}",
                    step["label"].as_str().unwrap_or(key),
                    state,
                    message
                );
                seen.insert(key.to_owned(), signature);
            }
        }
        match job["state"].as_str().unwrap_or("") {
            "queued" | "running" => tokio::time::sleep(Duration::from_secs(1)).await,
            "done" => {
                println!(
                    "Done: {}",
                    job["result"]["message"]
                        .as_str()
                        .unwrap_or("software update completed")
                );
                return Ok(());
            }
            state => {
                let failed = job["steps"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|step| step["state"] == "failed");
                let step = failed
                    .and_then(|step| step["label"].as_str())
                    .unwrap_or("Installer");
                let message = failed
                    .and_then(|step| step["detail"]["message"].as_str())
                    .or_else(|| job["result"]["message"].as_str())
                    .unwrap_or("the software job did not finish");
                eprintln!("Failed at {step}: {message} (state {state})");
                return Err(Error::Invalid);
            }
        }
    }
}

fn find_job(status: &Value, id: i64) -> Option<&Value> {
    if status["job"]["id"].as_i64() == Some(id) {
        return Some(&status["job"]);
    }
    status["jobs"]
        .as_array()?
        .iter()
        .find(|job| job["id"].as_i64() == Some(id))
}

fn report_contract(error: ContractError) -> Error {
    match &error {
        ContractError::Backend(Error::StepUpRequired { reason, .. }) => match *reason {
            "invalid_code" => eprintln!("That 2FA code was not accepted."),
            "locked" => {
                eprintln!("Too many wrong 2FA codes. Wait, then try again with a fresh code.")
            }
            "not_enrolled" => eprintln!("The installation owner has no enrolled second factor."),
            _ => eprintln!("A fresh owner 2FA code is required."),
        },
        _ => eprintln!("The software operation was refused: {error}"),
    }
    match error {
        ContractError::Backend(error) => error,
        _ => Error::Invalid,
    }
}
