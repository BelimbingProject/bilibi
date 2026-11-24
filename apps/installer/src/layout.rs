//! One instance's folder under `/opt/bilibi`, as `scripts/install-server.sh`
//! lays it out (`docs/server-install.md`): `env`, `releases/<n>`, the
//! `current` link, `backups/` and `data/`.
use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

/// The files the installer copies with a release, when the build wrote them
/// (`release.json`) or the release came as a file (`bundle.json`). Add-ons
/// that travel with the release are the `plugins/` folder beside them.
const RELEASE_FILES: [&str; 7] = [
    "bilibi-web",
    "SHA256SUMS",
    "manifest.txt",
    "Cargo.lock",
    "release.json",
    "bundle.json",
    "admission.json",
];
const PLUGINS_DIR: &str = "plugins";
/// The add-ons a release switches on, included by the local build in the
/// release's `plugins/` folder.
const PLUGINS_ENABLED: &str = "enabled.json";

#[derive(Clone, Debug)]
pub struct Layout {
    /// The instance's folder name: `production`, `dev`.
    pub name: String,
    /// `/opt/bilibi/<name>`
    pub dir: PathBuf,
}
impl Layout {
    /// The instance whose folder this is. The folder must hold an `env`
    /// file, which only `install-server.sh create` writes.
    pub fn open(dir: &Path) -> Result<Self, String> {
        let dir = dir
            .canonicalize()
            .map_err(|error| format!("{}: {error}", dir.display()))?;
        let name = dir
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| valid_instance_name(name))
            .ok_or_else(|| {
                format!(
                    "{} is not an instance folder: the name must be 1 to 32 lower-case letters, digits or hyphens",
                    dir.display()
                )
            })?
            .to_owned();
        if !dir.join("env").is_file() {
            return Err(format!(
                "{} has no env file; create the instance with scripts/install-server.sh create {name} first",
                dir.display()
            ));
        }
        Ok(Self { name, dir })
    }
    pub fn releases(&self) -> PathBuf {
        self.dir.join("releases")
    }
    pub fn release(&self, n: u64) -> PathBuf {
        self.releases().join(n.to_string())
    }
    pub fn backups(&self) -> PathBuf {
        self.dir.join("backups")
    }
    fn server_root(&self) -> Result<&Path, String> {
        self.dir
            .parent()
            .ok_or_else(|| format!("{} has no Bilibi server folder", self.dir.display()))
    }
    pub fn check_release_tag(&self, tag: &str) -> Result<(), String> {
        self.release_proof(&["check-replay", "--state"], tag, None)
    }
    pub fn record_release(&self, tag: &str, digest: &str) -> Result<(), String> {
        self.release_proof(&["record-replay", "--state"], tag, Some(digest))
    }
    fn release_proof(
        &self,
        operation: &[&str],
        tag: &str,
        digest: Option<&str>,
    ) -> Result<(), String> {
        let root = self.server_root()?;
        let state = self.dir.join("trust/release-state.json");
        let mut command = Command::new("python3");
        command
            .arg(root.join("release-proof.py"))
            .args(operation)
            .arg(&state);
        command.args(["--tag", tag]);
        if let Some(digest) = digest {
            command.args(["--digest", digest]);
        }
        let output = command
            .output()
            .map_err(|error| format!("could not run the release proof verifier: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        Err(String::from_utf8_lossy(&output.stderr)
            .trim()
            .trim_start_matches("release-proof: ")
            .to_owned())
    }
    pub fn replace_release_trust(
        &self,
        job: i64,
        requested_by: i64,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let object = request
            .as_object()
            .ok_or("the release trust request is not an object")?;
        let allowed = [
            "kind",
            "repository",
            "repository_id",
            "workflow",
            "minimum_tag",
            "public_key",
        ];
        if object.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err("the release trust request has an unexpected field".into());
        }
        let kind = request["kind"]
            .as_str()
            .filter(|kind| matches!(*kind, "github" | "operator"))
            .ok_or("the release trust kind must be github or operator")?;
        let repository = request["repository"]
            .as_str()
            .ok_or("the release trust request names no repository")?;
        let repository_id = request["repository_id"]
            .as_i64()
            .filter(|id| *id > 0)
            .ok_or("the release trust request names no repository number")?;
        let workflow = request["workflow"]
            .as_str()
            .ok_or("the release trust request names no workflow")?;
        let minimum_tag = request["minimum_tag"]
            .as_str()
            .ok_or("the release trust request names no minimum tag")?;
        let root = self.server_root()?;
        let trust = root.join("trust");
        let next = trust.join(format!(".release-{job}.json"));
        let public = trust.join(format!(".release-{job}.pem"));
        let _ = fs::remove_file(&next);
        let _ = fs::remove_file(&public);
        if kind == "operator" {
            let key = request["public_key"]
                .as_str()
                .filter(|key| !key.trim().is_empty())
                .ok_or("operator release trust has no public key")?;
            fs::write(&public, key)
                .map_err(|error| format!("could not stage the operator public key: {error}"))?;
            fs::set_permissions(&public, fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("could not protect the staged public key: {error}"))?;
        } else if !request["public_key"].is_null() {
            return Err("GitHub release trust must not carry an operator public key".into());
        }
        let mut command = Command::new("python3");
        command
            .arg(root.join("release-proof.py"))
            .args(["make-trust", "--kind", kind, "--repository", repository])
            .args(["--repository-id", &repository_id.to_string()])
            .args(["--workflow", workflow, "--minimum-tag", minimum_tag])
            .args(["--changed-by", &format!("account:{requested_by}")])
            .arg("--output")
            .arg(&next);
        if kind == "operator" {
            command.arg("--public-key").arg(&public);
        }
        let output = command
            .output()
            .map_err(|error| format!("could not run the release proof verifier: {error}"))?;
        let _ = fs::remove_file(&public);
        if !output.status.success() {
            let _ = fs::remove_file(&next);
            return Err(String::from_utf8_lossy(&output.stderr)
                .trim()
                .trim_start_matches("release-proof: ")
                .to_owned());
        }
        fs::set_permissions(&next, fs::Permissions::from_mode(0o640))
            .map_err(|error| format!("could not protect the release trust record: {error}"))?;
        fs::rename(&next, trust.join("release.json"))
            .map_err(|error| format!("could not replace the release trust record: {error}"))?;
        serde_json::from_slice(&output.stdout)
            .map_err(|_| "the release proof verifier returned no trust facts".into())
    }
    /// Where the program puts a release that came from another machine or
    /// from an uploaded file, for this installer to take (`base/software`,
    /// `transport.rs`): `incoming/job-<id>/` or `incoming/upload-<n>/`.
    pub fn incoming(&self, name: &str) -> Option<PathBuf> {
        let digits = |rest: &str| {
            !rest.is_empty() && rest.len() <= 12 && rest.bytes().all(|b| b.is_ascii_digit())
        };
        let valid = name
            .strip_prefix("job-")
            .or_else(|| name.strip_prefix("upload-"))
            .is_some_and(digits);
        valid.then(|| self.dir.join("incoming").join(name))
    }
    /// The kept release numbers, lowest first.
    pub fn release_numbers(&self) -> Vec<u64> {
        let mut numbers: Vec<u64> = fs::read_dir(self.releases())
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().is_dir())
                    .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        numbers.sort_unstable();
        numbers
    }
    /// The release in service, if any: `current` is a fixed link to
    /// `releases/current`, which points at the release's number.
    pub fn current(&self) -> Option<u64> {
        let target = fs::canonicalize(self.dir.join("current")).ok()?;
        target.file_name()?.to_str()?.parse().ok()
    }
    pub fn next_release(&self) -> u64 {
        self.release_numbers().last().map_or(1, |last| last + 1)
    }
    /// Points the release in service at `releases/<n>` in one step: a new
    /// `releases/current` link is made beside the old one and renamed over
    /// it, so there is never a moment without one. The instance folder and
    /// its `current` link belong to root; `releases/` belongs to this user,
    /// which is why the moving link lives there.
    pub fn point_current(&self, n: u64) -> io::Result<()> {
        if !self.converted() {
            return Err(io::Error::other(format!(
                "{} must be a link to releases/current (rerun scripts/install-server.sh create {} to convert it)",
                self.dir.join("current").display(),
                self.name
            )));
        }
        let fresh = self.releases().join("current.new");
        let _ = fs::remove_file(&fresh);
        std::os::unix::fs::symlink(n.to_string(), &fresh)?;
        fs::rename(&fresh, self.releases().join("current"))
    }
    /// Whether `current` is the fixed link to `releases/current`, the shape
    /// the installer can move. An older instance is converted by
    /// `scripts/install-server.sh installer` or `create`.
    pub fn converted(&self) -> bool {
        fs::read_link(self.dir.join("current"))
            .is_ok_and(|link| link == Path::new("releases/current"))
    }
    /// What to tell the owner when `converted` is false.
    pub fn conversion_advice(&self) -> String {
        format!(
            "{} still has the older folder layout, so a release cannot be switched here. Run `sudo scripts/install-server.sh installer <bilibi-installer>` (or `create {}`) on the server to convert it, then start again.",
            self.name, self.name
        )
    }
    /// One value from the instance's `env` file (`BIND_ADDR`, `APP_ORIGIN`).
    pub fn env_value(&self, key: &str) -> Option<String> {
        let text = fs::read_to_string(self.dir.join("env")).ok()?;
        text.lines()
            .filter_map(|line| line.split_once('='))
            .find(|(name, _)| name.trim() == key)
            .map(|(_, value)| value.trim().to_owned())
    }
    /// The systemd unit that runs this instance.
    pub fn unit(&self) -> String {
        format!("bilibi@{}", self.name)
    }
}

