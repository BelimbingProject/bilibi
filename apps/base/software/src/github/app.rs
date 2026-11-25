//! The server's GitHub App: read a private application repository without a
//! token that expires.
//!
//! `scripts/install-server.sh github-app` keeps the App's client ID and
//! private key in `/opt/bilibi/github-app.json` (root:bilibi, 0640), next to
//! the instance folders. Each GitHub use signs a JSON Web Token with the key
//! and asks GitHub for an installation token that can only read the one
//! repository, Contents and Attestations. The token is held in memory for that
//! use and expires within the hour; it is never saved, shown or logged.
//! `scripts/lib/github-app.py` does the same for the terminal installs.
use super::{ACCEPT, API, API_VERSION, LISTING_BYTES, LISTING_TIMEOUT, SYSTEM, USER_AGENT};
use crate::transport::Paths;
use bilibi_integration::{Call, Integration, Origin, Outgoing};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{fs, io, path::PathBuf};

/// The file name beside the instance folders.
const FILE: &str = "github-app.json";
const GUIDE: &str = "docs/server-install.md, Private application repositories";
/// Read-only and nothing else: release files and their build provenance.
const READS: [&str; 2] = ["contents", "attestations"];

pub(crate) struct App {
    pub client_id: String,
    private_key: Vec<u8>,
}

/// Why no token was minted.
pub(crate) enum Refusal {
    /// The App exists and its key works, but it is not installed on the repository.
    NotInstalled(String),
    Other(String),
}
impl Refusal {
    pub fn sentence(self) -> String {
        match self {
            Refusal::NotInstalled(why) | Refusal::Other(why) => why,
        }
    }
}

fn file(paths: Option<&Paths>) -> Option<PathBuf> {
    Some(paths?.dir.parent()?.join(FILE))
}

/// This server's App, `None` when there is none (for example a developer run
/// or a server reading a public repository anonymously).
pub(crate) fn load(paths: Option<&Paths>) -> Result<Option<App>, String> {
    let Some(path) = file(paths) else {
        return Ok(None);
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(format!("This server's GitHub App ({}) cannot be read. Put it here again with scripts/install-server.sh github-app.", path.display())),
    };
    let saved: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let client_id = saved["client_id"].as_str().filter(|id| client_id_ok(id));
    let private_key = saved["private_key"].as_str().and_then(pkcs1_der);
    match (client_id, private_key) {
        (Some(client_id), Some(private_key)) => Ok(Some(App {
            client_id: client_id.to_owned(),
            private_key,
        })),
        _ => Err(format!("{} is not a complete GitHub App. Put it here again with scripts/install-server.sh github-app.", path.display())),
    }
}

fn client_id_ok(id: &str) -> bool {
    let id_bytes = id.as_bytes();
    (id.starts_with("Iv")
        && (3..=42).contains(&id_bytes.len())
        && id_bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'.'))
        || (!id.starts_with('0')
            && (1..=16).contains(&id_bytes.len())
            && id_bytes.iter().all(u8::is_ascii_digit))
}

/// The key GitHub hands out: PKCS #1 RSA in PEM.
fn pkcs1_der(pem: &str) -> Option<Vec<u8>> {
    let body = pem
        .trim()
        .strip_prefix("-----BEGIN RSA PRIVATE KEY-----")?
        .strip_suffix("-----END RSA PRIVATE KEY-----")?;
    let joined: String = body.split_whitespace().collect();
    data_encoding::BASE64.decode(joined.as_bytes()).ok()
}

#[derive(serde::Serialize)]
struct Claims<'a> {
    iat: i64,
    exp: i64,
    iss: &'a str,
}

fn json_web_token(app: &App) -> Result<String, Refusal> {
    let now = jiff::Timestamp::now().as_second();
    // GitHub allows at most ten minutes; a minute back covers a clock a little ahead.
    let claims = Claims {
        iat: now - 60,
        exp: now + 540,
        iss: &app.client_id,
    };
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_rsa_der(&app.private_key),
    )
    .map_err(|_| Refusal::Other(format!("The GitHub App private key on this server cannot sign. Put a new key here with scripts/install-server.sh github-app ({GUIDE}).")))
}

