//! The one HTTP client: JSON to and from a fixed set of paths on the address
//! the person signed in to. It follows no redirect, so a bearer token is sent
//! to that address and nowhere else, and it refuses plain HTTP except to this
//! computer, so a token never crosses a network unencrypted.
use serde_json::Value;
use std::{sync::Arc, time::Duration};

/// The largest answer read (the API caps a plan at 2 MiB).
const MAX_BODY: usize = 8 << 20;

pub struct Wire {
    http: reqwest::Client,
    /// `harness/model-version-effort`, when the agent stated one.
    self_report: Option<String>,
}

pub struct Answer {
    pub status: u16,
    /// `Null` when the body was not JSON.
    pub body: Value,
    /// Bilibi answers these paths in JSON only; anything else was written
    /// by something in front of it.
    pub json: bool,
}

/// Why no answer came back. Never carries a header or a body.
pub struct Unreachable(pub String);

/// `https://host[:port]`, or `http://` for this computer alone. Returns the
/// address with no trailing slash.
pub fn origin(raw: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|_| {
        format!("`{raw}` is not an address; give one like https://bilibi.example.com")
    })?;
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let local = host == "localhost"
        || host.ends_with(".localhost")
        || host == "127.0.0.1"
        || host == "[::1]";
    let plain = url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && !host.is_empty();
    match url.scheme() {
        "https" if plain => {}
        "http" if plain && local => {}
        "http" if plain => {
            return Err("plain http is only for a Bilibi on this computer; use https".into())
        }
        _ => {
            return Err(format!(
                "`{raw}` is not a Bilibi address; give only https://host, with no path"
            ))
        }
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// Percent-encodes one query value.
pub fn query(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

impl Wire {
    /// Trusts the public certificate roots and this machine's own store, so
    /// a local Bilibi behind a development certificate authority answers too.
    pub fn new(self_report: Option<String>) -> Result<Self, String> {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        for certificate in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(certificate);
        }
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| "could not set up TLS".to_owned())?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(90))
            .user_agent(concat!("bilibi-axi/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| "could not set up the HTTP client".to_owned())?;
        Ok(Self { http, self_report })
    }

    pub async fn get(
        &self,
        origin: &str,
        path: &str,
        bearer: Option<&str>,
    ) -> Result<Answer, Unreachable> {
        self.send(self.http.get(format!("{origin}{path}")), origin, bearer)
            .await
    }

    pub async fn post(
        &self,
        origin: &str,
        path: &str,
        bearer: Option<&str>,
        body: &Value,
    ) -> Result<Answer, Unreachable> {
        let request = self
            .http
            .post(format!("{origin}{path}"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        self.send(request, origin, bearer).await
    }

    async fn send(
        &self,
        mut request: reqwest::RequestBuilder,
        origin: &str,
        bearer: Option<&str>,
    ) -> Result<Answer, Unreachable> {
        request = request
            .header(reqwest::header::ACCEPT, "application/json")
            .header("x-bilibi-agent-id", "bilibi-axi");
        if let Some(report) = &self.self_report {
            request = request.header("x-bilibi-agent-self-report", report);
        }
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        // The cause only: reqwest's own text can repeat the request.
        let unreachable = |error: reqwest::Error| {
            Unreachable(if error.is_timeout() {
                format!("{origin} did not answer in time")
            } else if error.is_connect() {
                format!("could not connect to {origin} (check the address, and that this machine trusts its certificate)")
            } else {
                format!("the request to {origin} failed")
            })
        };
        let mut response = request.send().await.map_err(unreachable)?;
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(unreachable)? {
            if bytes.len() + chunk.len() > MAX_BODY {
                return Err(Unreachable(format!(
                    "{origin} sent more than this command reads"
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        // A non-JSON body (a firewall's or proxy's error page) keeps its status.
        let parsed = serde_json::from_slice(&bytes).ok();
        Ok(Answer {
            status,
            json: parsed.is_some(),
            body: parsed.unwrap_or(Value::Null),
        })
    }
}
