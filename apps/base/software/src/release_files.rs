//! Release controls embedded in the Software page’s Releases tab and the
//! manual upload.
use crate::{web::PAGES, MANAGE, VIEW};
use askama::Template;
use bilibi_agent_api::Error as AgentError;
use bilibi_authz::authorize_operator;
use bilibi_database::{Error, Result};
use bilibi_system::Instance;
use bilibi_tenancy::Scope;
use bilibi_ui::{
    forms::{Field, InputKind},
    page::{LiveInput, LiveRequest, Uploads},
    ActionLink, BuiltinColumn, Button, ButtonVariant, Card, ColumnsOutcome, Fact, Facts, FlexRow,
    FlexTable, Fold, Form, Html, ListColumns, SectionHeading, StatusKind, TableEmpty, UploadSpec,
};
use serde_json::{json, Value};

#[derive(Template)]
#[template(
    source = r#"{% import "components.html" as ui %}<div data-hook="ClipboardCopy">{{ label }} {% call ui::copy_button(id=id, text=prompt, label=label) %}{% endcall %}</div>"#,
    ext = "html"
)]
pub(super) struct UpdatePrompt<'a> {
    pub id: &'a str,
    pub label: &'a str,
    pub prompt: &'a str,
}

pub const PATH: &str = "/system/software/releases";
/// The file input's name: the key the owner consumes by.
const UPLOAD: &str = "release_file";
/// An operator-signed release's detached DSSE proof.
const PROOF: &str = "release_proof";
/// A release file is a compressed program: tens of megabytes, so the input
/// is resumable and a dropped connection carries on where it stopped.
const UPLOAD_BYTES: u64 = bilibi_ui::upload::MAX_FILE_SIZE;

fn render(template: impl Template) -> Result<Html> {
    Html::render(&template).map_err(|_| Error::Internal)
}

/// Release-tab state: upload input, list columns, GitHub controls and feedback.
pub(super) struct ReleaseFiles {
    pages: crate::web::Pages,
    pub(super) uploads: Uploads,
    waiting: ListColumns,
    github_columns: ListColumns,
    /// Include GitHub files marked as a pre-release. Drafts stay hidden.
    github_prerelease: bool,
    /// The next render asks GitHub now, unless GitHub is limiting this server.
    github_refresh: bool,
    github_error: Option<String>,
    pending_github: Option<(&'static str, Value)>,
    pub(super) flash: Option<(StatusKind, String)>,
}
fn github_columns() -> Vec<BuiltinColumn> {
    vec![
        BuiltinColumn::new("tag", "Tag"),
        BuiltinColumn::new("target", "For"),
        BuiltinColumn::new("file", "File"),
        BuiltinColumn::new("asset", "GitHub asset"),
        BuiltinColumn::new("installed", "Installed"),
        BuiltinColumn::new("digest", "Archive SHA-256"),
        BuiltinColumn::new("published", "Published"),
    ]
}
fn waiting_columns() -> Vec<BuiltinColumn> {
    vec![
        BuiltinColumn::new("bundle", "File"),
        BuiltinColumn::new("release", "Release").right(),
        BuiltinColumn::new("from", "Made on"),
        BuiltinColumn::new("platform", "Bilibi"),
        BuiltinColumn::new("domains", "Domains"),
        BuiltinColumn::new("checksum", "File checksum"),
    ]
}
impl ReleaseFiles {
    pub(super) fn new(pages: crate::web::Pages) -> Result<Self> {
        Ok(Self {
            pages,
            uploads: Uploads::new([
                UploadSpec::resumable(UPLOAD, UPLOAD_BYTES).accept([
                    ".tar.gz",
                    ".tgz",
                    "application/gzip",
                    "application/x-gzip",
                ]),
                UploadSpec::new(PROOF, 64 * 1024).accept([".json", "application/json"]),
            ]),
            waiting: ListColumns::mount("release-files-waiting", waiting_columns())
                .map_err(|_| Error::Internal)?,
            github_columns: ListColumns::mount("release-files-github", github_columns())
                .map_err(|_| Error::Internal)?,
            github_prerelease: false,
            github_refresh: false,
            github_error: None,
            pending_github: None,
            flash: None,
        })
    }
    fn say(&mut self, kind: StatusKind, message: impl Into<String>) {
        self.flash = Some((kind, message.into()));
    }
    fn stage_github(&mut self, event: &'static str, input: Value) {
        self.github_error = None;
        self.pending_github = Some((event, input));
    }

