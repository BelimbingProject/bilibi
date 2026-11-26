//! Shared Software reads and reviewed installs.
use crate::{bound, SoftwareError, CARD_URI, MANAGE, STEP_UP, VIEW};
use bilibi_agent_api::{
    App, Channels, Definitions, Error as AgentError, Guide, Handler, Operation, OperationClient,
    OperationFuture, ReadHandler, WriteHandler, APPS, DEFINITIONS,
};
use bilibi_database::MutationTransaction;
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::collections::BTreeMap;

impl From<SoftwareError> for AgentError {
    fn from(error: SoftwareError) -> Self {
        match error {
            SoftwareError::Field { field, message } => {
                AgentError::InvalidInput(BTreeMap::from([(field, vec![message])]))
            }
            SoftwareError::Backend(error) => AgentError::Backend(error),
        }
    }
}

enum Read {
    Status,
    Catalogue,
    Releases,
    Open,
}
impl ReadHandler for Read {
    fn owner(&self) -> &'static str {
        "base/software"
    }
    fn call<'a>(
        &'a self,
        _client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let software = bound()?;
            let value = match self {
                Self::Status => software.status(scope).await?,
                Self::Catalogue => software.catalogue(scope).await?,
                Self::Releases => {
                    software
                        .releases_with(
                            scope,
                            input["refresh"].as_bool() == Some(true),
                            input["prerelease"].as_bool() == Some(true),
                        )
                        .await?
                }
                Self::Open => software.open(scope, input).await?,
            };
            Ok(value.into())
        })
    }
}

enum Write {
    Install,
    Undo,
    Go,
    Cancel,
    Github,
    Trust,
}
impl WriteHandler for Write {
    fn owner(&self) -> &'static str {
        "base/software"
    }
    fn preview<'a>(
        &'a self,
        client: &'a dyn OperationClient,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let software = bound()?;
            let sql = client.sql_client().await?;
            let mut review = match self {
                Self::Install => software.review_install(sql, scope, input).await?,
                Self::Undo => software.review_undo(sql, scope, input).await?,
                Self::Go => software.review_go(sql, scope, input).await?,
                Self::Cancel => software.review_cancel(sql, scope, input).await?,
                Self::Github => software.review_github(sql, scope, input).await?,
                Self::Trust => software.review_trust(sql, scope, input).await?,
            };
            if !matches!(self, Self::Cancel) {
                let methods = software.step_up.enrolled_methods(scope).await?;
                review.facts["two_factor_enabled"] = json!(!methods.is_empty());
                review.facts["two_factor_methods"] = methods
                    .iter()
                    .map(|m| json!({"id": m.id, "label": m.label, "length": m.code_length, "numeric": m.numeric}))
                    .collect();
                review.facts["two_factor_setup_url"] = json!(format!(
                    "{}{}",
                    software.origin,
                    bilibi_ui::StepUp::new(false, "/system/software?tab=releases").setup_href()
                ));
            }
            Ok(review.facts.into())
        })
    }
    fn commit<'a>(
        &'a self,
        tx: &'a mut MutationTransaction<'_>,
        scope: &'a Scope,
        input: &'a Value,
    ) -> OperationFuture<'a> {
        Box::pin(async move {
            let software = bound()?;
            let value = match self {
                Self::Install => software.install_in(tx, scope, input).await?,
                Self::Undo => software.undo_in(tx, scope, input).await?,
                Self::Go => software.go_in(tx, scope, input).await?,
                Self::Cancel => software.cancel_in(tx, scope, input).await?,
                Self::Github => software.github_in(tx, scope, input).await?,
                Self::Trust => software.trust_in(tx, scope, input).await?,
            };
            Ok(value.into())
        })
    }
}

