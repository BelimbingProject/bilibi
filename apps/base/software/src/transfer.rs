//! GitHub downloads and manual uploads, handed to the separate installer.
//! Archive checks live in `transport.rs`.
use crate::{
    jobs, release_proof, review,
    transport::{self, Paths},
    Software, MANAGE, VIEW,
};
use bilibi_authz::authorize_operator;
use bilibi_database::{Error, Result};
use bilibi_integration::{Call, Failure, Origin};
use bilibi_system::Instance;
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::io::AsyncWrite;

/// How often the program looks for an install job waiting for its download.
const TRANSFER_EVERY: Duration = Duration::from_secs(5);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Room kept free beyond the file and its unpacked copy.
const DISK_MARGIN: u64 = 64 * 1024 * 1024;
/// What `take_upload` found in an uploaded release file.
pub struct Upload {
    /// The folder name under `incoming/`, which an install names as `bundle`.
    pub bundle: String,
    pub facts: Value,
}

impl Software {
    /// The instance folder, or the plain reason there is none.
    pub(crate) fn paths(&self) -> std::result::Result<&Paths, String> {
        self.paths.as_ref().ok_or_else(|| {
            format!(
                "This program runs without an instance folder ({} is not set), so it keeps no releases and can receive none.",
                Paths::INSTANCE_DIR_KEY
            )
        })
    }
    /// Removes what a cancelled job's download left in `incoming/`: the
    /// folder it was unpacked into, or the file still on its way.
    pub(crate) fn drop_download(&self, job: &jobs::Job) {
        let Ok(paths) = self.paths() else { return };
        let folder = format!("{}{}", transport::JOB_PREFIX, job.id);
        let _ = fs::remove_dir_all(paths.incoming_folder(&folder));
        let _ = fs::remove_file(paths.incoming().join(format!("{folder}.tar.gz.part")));
    }
    /// The worker name a download runs under in the job rows.
    fn download_worker(&self) -> String {
        match &self.paths {
            Some(paths) => format!("download@{}", paths.name),
            None => "download".into(),
        }
    }

    /// GitHub assets, installed identity and files uploaded here.
    pub async fn releases(&self, scope: &Scope) -> Result<Value> {
        self.releases_with(scope, false, false).await
    }
    /// Refresh GitHub on request; page polling never calls this.
    pub async fn releases_with(
        &self,
        scope: &Scope,
        force: bool,
        prerelease: bool,
    ) -> Result<Value> {
        let client = self.db.get().await?;
        authorize_operator(&**client, scope, VIEW).await?;
        let instance = Instance::resolve(&self.settings, &**client).await?;
        let uploads = self
            .paths
            .as_ref()
            .map(Paths::uploads)
            .unwrap_or_default()
            .into_iter()
            .map(|(bundle, mut facts)| {
                facts["bundle"] = json!(bundle);
                facts
            })
            .collect::<Vec<_>>();
        let installed = self.installed_asset(&**client, scope).await?;
        let (trust, mut github) = match self.paths.as_ref() {
            Some(paths) => {
                let policy = release_proof::Policy::load(paths).map_err(Error::refused)?;
                let trust = release_proof::facts(paths).map_err(Error::refused)?;
                let github = self
                    .github_listing(&**client, scope, force, prerelease, &policy)
                    .await?;
                (trust, github)
            }
            None => (
                Value::Null,
                json!({
                    "available": false,
                    "message": "This program is not running from an installed instance, so it has no release trust record.",
                }),
            ),
        };
        let repository_id = github["repository_id"].clone();
        if let Some(releases) = github["releases"].as_array_mut() {
            for release in releases {
                release["repository_id"] = repository_id.clone();
                release["installed"] =
                    json!(crate::github::same_asset(release, &installed["github"]));
            }
        }
        let update = crate::update::discover(self.paths.as_ref(), &github);
        if let Some(fields) = github.as_object_mut() {
            fields.remove("latest");
            fields.remove("complete");
        }
        Ok(
            json!({"instance": review::instance_json(&instance), "release_trust": trust, "installed_asset": installed, "uploads": uploads, "github": github, "update":update}),
        )
    }