/// The same rule `scripts/install-server.sh` and `bilibi_system` apply.
pub fn valid_instance_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
}

/// Checks `SHA256SUMS` in a release folder and returns the program's
/// checksum. Uses the system's `sha256sum`, the same tool that wrote the file.
pub fn verify_checksum(dir: &Path) -> Result<String, String> {
    let sums = fs::read_to_string(dir.join("SHA256SUMS"))
        .map_err(|error| format!("{} has no SHA256SUMS ({error})", dir.display()))?;
    let sha = sums
        .lines()
        .find_map(|line| {
            let (sha, name) = line.split_once(char::is_whitespace)?;
            (name.trim_start_matches('*').trim() == "bilibi-web" && sha.len() == 64)
                .then(|| sha.to_owned())
        })
        .ok_or_else(|| {
            format!(
                "{} does not name bilibi-web",
                dir.join("SHA256SUMS").display()
            )
        })?;
    let checked = Command::new("sha256sum")
        .args(["-c", "--quiet", "SHA256SUMS"])
        .current_dir(dir)
        .output()
        .map_err(|error| format!("could not run sha256sum: {error}"))?;
    if !checked.status.success() {
        return Err(format!(
            "The program in {} does not match its checksum list (SHA256SUMS): the release was damaged on the way. Make the release file again on the source, or build it again.",
            dir.display()
        ));
    }
    Ok(sha.to_ascii_lowercase())
}