    pub(super) fn cancel_github(&mut self) {
        self.pending_github = None;
        self.github_error = None;
    }

    pub(super) async fn github_gate(&self, scope: &Scope) -> Result<Option<Html>> {
        let Some((event, _)) = &self.pending_github else {
            return Ok(None);
        };
        let methods = self
            .pages
            .software
            .step_up
            .enrolled_methods(scope)
            .await?
            .into_iter()
            .map(|method| {
                bilibi_ui::ProofMethod::new(
                    method.id,
                    method.label,
                    method.code_length,
                    method.numeric,
                )
            })
            .collect::<Vec<_>>();
        let mut step = bilibi_ui::StepUp::new(!methods.is_empty(), "/system/software?tab=releases");
        step.code = render(bilibi_ui::ProofInput::new(
            "github-action-code",
            "code",
            methods,
        ))?;
        step.actions = render(Button {
            button_type: "submit",
            variant: ButtonVariant::Primary,
            body: Html::text("Confirm"),
            ..Button::default()
        })?;
        step.secondary_actions = render(Button {
            button_type: "button",
            event: "cancel_github",
            body: Html::text("Cancel"),
            ..Button::default()
        })?;
        let mut body = Html::EMPTY;
        if let Some(message) = &self.github_error {
            body.push(&render(bilibi_ui::Alert {
                kind: StatusKind::Error,
                body: Html::text(message),
                ..Default::default()
            })?);
        }
        body.push(&render(step)?);
        let form = render(Form {
            id: "github-action-proof",
            event,
            body,
            ..Default::default()
        })?;
        let heading = "Save GitHub settings";
        let gate = bilibi_ui::StepUpGate::action("Software", heading, form)
            .map_err(|_| Error::Internal)?;
        Ok(Some(render(gate)?))
    }