    /// A release file the owner uploaded: unpacked into `incoming/upload-<n>/`
    /// and checked. `Ok(Err)` is the plain reason it was refused; the folder
    /// is then gone.
    pub async fn take_upload(
        &self,
        scope: &Scope,
        filename: &str,
        bytes: Vec<u8>,
        proof: Option<(String, Vec<u8>)>,
    ) -> Result<std::result::Result<Upload, String>> {
        let client = self.db.get().await?;
        authorize_operator(&**client, scope, MANAGE).await?;
        drop(client);
        let paths = match self.paths() {
            Ok(paths) => paths.clone(),
            Err(why) => return Ok(Err(why)),
        };
        let policy = match release_proof::Policy::load(&paths) {
            Ok(policy) => policy,
            Err(why) => return Ok(Err(why)),
        };
        let token = if policy.requires_operator_proof() {
            None
        } else {
            match self.github_token(scope, &policy.repository).await {
                Ok(token) => token,
                Err(why) => return Ok(Err(why)),
            }
        };
        if fs::create_dir_all(paths.incoming()).is_err() || !paths.incoming().is_dir() {
            return Ok(Err(format!(
                "This instance has no incoming folder ({}). Run scripts/install-server.sh create {} again on the server to add it.",
                paths.incoming().display(),
                paths.name
            )));
        }
        if let Some(free) = transport::free_bytes(&paths.incoming()) {
            let need = bytes.len() as u64 * 4 + DISK_MARGIN;
            if free < need {
                return Ok(Err(format!(
                    "Not enough disk to unpack the file: {} free, about {} needed. Free some space on the server first.",
                    transport::human(free),
                    transport::human(need)
                )));
            }
        }
        let filename = filename.to_owned();
        let unpacked = tokio::task::spawn_blocking(move || {
            let lock = instance_lock(&paths)?;
            lock.try_lock().map_err(|error| format!("Could not lock the instance to receive the upload: {error}."))?;
            let bundle = paths.next_upload();
            let into = paths.incoming_folder(&bundle);
            let archive = paths.incoming().join(format!("{bundle}.tar.gz.part"));
            let proof_path = paths.incoming().join(format!("{bundle}.proof.json.part"));
            let mut staged = StagedRelease::new(&archive, &proof_path, &into);
        fs::write(&archive, &bytes)
            .map_err(|error| format!("The uploaded release could not be saved for verification: {error}."))?;
        if let Some((_, proof_bytes)) = proof.as_ref() {
            fs::write(&proof_path, proof_bytes).map_err(|error| {
                format!("The uploaded release proof could not be saved for verification: {error}.")
            })?;
        }
            let (tag, target) = setup_identity_of(&filename).ok_or_else(|| "The file name does not carry its setup tag and target (bilibi-setup-vX.Y.Z-<target>.tar.gz). Upload it under the name it was published with. Nothing was unpacked.".to_owned())?;
            policy.verify(
                &paths,
                &archive,
                proof.as_ref().map(|_| proof_path.as_path()),
                &tag,
                &target,
                token.as_deref(),
            )?;
            let archive_sha256 = transport::sha256_of(&archive)
                .map_err(|error| format!("The uploaded release could not be hashed: {error}."))?;
            release_proof::check_replay(&paths, &tag)?;
            let admission = policy.admission(&tag, &target, &archive_sha256);
            release_proof::Policy::load(&paths)?.check_admission(&admission)?;
            let unpacked = transport::unpack_bundle(bytes.as_slice(), &into)?;
            check_folder(&into)?;
            let recipe: Value = fs::read_to_string(into.join("release.json"))
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .ok_or_else(|| "The release has no readable release.json. Nothing was installed.".to_owned())?;
            if recipe["setup"]["repository"].as_str() != Some(policy.repository.as_str())
                || recipe["setup"]["tag"].as_str() != Some(tag.as_str())
            {
                return Err("The release does not identify the trusted application repository and the setup tag in its file name. Nothing was installed.".into());
            }
            fs::write(
                into.join(transport::ADMISSION_FILE),
                serde_json::to_vec(&admission).map_err(|_| "The release admission record could not be made.".to_owned())?,
            )
            .map_err(|error| format!("The release admission record could not be saved: {error}."))?;
            let mut facts = transport::folder_facts(&into);
            facts["verified"] = json!(true);
            facts["uploaded_as"] = json!(filename);
            facts["program_bytes"] = json!(unpacked.bytes);
            facts["files"] = json!(unpacked.files);
            staged.keep_folder = true;
            Ok::<Upload, String>(Upload { bundle, facts })
        })
        .await
        .map_err(|_| Error::Internal)?;
        Ok(unpacked)
    }
    /// Removes an uploaded release file that will not be installed.
    pub async fn discard_upload(
        &self,
        scope: &Scope,
        bundle: &str,
    ) -> Result<std::result::Result<(), String>> {
        let client = self.db.get().await?;
        authorize_operator(&**client, scope, MANAGE).await?;
        let paths = match self.paths() {
            Ok(paths) => paths,
            Err(why) => return Ok(Err(why)),
        };
        if !bundle.starts_with(transport::UPLOAD_PREFIX) || !transport::valid_incoming_name(bundle)
        {
            return Ok(Err(format!("{bundle} is not an uploaded release file.")));
        }
        let dir = paths.incoming_folder(bundle);
        if !dir.is_dir() {
            return Ok(Err(format!(
                "There is no uploaded release file called {bundle} any more."
            )));
        }
        Ok(fs::remove_dir_all(&dir)
            .map_err(|error| format!("The file could not be removed: {error}.")))
    }