/// Copies a release folder's files, and its add-ons, into `dst` and
/// returns the program's size.
pub fn copy_release(src: &Path, dst: &Path) -> io::Result<u64> {
    fs::create_dir_all(dst)?;
    fs::set_permissions(dst, fs::Permissions::from_mode(0o755))?;
    let mut bytes = 0;
    for name in RELEASE_FILES {
        let from = src.join(name);
        if !from.is_file() {
            continue;
        }
        let to = dst.join(name);
        let copied = fs::copy(&from, &to)?;
        let mode = if name == "bilibi-web" { 0o755 } else { 0o644 };
        fs::set_permissions(&to, fs::Permissions::from_mode(mode))?;
        if name == "bilibi-web" {
            bytes = copied;
        }
    }
    let plugins = src.join(PLUGINS_DIR);
    if plugins.is_dir() {
        copy_plain_files(&plugins, &dst.join(PLUGINS_DIR))?;
    }
    Ok(bytes)
}
/// The add-ons the release at `dir` switches on (`plugins/enabled.json`), as
/// `{id, name, publisher, version, source}`; `None` when the release does not
/// say (one made before add-ons travelled), which leaves the server's own
/// list as it is. A file that cannot be read as a list is an error: the
/// release was damaged on the way.
pub fn read_plugins(dir: &Path) -> Result<Option<Vec<serde_json::Value>>, String> {
    use serde_json::{json, Value};
    let path = dir.join(PLUGINS_DIR).join(PLUGINS_ENABLED);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    let damaged = || {
        format!(
            "{PLUGINS_DIR}/{PLUGINS_ENABLED} is not a list of add-ons. Make the release file again on the server it came from."
        )
    };
    let list: Vec<Value> = serde_json::from_str(&text).map_err(|_| damaged())?;
    let text_of = |value: &Value, most: usize| {
        value
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| text.chars().take(most).collect::<String>())
    };
    let mut plugins: Vec<Value> = Vec::new();
    for entry in list {
        let id = text_of(&entry["id"], 64)
            .filter(|id| {
                id.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-.".contains(&b))
            })
            .ok_or_else(damaged)?;
        if plugins.iter().any(|plugin| plugin["id"] == id.as_str()) {
            return Err(damaged());
        }
        plugins.push(json!({
            "id": id,
            "name": text_of(&entry["name"], 120).unwrap_or_else(|| id.clone()),
            "publisher": text_of(&entry["publisher"], 120),
            "version": text_of(&entry["version"], 32),
            "source": text_of(&entry["source"], 255).unwrap_or_else(|| "catalogue".to_owned()),
        }));
    }
    plugins.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Ok(Some(plugins))
}