    async fn github_save(
        &mut self,
        request: &LiveRequest<'_, Scope>,
        event: &'static str,
        input: Value,
        scope: &Scope,
    ) -> Result<()> {
        let call = bilibi_agent_api::page::Call {
            scope,
            credential: request.credential,
            service: PAGES,
            event,
            contract: "base.software.github",
        };
        self.github_error = None;
        match self.pages.contracts.submit(&call, input.clone()).await {
            Ok(_) => {
                self.github_refresh = true;
                let message = "GitHub settings saved.";
                self.say(StatusKind::Success, message);
            }
            Err(AgentError::InvalidInput(fields)) => {
                let message = fields
                    .values()
                    .flatten()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| "The GitHub settings were not accepted.".into());
                self.github_error = Some(message.clone());
                self.say(StatusKind::Error, message);
            }
            Err(AgentError::Backend(Error::StepUpRequired { reason, .. })) => {
                self.pending_github = Some((event, input));
                self.github_error = Some(
                    match reason {
                        "locked" => "Too many wrong codes. Wait a while, then try again.",
                        "impersonating" => {
                            "GitHub settings cannot be changed while viewing as someone else."
                        }
                        "not_enrolled" => "Set up 2FA before changing GitHub settings.",
                        _ => "The 2FA code was refused.",
                    }
                    .into(),
                );
            }
            Err(AgentError::Backend(Error::Unauthorized)) => return Err(Error::Unauthorized),
            Err(AgentError::Forbidden) => {
                let message = "You do not have permission to change the GitHub repository.";
                self.github_error = Some(message.into());
                self.say(StatusKind::Error, message);
            }
            Err(AgentError::Backend(error)) => {
                let message = format!("The GitHub settings could not be saved ({error:?}).");
                self.github_error = Some(message.clone());
                self.say(StatusKind::Error, message);
            }
            Err(_) => {
                let message = "The GitHub settings could not be saved.";
                self.github_error = Some(message.into());
                self.say(StatusKind::Error, message);
            }
        }
        Ok(())
    }
    async fn upload(&mut self, scope: &Scope) -> Result<()> {
        let files = self.uploads.consume(UPLOAD);
        let Some(file) = files.into_iter().next() else {
            self.say(StatusKind::Info, "Choose a release file first.");
            return Ok(());
        };
        let proof = self
            .uploads
            .consume(PROOF)
            .into_iter()
            .next()
            .map(|file| (file.name, file.bytes));
        match self
            .pages
            .software
            .take_upload(scope, &file.name, file.bytes, proof)
            .await?
        {
            Ok(upload) => {
                let facts = &upload.facts;
                let from = facts["from"]["label"]
                    .as_str()
                    .map_or(String::new(), |label| format!(" from {label}"));
                let release = facts["release"]
                    .as_u64()
                    .map_or("a release".to_owned(), |n| format!("release {n}"));
                self.say(
                    StatusKind::Success,
                    format!(
                        "Received {release}{from} as {}. Choose it below to review and install it.",
                        upload.bundle
                    ),
                );
            }
            Err(why) => self.say(StatusKind::Error, why),
        }
        Ok(())
    }
    async fn discard(&mut self, scope: &Scope, bundle: &str) -> Result<()> {
        match self.pages.software.discard_upload(scope, bundle).await? {
            Ok(()) => self.say(StatusKind::Success, format!("Removed {bundle}.")),
            Err(why) => self.say(StatusKind::Error, why),
        }
        Ok(())
    }

    pub(super) async fn body(
        &mut self,
        scope: &Scope,
        active: bool,
        job: Html,
        extra_details: Html,
    ) -> Result<Html> {
        let client = self.pages.db.get().await?;
        authorize_operator(&**client, scope, VIEW).await?;
        let manages = authorize_operator(&**client, scope, MANAGE).await.is_ok();
        let instance = Instance::resolve(&self.pages.settings, &**client).await?;
        drop(client);
        let refresh_github = std::mem::take(&mut self.github_refresh);
        let facts = self
            .pages
            .software
            .releases_with(scope, refresh_github, self.github_prerelease)
            .await?;
        let enrolled = self.pages.software.step_up.enrolled(scope).await?;
        let update = &facts["update"];
        let release = &update["release"];
        let latest = match (
            facts["github"]["repository"].as_str(),
            release["tag"].as_str(),
            release["version"].as_str(),
        ) {
            (Some(repository), Some(tag), Some(version)) => render(ActionLink {
                id: "software-latest-notes",
                href: &format!("https://github.com/{repository}/releases/tag/{tag}"),
                title: "Release notes",
                body: Html::text(version),
                ..ActionLink::default()
            })?,
            _ => Html::text("Unavailable"),
        };
        let mut body = render(Facts {
            id: "software-update-versions",
            items: vec![
                Fact {
                    id: "software-update-installed",
                    title: "Installed version",
                    value: Html::text(update["current"].as_str().unwrap_or("Unknown")),
                },
                Fact {
                    id: "software-update-latest",
                    title: "Latest version",
                    value: latest,
                },
            ],
        })?;
        if manages {
            if !active && update["state"] == "ready" {
                let asset = &update["asset"];
                body.push(&Self::github_review_form(
                    asset,
                    asset["repository_id"].as_i64().ok_or(Error::Internal)?,
                    asset["asset"].as_i64().ok_or(Error::Internal)?,
                    "software-update",
                    &format!(
                        "Update to {}",
                        release["version"].as_str().ok_or(Error::Internal)?
                    ),
                )?);
            }
            body.push(&render(UpdatePrompt {
                id: "software-update-prompt",
                label: "Copy AI update prompt",
                prompt: &format!("Check the Bilibi server at {} for a stable update and show me the install review.", instance.origin),
            })?);
        }
        body.push(&job);
        let mut details = self.trust_section(&facts)?;
        details.push(&self.github_section(&facts, manages, enrolled)?);
        if manages {
            details.push(&self.upload_section(&instance)?);
        }
        if facts["uploads"]
            .as_array()
            .is_some_and(|uploads| !uploads.is_empty())
        {
            details.push(&self.waiting_section(&facts, manages)?);
        }
        details.push(&extra_details);
        body.push(&render(Fold {
            id: "software-release-details",
            title: "Details",
            open: self.github_error.is_some(),
            body: details,
            ..Fold::default()
        })?);
        Ok(body)
    }
    /// The file input for a release file, for a server owner.
    fn upload_section(&self, instance: &Instance) -> Result<Html> {
        let archive = self
            .uploads
            .input(UPLOAD, "release-files-upload")
            .label("Release file")
            .hint("Choose a tested .tar.gz release file, up to 64 MB.");
        let proof = self
            .uploads
            .input(PROOF, "release-files-proof")
            .label("Operator proof")
            .hint("Required when Release trust says Operator key: choose the .proof.json published beside this exact archive.");
        let mut inputs = render(archive)?;
        inputs.push(&render(proof)?);
        let form = render(Form {
            id: "release-files-upload-form",
            event: "upload_release",
            body: inputs,
            actions: render(Button {
                id: "release-files-upload-submit",
                button_type: "submit",
                variant: ButtonVariant::Primary,
                working: "Receiving…",
                body: Html::text("Upload file"),
                ..Button::default()
            })?,
            ..Form::default()
        })?;
        let description = format!(
            "Upload a tested update for {}, then review and install it below.",
            instance.label
        );
        section(
            "release-files-upload-card",
            "Release upload",
            &description,
            Html::default(),
            false,
            form,
        )
    }