    // ------------------------------------------------------ the download loop
    /// Runs beside the server for as long as it serves: downloads the
    /// release of each install job that waits for one, then wakes the
    /// installer.
    pub async fn run_downloads(
        self: Arc<Self>,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        loop {
            if *stop.borrow() {
                return Ok(());
            }
            if let Err(error) = self.transfer_once().await {
                eprintln!("warning: release download round failed: {error:?}");
            }
            tokio::select! {
                _ = stop.changed() => {}
                _ = tokio::time::sleep(TRANSFER_EVERY) => {}
            }
        }
    }
    async fn transfer_once(&self) -> Result<()> {
        let client = self.db.get().await?;
        let waiting = jobs::awaiting_download(&**client).await?;
        drop(client);
        for (tenant, job) in waiting {
            let worker = self.download_worker();
            match self.download_github(tenant, &job).await {
                Ok(true) => {}
                Ok(false) => eprintln!(
                    "software job {}: download stopped because the job was cancelled",
                    job.id
                ),
                Err(why) => {
                    let client = self.db.get().await?;
                    let message = format!("Nothing changed. {}", why.trim_end_matches('.'));
                    jobs::download_step(
                        &**client,
                        tenant,
                        job.id,
                        "failed",
                        &json!({"message": why}),
                    )
                    .await?;
                    jobs::fail_queued(&**client, tenant, job.id, &worker, &format!("{message}."))
                        .await?;
                    eprintln!("software job {}: download failed: {why}", job.id);
                }
            }
        }
        Ok(())
    }
    /// One GitHub job: re-read the reviewed file, stream it, check its
    /// archive code before unpacking, then the files inside. `Ok(false)`
    /// means the job was cancelled. The signed download address is never
    /// stored.
    async fn download_github(
        &self,
        tenant: i64,
        job: &jobs::Job,
    ) -> std::result::Result<bool, String> {
        let pin = crate::github::Pin::from_request(&job.request["source"])?;
        let paths = self.paths()?.clone();
        let policy = release_proof::Policy::load(&paths)?;
        policy.check_admission(&job.request["source"])?;
        release_proof::check_replay(&paths, &pin.tag)?;
        if policy.requires_operator_proof()
            && (pin.proof_asset_id.is_none() || pin.proof_digest.is_none())
        {
            return Err("The release request has no operator proof asset. Publish it beside the archive and ask again.".into());
        }
        let mut client = self.db.get().await.map_err(db)?;
        let scope = self
            .authentication
            .authenticate(
                &**client,
                tenant,
                job.requested_by,
                format!("software-download-{tenant}-{}", job.requested_by),
            )
            .await
            .map_err(|_| "The account that asked for this install is no longer active, so the download was not made in its name. Ask again.".to_owned())?;
        let folder = format!("{}{}", transport::JOB_PREFIX, job.id);
        let into = paths.incoming_folder(&folder);
        let part = paths.incoming().join(format!("{folder}.tar.gz.part"));
        let proof_part = paths.incoming().join(format!("{folder}.proof.json.part"));
        let mut staged = StagedRelease::new(&part, &proof_part, &into);
        let _ = fs::remove_dir_all(&into);
        let _ = fs::remove_file(&part);
        let _ = fs::remove_file(&proof_part);
        if fs::create_dir_all(paths.incoming()).is_err() || !paths.incoming().is_dir() {
            return Err(format!(
                "This instance has no incoming folder ({}). Run scripts/install-server.sh create {} again on the server to add it, then ask again.",
                paths.incoming().display(),
                paths.name
            ));
        }
        let progress = |message: String, extra: Value| {
            let mut detail = extra;
            detail["message"] = json!(message);
            detail
        };
        jobs::download_step(
            &**client,
            tenant,
            job.id,
            "running",
            &progress(
                format!("Checking GitHub release {} before download", pin.tag),
                json!({}),
            ),
        )
        .await
        .map_err(db)?;
        // One fresh token for the recheck, download and provenance check.
        let token = self.github_token(&scope, &pin.repository).await?;
        crate::github::recheck(self, &scope, &pin, token.as_deref()).await?;
        let size = pin.bytes;
        if let Some(free) = transport::free_bytes(&paths.incoming()) {
            let need = size.saturating_mul(2) + DISK_MARGIN;
            if size > 0 && free < need {
                return Err(format!(
                    "Not enough disk to receive the release: {} free, about {} needed. Free some space on the server first.",
                    transport::human(free),
                    transport::human(need)
                ));
            }
        }
        jobs::download_step(
            &**client,
            tenant,
            job.id,
            "running",
            &progress(
                format!(
                    "Downloading {} from GitHub ({})",
                    pin.name,
                    transport::human(size)
                ),
                json!({"bytes_total": size, "bytes": 0}),
            ),
        )
        .await
        .map_err(db)?;
        let file = tokio::fs::File::create(&part)
            .await
            .map_err(|error| format!("could not write {}: {error}", part.display()))?;
        let counted = Arc::new(AtomicU64::new(0));
        let mut sink = Counting {
            inner: file,
            bytes: counted.clone(),
        };
        let (cancelled_tx, mut cancelled) = tokio::sync::oneshot::channel::<()>();
        let ticker = {
            let db = self.db.clone();
            let counted = counted.clone();
            let name = pin.name.clone();
            let job_id = job.id;
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let got = counted.load(Ordering::Relaxed);
                    if let Ok(client) = db.get().await {
                        let queued = client
                            .query_opt(
                                "SELECT state = 'queued' FROM base_software_jobs WHERE tenant_id=$1 AND id=$2",
                                &[&tenant, &job_id],
                            )
                            .await
                            .map(|row| row.is_some_and(|row| row.get::<_, bool>(0)));
                        if let Ok(false) = queued {
                            let _ = cancelled_tx.send(());
                            return;
                        }
                        let _ = jobs::download_step(&**client, tenant, job_id, "running", &json!({
                            "message": format!("Downloading {name} from GitHub: {} of {}", transport::human(got), transport::human(size)),
                            "bytes_total": size, "bytes": got,
                        }))
                        .await;
                    }
                }
            })
        };
        let request = crate::github::asset_request(&pin.repository, pin.asset_id, token.as_deref())
            .timeout(DOWNLOAD_TIMEOUT)
            .max_body_bytes(transport::MAX_BUNDLE_BYTES);
        let call = Call {
            system: "github".into(),
            operation: "releases.download".into(),
            ..Call::default()
        };
        let download = self
            .integration
            .stream(Origin::User(&scope), &call, request, &mut sink);
        let streamed = tokio::select! {
            streamed = download => Some(streamed),
            _ = &mut cancelled => None,
        };
        ticker.abort();
        drop(sink);
        let Some(streamed) = streamed else {
            return Ok(false);
        };
        let streamed = match streamed {
            Ok(streamed) if streamed.status == 200 => streamed,
            Ok(streamed) => {
                let header = |name: &str| {
                    streamed
                        .headers
                        .get(name)
                        .and_then(|value| value.to_str().ok())
                };
                return Err(crate::github::download_refusal(
                    streamed.status,
                    [
                        header("retry-after"),
                        header("x-ratelimit-remaining"),
                        header("x-ratelimit-reset"),
                    ],
                    token.is_some(),
                ));
            }
            Err(failure) => {
                return Err(github_transport_sentence(&failure));
            }
        };
        if let (Some(proof_id), Some(proof_digest)) =
            (pin.proof_asset_id, pin.proof_digest.as_deref())
        {
            self.download_proof_asset(
                &scope,
                &pin.repository,
                proof_id,
                proof_digest,
                token.as_deref(),
                &proof_part,
            )
            .await?;
        }
        jobs::download_step(
            &**client,
            tenant,
            job.id,
            "running",
            &progress(
                format!(
                    "Checking {} ({} received)",
                    pin.name,
                    transport::human(streamed.bytes)
                ),
                json!({"bytes_total": size, "bytes": streamed.bytes}),
            ),
        )
        .await
        .map_err(db)?;
        let part_for_check = part.clone();
        let into_for_unpack = into.clone();
        let expected = pin.digest.clone();
        let file_name = pin.name.clone();
        let verify_tag = pin.tag.clone();
        let verify_target = pin.target.clone();
        let verify_token = token.clone();
        let verify_policy = policy.clone();
        let verify_paths = paths.clone();
        let verify_proof = pin.proof_asset_id.map(|_| proof_part.clone());
        let checked = tokio::task::spawn_blocking(move || {
            let got = crate::github::digest_of(&part_for_check)
                .map_err(|error| format!("The downloaded file could not be checked: {error}"))?;
            if got != expected {
                return Err(format!(
                    "The downloaded file {file_name} does not match the check code that was reviewed. Nothing was unpacked."
                ));
            }
            verify_policy.verify(
                &verify_paths,
                &part_for_check,
                verify_proof.as_deref(),
                &verify_tag,
                &verify_target,
                verify_token.as_deref(),
            )?;
            let lock = instance_lock(&verify_paths)?;
        lock.try_lock_shared().map_err(|error| format!("Could not lock the instance to check the download: {error}."))?;
            release_proof::check_replay(&verify_paths, &verify_tag)?;
            let admission = verify_policy.admission(&verify_tag, &verify_target, &expected);
            release_proof::Policy::load(&verify_paths)?.check_admission(&admission)?;
            let file = fs::File::open(&part_for_check)
                .map_err(|error| format!("could not read the downloaded file: {error}"))?;
            let unpacked = transport::unpack_bundle(file, &into_for_unpack)?;
            let sha = check_folder(&into_for_unpack)?;
            fs::write(
                into_for_unpack.join(transport::ADMISSION_FILE),
                serde_json::to_vec(&admission).map_err(|_| "The release admission record could not be made.".to_owned())?,
            )
            .map_err(|error| format!("The release admission record could not be saved: {error}."))?;
            Ok::<(String, u64), String>((sha, unpacked.bytes))
        })
        .await
        .map_err(|_| "the check did not finish".to_owned());
        let (program_sha, program_bytes) = checked??;
        let detail = json!({
            "message": format!("Downloaded {} from GitHub repository {} (tag {}) and checked it", pin.name, pin.repository, pin.tag),
            "path": into.display().to_string(),
            "folder": folder,
            "digest": pin.digest,
            "archive_sha256": pin.digest,
            "sha256": program_sha,
            "bytes": program_bytes,
            "bytes_total": size,
            "repository": pin.repository,
            "repository_id": pin.repository_id,
            "release_id": pin.release_id,
            "asset_id": pin.asset_id,
            "tag": pin.tag,
            "target": pin.target,
        });
        let handed = jobs::download_done(&mut client, tenant, job.id, &detail)
            .await
            .map_err(db)?;
        staged.keep_folder = handed;
        if handed {
            paths.wake_installer(job.id)?;
        }
        Ok(handed)
    }

    async fn download_proof_asset(
        &self,
        scope: &Scope,
        repository: &str,
        asset_id: i64,
        expected_digest: &str,
        token: Option<&str>,
        path: &Path,
    ) -> std::result::Result<(), String> {
        let file = tokio::fs::File::create(path)
            .await
            .map_err(|error| format!("could not save the operator proof: {error}"))?;
        let mut sink = file;
        let request = crate::github::asset_request(repository, asset_id, token)
            .timeout(DOWNLOAD_TIMEOUT)
            .max_body_bytes(64 * 1024);
        let call = Call {
            system: "github".into(),
            operation: "releases.proof_download".into(),
            ..Call::default()
        };
        let reply = self
            .integration
            .stream(Origin::User(scope), &call, request, &mut sink)
            .await
            .map_err(|failure| github_transport_sentence(&failure))?;
        drop(sink);
        if reply.status != 200 {
            return Err(format!(
                "GitHub refused the operator proof download with status {}.",
                reply.status
            ));
        }
        let got = crate::github::digest_of(path).map_err(|error| {
            format!("The downloaded operator proof could not be checked: {error}")
        })?;
        if got != expected_digest {
            return Err(
                "The downloaded operator proof does not match the check code that was reviewed."
                    .into(),
            );
        }
        Ok(())
    }
}

