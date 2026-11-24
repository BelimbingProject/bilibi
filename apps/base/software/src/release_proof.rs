//! The release proof policy pinned on this installation.
//!
//! The root-owned record is written at setup, or replaced by the separate
//! installer after an owner-authorized Software job. This process can read
//! it but cannot replace it. GitHub attestations keep their existing online
//! verifier; an operator proof is checked by the installed verifier without
//! making a network call.
use crate::{transport, transport::Paths};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

const VERSION: u8 = 1;
const ED25519_SPKI_PREFIX: &[u8] = &[
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Github,
    Operator,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    version: u8,
    pub kind: Kind,
    pub repository: String,
    pub repository_id: i64,
    pub workflow: String,
    pub minimum_tag: String,
    pub changed_at: String,
    pub changed_by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replay {
    version: u8,
    highest_tag: String,
    highest_digest: String,
}

pub struct Change {
    pub request: Value,
    pub facts: Value,
}

pub fn review_change(input: &Value) -> Result<Change, (String, String)> {
    let field = |name: &str, message: &str| Err((name.to_owned(), message.to_owned()));
    let Some(object) = input.as_object() else {
        return field("trust", "Give the release trust to install.");
    };
    let allowed = [
        "kind",
        "repository",
        "repository_id",
        "workflow",
        "minimum_tag",
        "public_key",
    ];
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return field(
            "trust",
            "The release trust request has an unexpected field.",
        );
    }
    let Some(kind) = input["kind"].as_str() else {
        return field("kind", "Choose github or operator release proof.");
    };
    if !matches!(kind, "github" | "operator") {
        return field("kind", "Choose github or operator release proof.");
    }
    let repository = input["repository"].as_str().unwrap_or("").trim();
    if !valid_repository(repository) {
        return field("repository", "Enter the trusted repository as owner/name.");
    }
    let Some(repository_id) = input["repository_id"].as_i64().filter(|id| *id > 0) else {
        return field(
            "repository_id",
            "Enter the trusted repository's immutable positive number.",
        );
    };
    let workflow = input["workflow"].as_str().unwrap_or("").trim();
    if workflow.is_empty()
        || workflow.len() > 255
        || !workflow.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        return field(
            "workflow",
            "Enter the trusted workflow identity as visible ASCII without spaces.",
        );
    }
    let minimum_tag = input["minimum_tag"].as_str().unwrap_or("").trim();
    if let Err(message) = version(minimum_tag) {
        return field("minimum_tag", &message);
    }
    let public_key = input["public_key"].as_str().map(str::trim);
    let fingerprint = match kind {
        "operator" => {
            let Some(public_key) = public_key.filter(|key| !key.is_empty()) else {
                return field(
                    "public_key",
                    "Paste the operator's Ed25519 public key in PEM form.",
                );
            };
            if public_key.len() > 16 * 1024 {
                return field("public_key", "The operator public key is too large.");
            }
            match public_key_fingerprint(public_key) {
                Ok(fingerprint) => Some(fingerprint),
                Err(message) => return field("public_key", &message),
            }
        }
        _ if public_key.is_some_and(|key| !key.is_empty()) => {
            return field(
                "public_key",
                "GitHub release trust does not use an operator public key.",
            );
        }
        _ => None,
    };
    let request = json!({
        "kind": kind,
        "repository": repository,
        "repository_id": repository_id,
        "workflow": workflow,
        "minimum_tag": minimum_tag,
        "public_key": if kind == "operator" { Value::String(public_key.unwrap_or_default().to_owned()) } else { Value::Null },
    });
    let facts = json!({
        "kind": kind,
        "repository": repository,
        "repository_id": repository_id,
        "workflow": workflow,
        "minimum_tag": minimum_tag,
        "fingerprint": fingerprint,
    });
    Ok(Change { request, facts })
}

