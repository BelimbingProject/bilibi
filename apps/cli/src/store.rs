//! The saved sign-in: one file per Bilibi, readable by its owner alone.
//!
//! A token reaches this program only in a token response and leaves it only
//! in an `Authorization` header to the address it was issued by. It is never
//! an argument, an environment variable, a log line or part of an error. The
//! folder is `0700` and each file `0600`; a file anyone else can read is
//! refused rather than used. Writes replace the file whole (a new `0600` file,
//! then a rename), and every read-refresh-write runs under one lock file, so
//! two commands at once never spend the same single-use refresh token.
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// A message for the person; never contains a credential.
pub type Refusal = String;

/// A device sign-in that is waiting for the person's approval.
#[derive(Clone)]
pub struct Pending {
    pub device_code: String,
    pub user_code: String,
    pub link: String,
    pub expires_at: u64,
    pub interval: u64,
}

/// One Bilibi this machine signed in to. No `Debug`: nothing prints it.
#[derive(Clone)]
pub struct Saved {
    pub origin: String,
    pub server_name: String,
    pub client_id: String,
    pub access_token: Option<String>,
    pub access_expires_at: u64,
    pub refresh_token: Option<String>,
    pub pending: Option<Pending>,
}

impl Saved {
    fn to_json(&self) -> Value {
        json!({
            "origin": self.origin,
            "server_name": self.server_name,
            "client_id": self.client_id,
            "access_token": self.access_token,
            "access_expires_at": self.access_expires_at,
            "refresh_token": self.refresh_token,
            "pending": self.pending.as_ref().map(|pending| json!({
                "device_code": pending.device_code,
                "user_code": pending.user_code,
                "link": pending.link,
                "expires_at": pending.expires_at,
                "interval": pending.interval,
            })),
        })
    }
    fn from_json(value: &Value) -> Option<Self> {
        let text = |value: &Value, name: &str| value.get(name)?.as_str().map(str::to_owned);
        let pending = value.get("pending").filter(|pending| !pending.is_null());
        Some(Self {
            origin: text(value, "origin")?,
            server_name: text(value, "server_name")?,
            client_id: text(value, "client_id")?,
            access_token: text(value, "access_token"),
            access_expires_at: value.get("access_expires_at")?.as_u64()?,
            refresh_token: text(value, "refresh_token"),
            pending: match pending {
                None => None,
                Some(pending) => Some(Pending {
                    device_code: text(pending, "device_code")?,
                    user_code: text(pending, "user_code")?,
                    link: text(pending, "link")?,
                    expires_at: pending.get("expires_at")?.as_u64()?,
                    interval: pending.get("interval")?.as_u64()?,
                }),
            },
        })
    }
}

/// The folder of saved sign-ins.
pub struct Store {
    dir: PathBuf,
}

/// Held for one read-refresh-write; released when dropped.
pub struct Lock {
    _file: File,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
    /// Server names are `Instance::server_name`: lower-case letters, digits
    /// and hyphens. Anything else never becomes a file name.
    fn file(&self, server: &str) -> Result<PathBuf, Refusal> {
        let safe = !server.is_empty()
            && server.len() <= 64
            && server
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if safe {
            Ok(self.dir.join(format!("{server}.json")))
        } else {
            Err(format!("`{server}` is not a server name"))
        }
    }
    fn prepare(&self) -> Result<(), Refusal> {
        let failed = |_| format!("could not prepare {}", self.dir.display());
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&self.dir).map_err(failed)?;
        private(&self.dir, 0o700)
    }
    /// Serialises commands that may refresh the sign-in.
    pub fn lock(&self) -> Result<Lock, Refusal> {
        self.prepare()?;
        let path = self.dir.join("lock");
        let file = options(true)
            .open(&path)
            .map_err(|_| format!("could not open {}", path.display()))?;
        file.lock()
            .map_err(|_| format!("could not lock {}", path.display()))?;
        Ok(Lock { _file: file })
    }
    /// The Bilibi commands use when none is named: the one signed in to last.
    pub fn current(&self) -> Option<String> {
        let name = fs::read_to_string(self.dir.join("current")).ok()?;
        Some(name.trim().to_owned()).filter(|name| !name.is_empty())
    }
    pub fn load(&self, server: &str) -> Result<Option<Saved>, Refusal> {
        let path = self.file(server)?;
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(format!("could not read {}", path.display())),
        };
        private(&path, 0o600)?;
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|_| format!("could not read {}", path.display()))?;
        serde_json::from_str::<Value>(&text)
            .ok()
            .as_ref()
            .and_then(Saved::from_json)
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "{} is not a saved sign-in; delete it and sign in again",
                    path.display()
                )
            })
    }
    /// Replaces the saved sign-in whole and makes it the current one.
    pub fn save(&self, saved: &Saved) -> Result<(), Refusal> {
        self.prepare()?;
        let path = self.file(&saved.server_name)?;
        self.replace(&path, saved.to_json().to_string().as_bytes())?;
        self.replace(&self.dir.join("current"), saved.server_name.as_bytes())
    }
    pub fn forget(&self, server: &str) -> Result<bool, Refusal> {
        let path = self.file(server)?;
        match fs::remove_file(&path) {
            Ok(()) => {
                if self.current().as_deref() == Some(server) {
                    let _ = fs::remove_file(self.dir.join("current"));
                }
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(format!("could not delete {}", path.display())),
        }
    }
    /// A new private file beside the old one, then a rename over it, so a
    /// reader sees the old content or the new and never part of either.
    fn replace(&self, path: &Path, bytes: &[u8]) -> Result<(), Refusal> {
        let failed = |_| format!("could not write {}", path.display());
        let fresh = path.with_extension(format!("{}.new", std::process::id()));
        let _ = fs::remove_file(&fresh);
        let mut file = options(false)
            .create_new(true)
            .open(&fresh)
            .map_err(failed)?;
        let written = file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::rename(&fresh, path));
        if written.is_err() {
            let _ = fs::remove_file(&fresh);
        }
        written.map_err(failed)
    }
}

/// Write access to a file only its owner can read (`0600`).
fn options(create: bool) -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create(create);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

/// Refuses a path other accounts on this machine can reach.
#[cfg(unix)]
fn private(path: &Path, mode: u32) -> Result<(), Refusal> {
    use std::os::unix::fs::PermissionsExt;
    let found = fs::metadata(path)
        .map_err(|_| format!("could not read {}", path.display()))?
        .permissions()
        .mode()
        & 0o777;
    if found & 0o077 == 0 {
        Ok(())
    } else {
        Err(format!(
            "{} can be read by other accounts on this machine (mode {found:o}). Run `chmod {mode:o} {}` and try again",
            path.display(),
            path.display()
        ))
    }
}
#[cfg(not(unix))]
fn private(_path: &Path, _mode: u32) -> Result<(), Refusal> {
    Ok(())
}