/// A database failure as the sentence a job fails with.
fn db<E: Into<Error>>(error: E) -> String {
    format!(
        "The database could not be reached ({:?}). Ask again.",
        error.into()
    )
}
/// The checks every way in shares, on an unpacked folder: the checksum and
/// that the program was built for this server. Returns the checksum.
/// `setup-v1.2.3` from a published name like `bilibi-setup-v1.2.3-<target>.tar.gz`.
fn setup_identity_of(filename: &str) -> Option<(String, String)> {
    let rest = filename.strip_prefix("bilibi-")?;
    let version = rest.strip_prefix("setup-v")?;
    let end = version
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(version.len());
    let numbers: Vec<&str> = version[..end].split('.').collect();
    if numbers.len() != 3
        || numbers.iter().any(|number| {
            number.is_empty()
                || (number.len() > 1 && number.starts_with('0'))
                || !number.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return None;
    }
    let target = version[end..].strip_prefix('-')?.strip_suffix(".tar.gz")?;
    (!target.is_empty()
        && target.len() <= 128
        && target
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')))
    .then(|| (format!("setup-v{}", &version[..end]), target.to_owned()))
}

struct StagedRelease<'a> {
    archive: &'a Path,
    proof: &'a Path,
    folder: &'a Path,
    keep_folder: bool,
}

impl<'a> StagedRelease<'a> {
    fn new(archive: &'a Path, proof: &'a Path, folder: &'a Path) -> Self {
        Self {
            archive,
            proof,
            folder,
            keep_folder: false,
        }
    }
}

impl Drop for StagedRelease<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.archive);
        let _ = fs::remove_file(self.proof);
        if !self.keep_folder {
            let _ = fs::remove_dir_all(self.folder);
        }
    }
}