    fn trust_section(&self, facts: &Value) -> Result<Html> {
        let trust = &facts["release_trust"];
        let kind = match trust["kind"].as_str() {
            Some("github") => "GitHub artifact attestations",
            Some("operator") => "Operator Ed25519 key",
            _ => "Not set",
        };
        let repository = match (
            trust["repository"].as_str(),
            trust["repository_id"].as_i64(),
        ) {
            (Some(name), Some(id)) => format!("{name} ({id})"),
            _ => "Not set".into(),
        };
        let fingerprint = trust["fingerprint"].as_str().unwrap_or("Not applicable");
        let body = render(Facts {
            id: "software-release-trust-facts",
            items: vec![
                Fact {
                    id: "software-release-trust-kind",
                    title: "Proof",
                    value: Html::text(kind),
                },
                Fact {
                    id: "software-release-trust-repository",
                    title: "Repository",
                    value: Html::text(&repository),
                },
                Fact {
                    id: "software-release-trust-workflow",
                    title: "Workflow",
                    value: Html::text(trust["workflow"].as_str().unwrap_or("Not set")),
                },
                Fact {
                    id: "software-release-trust-minimum-tag",
                    title: "Oldest accepted tag",
                    value: Html::text(trust["minimum_tag"].as_str().unwrap_or("Not set")),
                },
                Fact {
                    id: "software-release-trust-fingerprint",
                    title: "Key fingerprint",
                    value: Html::text(fingerprint),
                },
                Fact {
                    id: "software-release-trust-highest-tag",
                    title: "Highest admitted tag",
                    value: Html::text(trust["highest_admitted_tag"].as_str().unwrap_or("None yet")),
                },
                Fact {
                    id: "software-release-trust-changed-at",
                    title: "Trusted since",
                    value: Html::text(trust["changed_at"].as_str().unwrap_or("Not recorded")),
                },
                Fact {
                    id: "software-release-trust-changed-by",
                    title: "Changed by",
                    value: Html::text(trust["changed_by"].as_str().unwrap_or("Not recorded")),
                },
            ],
        })?;
        section(
            "software-release-trust",
            "Release trust",
            "Read-only facts pinned for this installation. Changing them requires the installation owner and a fresh 2FA code.",
            Html::default(),
            false,
            body,
        )
    }
    /// The uploaded release files waiting for an install.
    fn waiting_section(&self, facts: &Value, manages: bool) -> Result<Html> {
        let mut rows = Vec::new();
        for upload in facts["uploads"].as_array().into_iter().flatten() {
            let bundle = upload["bundle"].as_str().unwrap_or("").to_owned();
            let sha = upload["sha256"].as_str().unwrap_or("");
            let id = format!("release-files-waiting-{}", bundle.replace('_', "-"));
            let remove = if manages {
                render(Button {
                    id: &format!("{id}-remove"),
                    button_type: "button",
                    event: "discard_upload",
                    name: "value",
                    value: &bundle,
                    working: "Removing…",
                    body: Html::text("Remove"),
                    ..Button::default()
                })?
            } else {
                Html::default()
            };
            let mut row = FlexRow::new(bundle.clone())
                .slot("bundle", Html::text(&bundle))
                .slot(
                    "release",
                    Html::text(
                        &upload["release"]
                            .as_u64()
                            .map_or(String::new(), |n| n.to_string()),
                    ),
                )
                .slot(
                    "from",
                    Html::text(upload["from"]["label"].as_str().unwrap_or("")),
                )
                .slot(
                    "platform",
                    Html::text(&describe_platform(&upload["platform"])),
                )
                .slot(
                    "domains",
                    Html::text(describe_domains(&upload["domains"]).trim_start_matches(" with ")),
                )
                .slot(
                    "checksum",
                    Html::text(&sha.chars().take(12).collect::<String>()),
                );
            row.dom_id = Some(id);
            row.actions = remove;
            rows.push(row);
        }
        let mut table = FlexTable::new(
            "release-files-waiting-table",
            self.waiting.flex_columns(),
            rows,
        );
        table.event = "grid_waiting";
        table.zoom = self.waiting.zoom;
        table.suggestions = self.waiting.flex_suggestions();
        table.add_query = &self.waiting.add_query;
        table.framed = false;
        table.actions = manages;
        table.caption = "Uploaded releases";
        table.empty = Some(TableEmpty::Content(Html::text(
            "No release file is waiting here.",
        )));
        section(
            "release-files-waiting-card",
            "Uploaded releases",
            "Choose a file below to review and install it.",
            Html::default(),
            true,
            render(table)?,
        )
    }
    /// Setup files from the GitHub repository the owner named.
    fn github_section(&self, facts: &Value, manages: bool, enrolled: bool) -> Result<Html> {
        let github = &facts["github"];
        let repository = github["repository"].as_str().unwrap_or("");
        let accepted = github["accepted"].as_bool() == Some(true);
        let identity_changed = github["identity_changed"].as_bool() == Some(true);
        let app = github["app"].as_str();
        let minutes = github["refresh_minutes"].as_i64().unwrap_or(15);
        let mut body = render(Facts {
            id: "release-files-github-facts",
            items: vec![
                Fact {
                    title: "Repository",
                    id: "release-files-github-repository-fact",
                    value: Html::text(if repository.is_empty() {
                        "Not set"
                    } else {
                        repository
                    }),
                },
                Fact {
                    title: "Repository identity",
                    id: "release-files-github-accepted",
                    value: Html::text(if accepted { "Matches Release trust" } else { "Does not match Release trust" }),
                },
                Fact {
                    title: "Read access",
                    id: "release-files-github-token-fact",
                    value: Html::text(&match app {
                        Some(client_id) => format!("This server's GitHub App {client_id}: a new read-only token for each check"),
                        None => "Anonymous for public repositories; GitHub App required for private repositories".into(),
                    }),
                },
                Fact {
                    title: "Check every",
                    id: "release-files-github-refresh-fact",
                    value: Html::text(&format!(
                        "{minutes} minute{}",
                        if minutes == 1 { "" } else { "s" }
                    )),
                },
            ],
        })?;
        let mut notice = github["message"].as_str().unwrap_or("").to_owned();
        if github["stale"].as_bool() == Some(true) {
            if !notice.is_empty() {
                notice.push(' ');
            }
            notice.push_str("This list was saved earlier. GitHub could not be asked just now.");
        }
        if !notice.is_empty() {
            body.push(&Html::text(&notice));
        }
        let installed = &facts["installed_asset"];
        body.push(&render(Facts {
            id: "software-installed-asset",
            items: vec![
                Fact {
                    id: "software-installed-local-release",
                    title: "Local release",
                    value: Html::text(
                        &installed["local_release"]
                            .as_u64()
                            .map_or("Not recorded".into(), |n| n.to_string()),
                    ),
                },
                Fact {
                    id: "software-installed-github-asset",
                    title: "Installed GitHub asset",
                    value: Html::text(&if installed["github"].is_object() {
                        let pin = &installed["github"];
                        format!(
                            "{} · tag {} · repository {} / release {} / asset {} · {}",
                            pin["repository"].as_str().unwrap_or(""),
                            pin["tag"].as_str().unwrap_or(""),
                            pin["repository_id"],
                            pin["release_id"],
                            pin["asset_id"],
                            pin["target"].as_str().unwrap_or("")
                        )
                    } else {
                        "Not recorded".into()
                    }),
                },
                Fact {
                    id: "software-installed-archive",
                    title: "Archive SHA-256",
                    value: Html::text(
                        installed["github"]["archive_sha256"]
                            .as_str()
                            .unwrap_or("Not recorded"),
                    ),
                },
                Fact {
                    id: "software-installed-program",
                    title: "Program SHA-256",
                    value: Html::text(
                        installed["program_sha256"]
                            .as_str()
                            .unwrap_or("Not recorded"),
                    ),
                },
                Fact {
                    id: "software-installed-setup",
                    title: "Setup commit",
                    value: Html::text(
                        installed["github"]["setup_commit"]
                            .as_str()
                            .unwrap_or("Not recorded"),
                    ),
                },
            ],
        })?);
        if manages {
            let minutes_field = Field::in_form("github", "refresh_minutes")
                .id("release-files-github-minutes")
                .label("Check GitHub every this many minutes")
                .kind(InputKind::Number)
                .value(minutes.to_string())
                .hint("15 means the list is refreshed at most every 15 minutes, and again when you choose Refresh.")
                .wrapper_class("mb-0 w-full sm:w-72")
                .attr("min", "1")
                .attr("max", "1440");
            let fields = render(minutes_field)?;
            let actions = render(Button {
                id: "release-files-github-save",
                button_type: "submit",
                variant: ButtonVariant::Primary,
                working: "Saving…",
                body: Html::text("Save"),
                ..Button::default()
            })?;
            let mut step = bilibi_ui::StepUp::new(enrolled, "/system/software?tab=releases");
            step.body = fields;
            step.actions = actions;
            body.push(&render(Form {
                id: "release-files-github-form",
                event: "github_source",
                class: "mt-3",
                body: render(step)?,
                ..Form::default()
            })?);
        }
        body.push(&self.github_table(
            github,
            manages && accepted && !identity_changed,
            github["repository_id"].as_i64().unwrap_or(0),
        )?);
        let mut actions = render(Button {
            id: "release-files-github-refresh",
            button_type: "button",
            event: "refresh_github",
            working: "Checking…",
            body: Html::text("Refresh"),
            ..Button::default()
        })?;
        actions.push(&render(Button {
            id: "release-files-github-prerelease",
            button_type: "button",
            event: "github_prerelease",
            body: Html::text(if self.github_prerelease {
                "Show stable releases"
            } else {
                "Show pre-releases"
            }),
            ..Button::default()
        })?);
        section(
            "release-files-github",
            "GitHub releases",
            "Install a setup file from the repository pinned in Release trust, or upload a saved copy. Bilibi verifies the one proof policy pinned for this server before unpacking either source.",
            actions,
            false,
            body,
        )
    }
    fn github_table(&self, github: &Value, can_review: bool, repository_id: i64) -> Result<Html> {
        let mut rows = Vec::new();
        for release in github["releases"].as_array().into_iter().flatten() {
            let asset = release["asset_id"].as_i64().unwrap_or(0);
            let digest = release["digest"].as_str().unwrap_or("");
            let published = release["published_at"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(10)
                .collect::<String>();
            let tag = release["tag"].as_str().unwrap_or("");
            let tag_text = if release["prerelease"].as_bool() == Some(true) {
                format!("{tag} (pre-release)")
            } else {
                tag.to_owned()
            };
            let mut row = FlexRow::new(asset.to_string())
                .slot(
                    "asset",
                    Html::text(&format!(
                        "{repository_id}/{}/{}",
                        release["release_id"], asset
                    )),
                )
                .slot(
                    "installed",
                    Html::text(if release["installed"] == true {
                        "Yes"
                    } else {
                        ""
                    }),
                )
                .slot("tag", Html::text(&tag_text))
                .slot(
                    "target",
                    Html::text(crate::github::describe_target(
                        release["target"].as_str().unwrap_or(""),
                    )),
                )
                .slot("file", Html::text(release["name"].as_str().unwrap_or("")))
                .slot("digest", Html::text(digest))
                .slot("published", Html::text(&published));
            row.dom_id = Some(format!("release-files-github-row-{asset}"));
            if can_review && release["installed"] != true {
                row.actions = Self::github_review_form(
                    release,
                    repository_id,
                    asset,
                    &format!("release-files-github-{asset}"),
                    "Review",
                )?;
            }
            rows.push(row);
        }
        let empty_list = rows.is_empty();
        let mut table = FlexTable::new(
            "release-files-github-table",
            self.github_columns.flex_columns(),
            rows,
        );
        table.event = "grid_github";
        table.zoom = self.github_columns.zoom;
        table.suggestions = self.github_columns.flex_suggestions();
        table.add_query = &self.github_columns.add_query;
        table.framed = false;
        table.actions = can_review;
        table.caption = "GitHub releases";
        if empty_list {
            let empty = github["message"].as_str().unwrap_or(
                if github["repository"]
                    .as_str()
                    .is_some_and(|name| !name.is_empty())
                {
                    "No setup file for this server is in the list."
                } else {
                    "No GitHub repository is set."
                },
            );
            table.empty = Some(TableEmpty::Content(Html::text(empty)));
        }
        render(table)
    }
    fn github_review_form(
        release: &Value,
        repository_id: i64,
        asset: i64,
        id: &str,
        label: &str,
    ) -> Result<Html> {
        let pairs = [
            ("asset", asset.to_string()),
            ("repository_id", repository_id.to_string()),
            (
                "release_id",
                release["release_id"].as_i64().unwrap_or(0).to_string(),
            ),
            ("tag", release["tag"].as_str().unwrap_or("").to_owned()),
            (
                "target",
                release["target"].as_str().unwrap_or("").to_owned(),
            ),
            (
                "digest",
                release["digest"].as_str().unwrap_or("").to_owned(),
            ),
        ];
        let mut body = Html::EMPTY;
        for (name, value) in pairs {
            body.push(&render(
                Field::named(name)
                    .id(format!("{id}-{name}"))
                    .kind(InputKind::Hidden)
                    .value(value),
            )?);
        }
        let form_id = format!("{id}-form");
        let button_id = format!("{id}-review");
        render(Form {
            id: &form_id,
            event: "request_install",
            body,
            actions: render(Button {
                id: &button_id,
                button_type: "submit",
                variant: ButtonVariant::Primary,
                working: "Reviewing…",
                body: Html::text(label),
                ..Button::default()
            })?,
            ..Form::default()
        })
    }
}

fn section(
    id: &str,
    title: &str,
    description: &str,
    actions: Html,
    flat: bool,
    body: Html,
) -> Result<Html> {
    let heading = format!("{id}-heading");
    let mut section = render(SectionHeading {
        title,
        id: &heading,

        description,
        actions,
        ..SectionHeading::default()
    })?;
    section.push(&render(Card {
        id,
        inner_class: if flat { "p-0" } else { "" },
        body,
        ..Card::default()
    })?);
    Ok(section)
}

impl ReleaseFiles {
    /// Handles release controls within the one mounted Software page.
    pub(super) async fn event(&mut self, request: &LiveRequest<'_, Scope>) -> Result<bool> {
        {
            let client = self.pages.db.get().await?;
            authorize_operator(&**client, request.scope, VIEW).await?;
        }
        if let LiveInput::Event(value) = request.input {
            let event = value["type"].as_str().unwrap_or("");
            if let Some((expected, _)) = &self.pending_github {
                if event == "cancel_github" {
                    self.cancel_github();
                    return Ok(true);
                }
                if event != *expected {
                    return Err(Error::Invalid);
                }
                let code = value["code"].as_str().ok_or(Error::Invalid)?;
                let scope = request.scope.clone().presenting_step_up(code);
                if scope.step_up_code().is_none() {
                    self.github_error = Some("Enter a current 2FA code.".into());
                    return Ok(true);
                }
                let (event, input) = self.pending_github.take().ok_or(Error::Internal)?;
                self.github_save(request, event, input, &scope).await?;
                return Ok(true);
            }
            if value.get("code").is_some() && event == "github_source" {
                return Err(Error::Invalid);
            }
            let values =
                bilibi_ui::params::string_values_checked(value, bilibi_ui::params::FIELD_LIMIT)
                    .map_err(Error::refused)?;
            let get = |key: &str| values.get(key).map(String::as_str).unwrap_or("");
            match value["type"].as_str().unwrap_or("") {
                "github_source" => {
                    let mut input = json!({});
                    if let Ok(minutes) = get("github[refresh_minutes]").trim().parse::<i64>() {
                        input["refresh_minutes"] = json!(minutes);
                    } else if !get("github[refresh_minutes]").trim().is_empty() {
                        self.github_error = Some(
                            "Enter how many minutes to wait between GitHub checks, from 1 to 1440. 15 checks about four times an hour."
                                .into(),
                        );
                        self.say(
                            StatusKind::Error,
                            "Enter how many minutes to wait between GitHub checks, from 1 to 1440. 15 checks about four times an hour.",
                        );
                        return Ok(true);
                    }
                    self.stage_github("github_source", input);
                }
                "refresh_github" => self.github_refresh = true,
                "github_prerelease" => self.github_prerelease = !self.github_prerelease,
                "upload_release" => self.upload(request.scope).await?,
                "discard_upload" => {
                    let bundle = get("value").to_owned();
                    self.discard(request.scope, &bundle).await?;
                }
                "grid_waiting" => {
                    if let ColumnsOutcome::Sort(_) = self.waiting.handle(&values) {
                        return Err(Error::Invalid);
                    }
                }
                "grid_github" => {
                    if let ColumnsOutcome::Sort(_) = self.github_columns.handle(&values) {
                        return Err(Error::Invalid);
                    }
                }
                "clear-flash"
                    if self
                        .flash
                        .as_ref()
                        .is_some_and(|(kind, _)| kind.name() == get("key")) =>
                {
                    self.flash = None;
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
    }
}

fn describe_platform(platform: &Value) -> String {
    match platform["version"].as_str() {
        Some(version) => format!("Bilibi {version}"),
        None => "Not recorded".to_owned(),
    }
}
fn describe_domains(domains: &Value) -> String {
    let list: Vec<String> = domains
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| {
            Some(format!(
                "{} {}",
                d["id"].as_str()?,
                d["version"].as_str().unwrap_or("?")
            ))
        })
        .collect();
    if list.is_empty() {
        String::new()
    } else {
        format!(" with {}", list.join(", "))
    }
}