async fn call(
    integration: &Integration,
    scope: &Scope,
    outgoing: Outgoing,
    credential: &str,
    operation: &str,
) -> Result<(u16, Value), Refusal> {
    let outgoing = outgoing
        .header("accept", ACCEPT)
        .header("user-agent", USER_AGENT)
        .header("x-github-api-version", API_VERSION)
        .header("authorization", format!("Bearer {credential}"))
        .timeout(LISTING_TIMEOUT)
        .max_body_bytes(LISTING_BYTES)
        .no_redirects();
    let call = Call {
        system: SYSTEM.into(),
        operation: operation.into(),
        ..Call::default()
    };
    let reply = integration
        .send(Origin::User(scope), &call, outgoing)
        .await
        .map_err(|failure| Refusal::Other(super::failure_sentence(failure)))?;
    let body = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
    Ok((reply.status, body))
}

/// A fresh installation token that can only read `repository`. Refused
/// unless the App is installed on selected repositories only and can only
/// read them, so a key on a server can never change code or releases.
pub(crate) async fn mint(
    integration: &Integration,
    scope: &Scope,
    app: &App,
    repository: &str,
) -> Result<String, Refusal> {
    let id = &app.client_id;
    let (status, found) = call(
        integration,
        scope,
        Outgoing::get(format!("{API}/repos/{repository}/installation")),
        &json_web_token(app)?,
        "app.installation",
    )
    .await?;
    match status {
        200 => {}
        401 => return Err(Refusal::Other(format!("GitHub refused the private key of the GitHub App {id}: the App or this key was deleted. Generate a new key on the App settings page and put it on the server with scripts/install-server.sh github-app ({GUIDE})."))),
        404 => return Err(Refusal::NotInstalled(format!("The GitHub App {id} on this server is not installed on {repository}. Install it on that repository only (\"Only select repositories\"), or set the repository the App was made for."))),
        status => return Err(Refusal::Other(format!("GitHub answered {status} when asked for the App installation on {repository}. Try again in a few minutes."))),
    }
    let granted = found["permissions"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let writes: Vec<&str> = granted
        .iter()
        .filter(|(_, level)| level.as_str() != Some("read"))
        .map(|(name, _)| name.as_str())
        .collect();
    if !writes.is_empty() {
        return Err(Refusal::Other(format!("The GitHub App {id} can change {} of {repository}. A server needs it to read only: on the App settings page, set every permission except Contents and Attestations to No access and those two to Read-only, then accept the change on the install page.", writes.join(", "))));
    }
    let missing: Vec<&str> = READS
        .into_iter()
        .filter(|name| granted.get(*name).and_then(Value::as_str) != Some("read"))
        .collect();
    if !missing.is_empty() {
        return Err(Refusal::Other(format!("The GitHub App {id} cannot read {} of {repository}. On the App settings page, set Contents and Attestations to Read-only, then accept the change on the install page.", missing.join(" and "))));
    }
    if found["repository_selection"].as_str() != Some("selected") {
        let owner = repository.split('/').next().unwrap_or(repository);
        return Err(Refusal::Other(format!("The GitHub App {id} is installed on every repository of {owner}. On its install page choose \"Only select repositories\" and select only {repository}.")));
    }
    let Some(installation) = found["id"].as_i64() else {
        return Err(Refusal::Other(format!(
            "GitHub answered for the App installation on {repository}, but without its number."
        )));
    };
    let name = repository.split('/').nth(1).unwrap_or(repository);
    let (status, minted) = call(
        integration,
        scope,
        Outgoing::post(format!(
            "{API}/app/installations/{installation}/access_tokens"
        ))
        .json(json!({
            "repositories": [name],
            "permissions": {"contents": "read", "attestations": "read"},
        })),
        &json_web_token(app)?,
        "app.token",
    )
    .await?;
    match minted["token"].as_str() {
        Some(token) if status == 201 && !token.is_empty() => Ok(token.to_owned()),
        _ => Err(Refusal::Other(format!("GitHub did not give the App a read token for {repository} (it answered {status}). Try again in a few minutes."))),
    }
}