fn instance_lock(paths: &Paths) -> std::result::Result<fs::File, String> {
    fs::File::open(&paths.dir).map_err(|error| {
        format!("Could not open the instance folder for the release lock: {error}.")
    })
}

fn check_folder(dir: &Path) -> std::result::Result<String, String> {
    let sha = transport::verify_checksum(dir)?;
    transport::built_for_this_server(dir)?;
    Ok(sha)
}
fn github_transport_sentence(failure: &Failure) -> String {
    match failure {
        Failure::Transport { class, .. } => match *class {
            "timeout" => "GitHub did not finish sending the file. Ask again.".into(),
            "connect" => "GitHub could not be reached. Ask again.".into(),
            "redirect" => {
                "GitHub sent the file to an address this server does not call. Nothing was saved."
                    .into()
            }
            "body_limit" => "The GitHub file is larger than this server will download.".into(),
            "sink" => {
                "The file could not be written to disk here. Check the disk, then ask again.".into()
            }
            other => format!("GitHub could not be read ({other}). Ask again."),
        },
        Failure::Invalid => "The GitHub file address could not be called.".into(),
        _ => "GitHub may have answered, but the exchange could not be recorded. Ask again.".into(),
    }
}
/// A sink that counts what it has written, so progress can be shown.
struct Counting<W> {
    inner: W,
    bytes: Arc<AtomicU64>,
}
impl<W: AsyncWrite + Unpin> AsyncWrite for Counting<W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let polled = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(n)) = &polled {
            self.bytes.fetch_add(*n as u64, Ordering::Relaxed);
        }
        polled
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