fn empty_schema() -> Value {
    json!({"type":"object","properties":{}})
}
fn install_schema() -> Value {
    json!({"type":"object","properties":{
        "bundle":{"type":"string","maxLength":32,"description":"The name of a release file uploaded here, like upload-3 (base.software.releases lists them)."},
        "asset":{"type":"integer","minimum":1,"description":"The GitHub file number from base.software.releases."},
        "repository_id":{"type":"integer","minimum":1,"description":"The GitHub repository number listed with that file."},
        "release_id":{"type":"integer","minimum":1,"description":"The GitHub release number listed with that file."},
        "tag":{"type":"string","maxLength":200,"description":"The setup tag listed with that file."},
        "target":{"type":"string","maxLength":80,"description":"The computer kind listed with that file, such as x86_64-unknown-linux-gnu."},
        "digest":{"type":"string","maxLength":80,"description":"The file's check code from base.software.releases, 64 hex characters."},
        "staging":{"type":"object","required":["instance","asset","job","checked_at","outcome","checks"],"description":"Agent-reported Staging checks, retained in the Production review. Failed reports refuse promotion. Asset differences are shown for the owner to review. Not a signed server receipt.","properties":{
            "instance":{"type":"object","required":["origin","label","role"],"properties":{"origin":{"type":"string","maxLength":2048},"label":{"type":"string","maxLength":200},"role":{"type":"string","enum":["staging"]}}},
            "asset":{"type":"object","description":"The installed_asset.github identity from Staging status. Omit setup_commit when it is null.","required":["repository_id","release_id","asset_id","tag","target","archive_sha256"],"properties":{
                "repository":{"type":"string","maxLength":140},"repository_id":{"type":"integer","minimum":1},"release_id":{"type":"integer","minimum":1},"asset_id":{"type":"integer","minimum":1},
                "tag":{"type":"string","maxLength":200},"target":{"type":"string","maxLength":80},"archive_sha256":{"type":"string","maxLength":64},"program_sha256":{"type":"string","maxLength":64},"setup_commit":{"type":"string","maxLength":64}
            }},
            "job":{"type":"integer","minimum":1},"checked_at":{"type":"string","maxLength":40,"description":"UTC RFC 3339 time."},
            "outcome":{"type":"string","enum":["passed","failed"]},
            "checks":{"type":"array","items":{"type":"object","required":["name","outcome"],"properties":{"name":{"type":"string","maxLength":500},"outcome":{"type":"string","enum":["passed","failed"]}}}}
        }},
        "switch":{"type":"string","enum":["auto","ask"]}
    }})
}
fn github_schema() -> Value {
    json!({"type":"object","properties":{
        "refresh_minutes":{"type":"integer","minimum":1,"maximum":1440,"description":"How many minutes to wait between GitHub checks. 15 checks about four times an hour."}
    }})
}
fn trust_schema() -> Value {
    json!({"type":"object","required":["kind","repository","repository_id","workflow","minimum_tag"],"properties":{
        "kind":{"type":"string","enum":["github","operator"],"description":"The one proof method this installation accepts."},
        "repository":{"type":"string","maxLength":140,"description":"The operator's repository as owner/name."},
        "repository_id":{"type":"integer","minimum":1,"description":"The repository's immutable number from a trusted record."},
        "workflow":{"type":"string","maxLength":255,"description":"The exact trusted workflow identity."},
        "minimum_tag":{"type":"string","maxLength":80,"description":"The oldest tag this installation accepts: setup-vX.Y.Z or nightly-YYYY-MM-DD. It names the one channel (stable setup or nightly) the installation accepts; a tag from the other channel is refused."},
        "public_key":{"type":"string","maxLength":16384,"description":"Required for operator: the Ed25519 public key in PEM form. Omit for github."}
    }})
}
fn releases_schema() -> Value {
    json!({"type":"object","properties":{
        "refresh":{"type":"boolean","description":"true asks GitHub now, unless GitHub is limiting this server. Otherwise the saved list is used when it is still fresh."},
        "prerelease":{"type":"boolean","description":"true includes files GitHub marks as a pre-release. Drafts are never listed."}
    }})
}
fn releases_output() -> Value {
    json!({"type":"object","description":"update (state: ready, current, unavailable or source_checkout; current version, checked_at; ready includes release notes and the exact install asset); read-only release_trust; instance (origin, role, label); installed_asset (local_release, program_sha256, github with repository_id, release_id, asset_id, tag, target, archive_sha256, program_sha256, setup_commit); uploads; github (repository, repository_id, accepted, target, releases with release_id, asset_id, tag, digest, target, installed; or message)."})
}
fn job_schema() -> Value {
    json!({"type":"object","properties":{
        "job":{"type":"integer","minimum":1,"description":"The job number from base.software.status. Left out: the one that applies."}
    }})
}
fn open_schema() -> Value {
    json!({"type":"object","properties":{
        "propose":{"type":"object","description":"Domains or Extensions to highlight in the catalogue, by id. The catalogue supplies installation commands; choosing entries does not start an install.","properties":{
            "domains":{"type":"array","items":{"type":"string","maxLength":64}},
            "extensions":{"type":"array","items":{"type":"string","maxLength":64}}}}
    }})
}
fn open_output() -> Value {
    json!({"type":"object","description":"Everything base.software.status answers, plus catalogue (with uncatalogued installed Domains and Extensions) on every instance, proposal (found and unknown ids), next_step (build locally, then install here), and card.resource."})
}
fn status_output() -> Value {
    json!({"type":"object","description":"instance (origin, role, label), release_trust (the read-only pinned proof policy, repository, workflow, minimum tag, highest admitted tag, change time and actor, and operator-key fingerprint when applicable), installed_asset (local_release, program_sha256, github exact identity as in releases), installed (platform, domains, extensions from the running program, including extension_code when recorded), plugins, uploads (release files waiting here: bundle, release, from), job (the one waiting or running, or the last one, with its steps and whether cancel is available), undo (available, job, restores_backup, backup_at), jobs (recent), backups (at and bytes for recent jobs; file only for a server owner), backup_rule (how many backups are kept and what Undo restores)."})
}
fn catalogue_output() -> Value {
    json!({"type":"object","description":"available, url, running_platform, platform versions, domains and extensions with descriptions, repositories, versions, fits_running, tested_with_running, recommended_version, compatibility, official, status (installed, not_installed, update_available), latest_version, warning, local_steps and next_step; plugins and tested sets. uncatalogued contains installed domains and extensions absent from the selected catalogue, with id, version (null when unknown), commit, modules and extension_code (reviewed/customer when recorded). When not available: message and uncatalogued installed modules still returned."})
}
fn review_output() -> Value {
    json!({"type":"object","description":"The review: summary, the change list, warnings, the estimate, what happens, the steps, and the step-up operation key (base.software.change). The commit answers the queued job with its id, state and steps."})
}