/// Copies the plain files under `src` (and its folders) into `dst`; links
/// and anything else are left out.
fn copy_plain_files(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    fs::set_permissions(dst, fs::Permissions::from_mode(0o755))?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if kind.is_dir() {
            copy_plain_files(&entry.path(), &to)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &to)?;
            fs::set_permissions(&to, fs::Permissions::from_mode(0o644))?;
        }
    }
    Ok(())
}

/// Whether the program in `dir` was built for this kind of server: a Linux
/// program for this processor (read from the program itself) that needs a
/// system library this server has (`release.json`, when the local build
/// recorded it). Each refusal says what to do. The same check the program
/// makes when it receives a release (`base/software`, `transport.rs`).
pub fn built_for_this_server(dir: &Path) -> Result<(), String> {
    use std::io::Read;
    let mut head = [0u8; 20];
    let read = fs::File::open(dir.join("bilibi-web"))
        .and_then(|mut file| file.read(&mut head))
        .map_err(|error| format!("The program could not be read: {error}."))?;
    if read < 20 || head[..4] != [0x7f, b'E', b'L', b'F'] {
        return Err("The file called bilibi-web is not a program. Make the release file again on the source.".into());
    }
    let machine = u16::from_le_bytes([head[18], head[19]]);
    let built_arch = match (head[4], head[5], machine) {
        (2, 1, 0x3E) => "x86_64",
        (2, 1, 0xB7) => "aarch64",
        (1, 1, 0x03) => "x86",
        (1, 1, 0x28) => "arm",
        _ => "another kind of processor",
    };
    let here = std::env::consts::ARCH;
    if built_arch != here {
        return Err(format!(
            "This release was built for {} computers, and this server is {}. Build it locally for the same kind of server.",
            describe_arch(built_arch),
            describe_arch(here)
        ));
    }
    if !matches!(head[7], 0 | 3) || std::env::consts::OS != "linux" {
        return Err(
            "This release was built for another operating system. Build it locally for Linux."
                .into(),
        );
    }
    let recipe: serde_json::Value = fs::read_to_string(dir.join("release.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null);
    if let Some(needed) = recipe["built_for"]["system_library"].as_str() {
        if let (Some(need), Some(have_text)) = (library_version(needed), system_library()) {
            if let Some(have) = library_version(&have_text) {
                if have < need {
                    return Err(format!(
                        "This release needs the system library {needed}, and this server has {have_text}. Update this server, or build locally with a system library no newer than this server's."
                    ));
                }
            }
        }
    }
    Ok(())
}
fn describe_arch(arch: &str) -> &str {
    match arch {
        "x86_64" => "Linux x86-64",
        "aarch64" => "Linux ARM 64-bit",
        other => other,
    }
}
/// `glibc 2.39`, from `getconf GNU_LIBC_VERSION`; None when unknown.
fn system_library() -> Option<String> {
    let out = Command::new("getconf")
        .arg("GNU_LIBC_VERSION")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !text.is_empty()).then_some(text)
}
fn library_version(text: &str) -> Option<(u64, u64)> {
    let version = text.split_whitespace().last()?;
    let (major, minor) = version.split_once('.')?;
    let minor: String = minor.chars().take_while(char::is_ascii_digit).collect();
    Some((major.parse().ok()?, minor.parse().ok()?))
}
/// Free space on the file system holding `path`, in bytes.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let out = Command::new("df")
        .args(["--output=avail", "-B1"])
        .arg(path)
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .nth(1)?
        .trim()
        .parse()
        .ok()
}

/// Bytes as "1.2 GB" or "340 MB".
pub fn human(bytes: u64) -> String {
    const GB: f64 = 1_073_741_824.0;
    const MB: f64 = 1_048_576.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else {
        format!("{:.0} MB", (bytes / MB).max(1.0))
    }
}