fn public_key_fingerprint(public_key: &str) -> Result<String, String> {
    let mut child = Command::new("openssl")
        .args(["pkey", "-pubin", "-outform", "DER"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("OpenSSL could not check the operator public key: {error}."))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "OpenSSL could not read the operator public key.".to_owned())?
        .write_all(public_key.as_bytes())
        .map_err(|_| "OpenSSL could not read the operator public key.".to_owned())?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("OpenSSL could not check the operator public key: {error}."))?;
    if !output.status.success() {
        return Err("The operator public key is not valid PEM.".into());
    }
    if output.stdout.len() != ED25519_SPKI_PREFIX.len() + 32
        || !output.stdout.starts_with(ED25519_SPKI_PREFIX)
    {
        return Err("The operator release signing key must be Ed25519.".into());
    }
    let digest = Sha256::digest(&output.stdout);
    Ok(format!(
        "sha256:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

impl Policy {
    pub fn load(paths: &Paths) -> Result<Self, String> {
        let path = paths.dir.join("trust/release.json");
        let bytes = fs::read(&path).map_err(|error| {
            format!(
                "This server has no readable release trust record ({}: {error}). Run scripts/install-server.sh trust before installing a release.",
                path.display()
            )
        })?;
        if bytes.len() > 64 * 1024 {
            return Err("The server's release trust record is too large.".into());
        }
        let policy: Self = serde_json::from_slice(&bytes)
            .map_err(|_| "The server's release trust record is not valid.".to_owned())?;
        policy.validate()?;
        Ok(policy)
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != VERSION {
            return Err("The server's release trust record has an unsupported version.".into());
        }
        if !valid_repository(&self.repository) || self.repository_id <= 0 {
            return Err("The server's trusted release repository is invalid.".into());
        }
        if self.workflow.is_empty()
            || self.workflow.len() > 255
            || !self
                .workflow
                .bytes()
                .all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return Err("The server's trusted release workflow is invalid.".into());
        }
        version(&self.minimum_tag)?;
        if self.changed_at.parse::<jiff::Timestamp>().is_err()
            || self.changed_by.is_empty()
            || self.changed_by.len() > 255
            || !self
                .changed_by
                .bytes()
                .all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return Err("The server's release trust change record is invalid.".into());
        }
        match self.kind {
            Kind::Github if self.public_key.is_some() || self.fingerprint.is_some() => {
                Err("GitHub release trust must not carry an operator key.".into())
            }
            Kind::Operator => {
                let key = self.public_key.as_deref().ok_or_else(|| {
                    "Operator release trust needs an Ed25519 public key.".to_owned()
                })?;
                if key.len() > 16 * 1024 {
                    return Err("The operator public key is too large.".into());
                }
                let recorded = self
                    .fingerprint
                    .as_deref()
                    .filter(|fingerprint| valid_fingerprint(fingerprint))
                    .ok_or_else(|| {
                        "Operator release trust needs a valid key fingerprint.".to_owned()
                    })?;
                let actual = public_key_fingerprint(key)?;
                if actual != recorded {
                    return Err(
                        "The operator public key does not match its trusted fingerprint.".into(),
                    );
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub fn facts(&self) -> Value {
        json!({
            "kind": match self.kind { Kind::Github => "github", Kind::Operator => "operator" },
            "repository": self.repository,
            "repository_id": self.repository_id,
            "workflow": self.workflow,
            "minimum_tag": self.minimum_tag,
            "fingerprint": self.fingerprint,
            "changed_at": self.changed_at,
            "changed_by": self.changed_by,
        })
    }

    pub fn requires_operator_proof(&self) -> bool {
        self.kind == Kind::Operator
    }

    pub fn check_tag(&self, tag: &str) -> Result<(), String> {
        if version(tag)? < version(&self.minimum_tag)? {
            return Err(format!(
                "Release {tag} is older than this server's minimum trusted release {}.",
                self.minimum_tag
            ));
        }
        Ok(())
    }

    pub fn admission(&self, tag: &str, target: &str, digest: &str) -> Value {
        json!({
            "version": VERSION,
            "proof": self.kind,
            "repository": self.repository,
            "repository_id": self.repository_id,
            "workflow": self.workflow,
            "tag": tag,
            "target": target,
            "archive_sha256": digest,
            "fingerprint": self.fingerprint,
        })
    }

    pub fn check_admission(&self, source: &Value) -> Result<(), String> {
        let facts = self.facts();
        if source["proof"] != facts["kind"]
            || source["repository"] != facts["repository"]
            || source["repository_id"] != facts["repository_id"]
            || source["workflow"] != facts["workflow"]
            || source["fingerprint"] != facts["fingerprint"]
        {
            return Err("This release was verified under a different Release trust policy.".into());
        }
        self.check_tag(source["tag"].as_str().unwrap_or(""))
    }

    pub fn verify(
        &self,
        paths: &Paths,
        artifact: &std::path::Path,
        proof: Option<&std::path::Path>,
        tag: &str,
        target: &str,
        token: Option<&str>,
    ) -> Result<(), String> {
        self.check_tag(tag)?;
        match self.kind {
            Kind::Github => {
                if proof.is_some() {
                    return Err(
                        "This server trusts GitHub attestations, not an operator proof.".into(),
                    );
                }
                transport::verify_attestation(&helper_path(paths)?, artifact, self, tag, token)
            }
            Kind::Operator => {
                let proof = proof.ok_or_else(|| {
                    "This server requires the operator proof published beside the release file."
                        .to_owned()
                })?;
                let helper = helper_path(paths)?;
                let mut verifier = Command::new("python3")
                    .arg(&helper)
                    .args(["verify-operator", "--artifact"])
                    .arg(artifact)
                    .arg("--proof")
                    .arg(proof)
                    .args(["--trust", "/dev/stdin"])
                    .args(["--tag", tag, "--kind", "setup", "--target", target])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|error| {
                        format!(
                            "The installed release proof verifier could not run ({}: {error}).",
                            helper.display()
                        )
                    })?;
                let mut stdin = verifier
                    .stdin
                    .take()
                    .ok_or("The release proof verifier has no input.")?;
                let sent = serde_json::to_writer(&mut stdin, self);
                drop(stdin);
                let output = verifier.wait_with_output().map_err(|error| {
                    format!("The release proof verifier did not finish: {error}.")
                })?;
                sent.map_err(|error| {
                    format!("The release trust could not be sent to the verifier: {error}.")
                })?;
                if output.status.success() {
                    return Ok(());
                }
                let detail = String::from_utf8_lossy(&output.stderr);
                Err(format!(
                    "The operator release proof was refused: {}",
                    detail.trim().trim_start_matches("release-proof: ")
                ))
            }
        }
    }
}

pub fn check_replay(paths: &Paths, tag: &str) -> Result<(), String> {
    let wanted = version(tag)?;
    let Some(replay) = load_replay(paths)? else {
        return Ok(());
    };
    if wanted <= version(&replay.highest_tag)? {
        return Err(format!(
            "Release {tag} conflicts with the highest admitted release {}. Use Software Undo for a deliberate rollback.",
            replay.highest_tag
        ));
    }
    Ok(())
}

pub fn facts(paths: &Paths) -> Result<Value, String> {
    let mut facts = Policy::load(paths)?.facts();
    if let Some(replay) = load_replay(paths)? {
        facts["highest_admitted_tag"] = json!(replay.highest_tag);
        facts["highest_admitted_digest"] = json!(replay.highest_digest);
    } else {
        facts["highest_admitted_tag"] = Value::Null;
        facts["highest_admitted_digest"] = Value::Null;
    }
    Ok(facts)
}

fn load_replay(paths: &Paths) -> Result<Option<Replay>, String> {
    let path = replay_path(paths);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "The release replay record could not be read ({}: {error}).",
                path.display()
            ));
        }
    };
    let replay: Replay = serde_json::from_slice(&bytes)
        .map_err(|_| "The release replay record is not valid.".to_owned())?;
    if replay.version != VERSION
        || replay.highest_digest.len() != 64
        || !replay
            .highest_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("The release replay record is not valid.".into());
    }
    version(&replay.highest_tag)?;
    Ok(Some(replay))
}