inventory::submit! { Contribution::new(DEFINITIONS, "base/software", Provenance { module: "base/software", source: file!(), line: line!() }, &Definitions {
    operations: &[
        Operation { key:"base.software.status", title:"Software status", summary:"See what this server runs, follow an install or undo, and read backup times, sizes and the retention rule; owners also see the files.", keywords:&["software","status","build","install","domain","version","job","undo"], capability:VIEW, input:empty_schema, output:status_output, handler:Handler::Read(&Read::Status), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.software.open", title:"Manage domains and extensions", summary:"Open the Software card to see installed modules, find more modules, follow updates and backups, or undo an install. Includes installation commands for the requested modules.", keywords:&["software","card","open","propose","domain","build"], capability:VIEW, input:open_schema, output:open_output, handler:Handler::Read(&Read::Open), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.software.catalogue", title:"Available domains and extensions", summary:"Find business modules (Domains) and adaptations or connections (Extensions). See which versions support this Bilibi and whether checks covered the exact code running here. Includes installation commands to build and test on a computer.", keywords:&["software","catalogue","domain","version","release","tested"], capability:VIEW, input:empty_schema, output:catalogue_output, handler:Handler::Read(&Read::Catalogue), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.software.releases", title:"Releases available to install", summary:"List exact GitHub assets, what is installed, and uploaded files waiting to install.", keywords:&["software","release","install","source","download","upload","github"], capability:VIEW, input:releases_schema, output:releases_output, handler:Handler::Read(&Read::Releases), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.software.install", title:"Install a tested release", summary:"Install a GitHub setup file (asset) or an uploaded file (bundle). Include the Staging report when promoting the same asset. Review the changes first. Confirmation requires the server owner's authenticator code.", keywords:&["software","install","release","staging","production","upload","github"], capability:MANAGE, input:install_schema, output:review_output, handler:Handler::SafeWrite(&Write::Install), channels:Channels::Every, step_up:&[STEP_UP] },
        Operation { key:"base.software.undo", title:"Undo the last software change", summary:"Return to the previous program. Review which backup Undo restores and what data would be lost. Confirmation requires the server owner's authenticator code.", keywords:&["software","undo","rollback","previous","restore"], capability:MANAGE, input:job_schema, output:review_output, handler:Handler::SafeWrite(&Write::Undo), channels:Channels::Every, step_up:&[STEP_UP] },
        Operation { key:"base.software.go", title:"Confirm installation", summary:"Let a copied install go ahead when it was started with switch ask: the server then pauses for about a minute to back up, update, switch and check. Confirmation requires the server owner's authenticator code.", keywords:&["software","go","ahead","switch","pause","install","ask"], capability:MANAGE, input:job_schema, output:review_output, handler:Handler::SafeWrite(&Write::Go), channels:Channels::Every, step_up:&[STEP_UP] },
        Operation { key:"base.software.cancel", title:"Cancel the software job", summary:"Cancel a queued job or ask a running install or undo to stop before it pauses. A claimed trust replacement cannot be cancelled.", keywords:&["software","cancel","stop","job"], capability:MANAGE, input:job_schema, output:review_output, handler:Handler::SafeWrite(&Write::Cancel), channels:Channels::Every, step_up:&[] },
        Operation { key:"base.software.github", title:"GitHub release access", summary:"Connect the GitHub App for read access to the repository already pinned in Release trust and choose how often to check. Confirmation requires the server owner's authenticator code.", keywords:&["software","github","release","repository","app"], capability:MANAGE, input:github_schema, output:review_output, handler:Handler::SafeWrite(&Write::Github), channels:Channels::Every, step_up:&[STEP_UP] },
        Operation { key:"base.software.trust", title:"Release trust", summary:"Replace the one proof policy pinned for this installation. Review the repository, workflow, minimum tag and operator-key fingerprint. Confirmation requires the installation owner's fresh authenticator code.", keywords:&["software","release","trust","signature","attestation","key"], capability:MANAGE, input:trust_schema, output:review_output, handler:Handler::SafeWrite(&Write::Trust), channels:Channels::Every, step_up:&[STEP_UP] },
    ],
    guides: &[Guide { key:"base.software", title:"Discover and install software", summary:"Find modules, prepare tested updates on your computer, install them with backups, undo or cancel.", keywords:&["software","domain","extension","build","install","catalogue","undo"], capability:VIEW,
        body:"Start with base.software.status to see what this server runs and the read-only Release trust facts. Read base.software.catalogue or open the Software card with propose.domains or propose.extensions for what the person asked for. Each entry says what it does, which version supports this Bilibi, whether checks covered the exact code running here, and local_steps. Add the modules in a Bilibi code folder, run local_steps together, run the composition locally and check it in the browser. Servers never build software. Publish the saved composition through any CI that produces a complete setup archive and the proof this server was set up to trust: GitHub artifact attestation or an operator-key detached proof. The proof binds the archive to the pinned repository, workflow, tag and target. For installation read base.software.releases, then plan base.software.install with bundle (an archive uploaded here, with its proof when Release trust uses an operator key) or asset (a setup archive and proof from the pinned GitHub repository, with repository_id, release_id, tag, target and digest from that same list). Install and undo back up, switch, check and switch back automatically on failure through the installer. Install, undo, go, GitHub access and Release trust changes require the owner's current authenticator code every time. Only the installation owner may replace Release trust; use base.software.trust and show the complete current and replacement policy, including the operator-key fingerprint, before confirmation. Follow jobs and recorded backups with status. Show each backup's time and size, the file when it is given, and the backup_rule sentence once. An install with switch ask waits after copying: plan base.software.go to proceed. Cancel is available until the pause starts. Only a server owner can change software, because an install affects every company on this server. For test on Staging then Production, use two separately authenticated connections. Identify each by origin and role, never label alone. Install the exact repository_id, release_id, asset_id, tag, target and archive digest on Staging with its own reviewed plan and fresh 2FA. Wait for completion; compare installed_asset.github and run relevant business workflows. Retain the Staging instance, job, asset identity (including proof, program_sha256 and setup_commit when recorded; omit a null setup_commit from the report), UTC checked_at, outcome and named checks. Keep failed results and stop promotion on failure. Re-read Production status and releases immediately before making a separate Production plan for that exact asset; include the report as staging so the owner sees the checks, backup and pause wording. Obtain separate consent and a fresh code; never precommit Production in a long Staging plan. After completion compare the installed identity again. The review says whether this is the same file, the same setup tag and commit built for a different computer type, or a different setup; an unavailable setup commit is reported as unverified. These comparisons inform the owner and never refuse an install. A tag or local release number alone proves nothing. Staging reports are agent supplied, not server verified receipts. Manual installs do not require a Staging report. Never give an agent the authenticator seed, signing key or GitHub token." }],
}) }

/// How an agent works with the card, for the plugin package's skill.
const CARD_SKILL: &str = "# Software discovery and installation\n\nCall open_software to find business features (Domains) and adaptations or connections (Extensions), inspect this instance, read its pinned Release trust or install a tested release. Pass propose.domains or propose.extensions with requested catalogue ids; they highlight entries, never request a server build. Follow each entry's local_steps in a local checkout, run the result and check it in the browser. Servers never build software. Publish a complete setup archive through any CI, with either GitHub artifact attestation or the operator-key detached proof selected during server setup. Bilibi checks that proof against the pinned repository, workflow, tag and target before unpacking an archive from GitHub or an upload. For a tested release, the person reviews and confirms install or undo in the card with their authenticator code. Do not plan or commit those writes yourself while the card is open. Follow the job and explain its outcome. If the host cannot show cards, read base.software.catalogue and status, then plan install or undo and show the full review before confirmation. Every install, undo and go-ahead asks for a fresh code on every instance. Only a server owner may install software because it affects every company. Only the installation owner may replace Release trust, through a separately reviewed base.software.trust operation with fresh 2FA; always show the current and replacement policy and operator-key fingerprint. For test on Staging then Production, use two separately authenticated connections. Identify each by origin and role, never label alone. Install the exact repository_id, release_id, asset_id, tag, target and archive digest on Staging with its own reviewed plan and fresh 2FA. Wait for completion; compare installed_asset.github and run relevant business workflows. Retain the Staging instance, job, proof and asset identity, UTC checked_at, outcome and named checks. Keep failed results and stop promotion on failure. Re-read Production status and releases immediately before making a separate Production plan for that exact asset; include the report as staging, obtain separate consent and a fresh code, and compare the installed identity after completion. A tag or local release number alone proves nothing. Staging reports are agent supplied, not server verified receipts. Manual installs do not require a Staging report. Never give an agent the authenticator seed, signing key or GitHub token.\n";

inventory::submit! { Contribution::new(APPS, "base/software", Provenance { module: "base/software", source: file!(), line: line!() }, &App {
    uri: CARD_URI,
    tool: "open_software",
    title: "Manage domains and extensions",
    description: "Open Software: find business features (Domains) and adaptations or connections (Extensions), compatible versions, records of checks on the exact code and local build steps; see what this server runs; install a tested release or undo with the owner's code. Pass propose.domains or propose.extensions to highlight requested entries. Servers never build software.",
    source: bilibi_agent_api::AppSource::Operation("base.software.open"),
    html: include_str!("../card/software-v1.html"),
    connect_domains: &[],
    resource_domains: &[],
    skill: CARD_SKILL,
}) }