pub fn helper_path(paths: &Paths) -> Result<PathBuf, String> {
    Ok(server_root(paths)?.join("release-proof.py"))
}

pub fn replay_path(paths: &Paths) -> PathBuf {
    paths.dir.join("trust/release-state.json")
}

fn server_root(paths: &Paths) -> Result<&std::path::Path, String> {
    paths.dir.parent().ok_or_else(|| {
        format!(
            "{} has no server folder, so release trust cannot be read.",
            paths.dir.display()
        )
    })
}

fn valid_repository(value: &str) -> bool {
    let mut parts = value.split('/');
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    };
    matches!((parts.next(), parts.next(), parts.next()), (Some(owner), Some(name), None) if valid(owner) && valid(name))
}

fn valid_fingerprint(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn version(tag: &str) -> Result<(u64, u64, u64), String> {
    let Some(value) = tag.strip_prefix("setup-v") else {
        return Err("The release tag must be setup-vX.Y.Z.".into());
    };
    let mut parts = value.split('.');
    let parse = |part: Option<&str>| -> Option<u64> {
        let part = part?;
        if part.is_empty()
            || (part.len() > 1 && part.starts_with('0'))
            || !part.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        part.parse().ok()
    };
    match (
        parse(parts.next()),
        parse(parts.next()),
        parse(parts.next()),
        parts.next(),
    ) {
        (Some(major), Some(minor), Some(patch), None) => Ok((major, minor, patch)),
        _ => Err("The release tag must be setup-vX.Y.Z.".into()),
    }
}
