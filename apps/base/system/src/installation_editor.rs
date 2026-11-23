//! Installation-owner policy for admitting customer code Extensions (ADR 28).
use super::{render, InstallationMode, Instances, Pages, INSTALLATION_MODE_KEY};
use bilibi_database::{Error, Result};
use bilibi_settings::Target;
use bilibi_tenancy::Scope;
use bilibi_ui::{
    page::{LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState},
    Alert, Button, ButtonVariant, Card, Choice, Field, FlashGroup, FlashMap, Form, Header, Html,
    InputKind, Page, PageVariant, StatusKind,
};
use serde_json::{json, Value};

pub const VIEW: &str = "base.system.installation.view";
const CHANGE: &str = "base.system.installation.mode.change";
bilibi_step_up::declare!(
    "base/system",
    bilibi_step_up::Operation::new(VIEW, "Open installation policy").floor()
);
bilibi_step_up::declare!(
    "base/system",
    bilibi_step_up::Operation::new(CHANGE, "Change installation policy")
        .always()
        .guards_setting(INSTALLATION_MODE_KEY)
);

impl Instances {
    pub async fn installation_mode(&self) -> Result<InstallationMode> {
        let mut client = self.db.get().await?;
        let tx = client.transaction().await?;
        tx.execute(
            "SELECT pg_advisory_xact_lock_shared(hashtextextended($1,0))",
            &[&"base/system.installation-mode"],
        )
        .await?;
        InstallationMode::resolve(&self.settings, &*tx).await
    }

    /// Startup and installer policy: shared services never load customer code.
    pub async fn validate_composition(&self) -> Result<()> {
        let mode = self.installation_mode().await?;
        self.composition.validate_installation_mode(mode.as_str())?;
        Ok(())
    }

    pub async fn change_installation_mode(
        &self,
        scope: &Scope,
        expected: InstallationMode,
        mode: InstallationMode,
    ) -> Result<()> {
        let mut client = self.db.get().await?;
        let mut tx = self
            .boundary
            .begin(
                &mut client,
                scope.mutation_context("system.installation-mode.change"),
            )
            .await?;
        bilibi_tenancy::prove_owner(tx.sql(), scope).await?;
        tx.sql()
            .execute(
                "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
                &[&"base/system.installation-mode"],
            )
            .await?;
        let before = InstallationMode::resolve(&self.settings, tx.sql()).await?;
        if before != expected {
            return Err(Error::Invalid);
        }
        if mode == before {
            return Ok(());
        }
        if self
            .composition
            .validate_installation_mode(mode.as_str())
            .is_err()
        {
            return Err(Error::Invalid);
        }
        self.step_up
            .require_code(
                scope,
                CHANGE,
                &format!("installation.mode:{}:{}", before.as_str(), mode.as_str()),
            )
            .await?;
        self.settings
            .put(
                &mut tx,
                scope,
                Target::Global,
                INSTALLATION_MODE_KEY,
                json!(mode.as_str()),
            )
            .await?;
        tx.commit().await
    }
}

pub(super) struct Editor {
    pages: Pages,
    mode: InstallationMode,
    pending: Option<InstallationMode>,
    flash: Option<String>,
}
impl Editor {
    pub(super) async fn new(pages: Pages) -> Result<Self> {
        let mode = pages.instances.installation_mode().await?;
        Ok(Self {
            pages,
            mode,
            pending: None,
            flash: None,
        })
    }

    pub(super) async fn body(&self, scope: &Scope) -> Result<String> {
        let client = self.pages.db.get().await?;
        bilibi_tenancy::prove_owner(&**client, scope).await?;
        drop(client);
        if self.pending.is_some() {
            return super::action_gate(
                &self.pages.instances.step_up,
                scope,
                "Installation policy",
                "save_installation_mode",
                self.flash.as_deref(),
            )
            .await;
        }
        let customer = self.pages.instances.composition.customer_extensions();
        let field = render(
            Field::named("installation[mode]")
                .id("installation-mode")
                .kind(InputKind::Select)
                .label("Installation mode")
                .value(self.mode.as_str())
                .options(vec![
                    Choice::new("Shared service", "shared"),
                    Choice::new("Isolated installation", "isolated"),
                ]),
        )?;
        let enrolled = self.pages.instances.step_up.enrolled(scope).await?;
        let mut protected = bilibi_ui::StepUp::new(enrolled, "/system/installation");
        protected.body = field;
        protected.reason = "to change which Extension code this installation admits.".into();
        let mut body = render(protected)?;
        body.push(&render(Alert {
            kind: StatusKind::Info,
            body: Html::text("Shared service admits only Platform or operator-reviewed Extensions. Isolated installation also admits customer code Extensions. Data settings, custom fields, labels, views and rules work the same in both modes."),
            ..Default::default()
        })?);
        if !customer.is_empty() {
            body.push(&render(Alert {
                kind: StatusKind::Warning,
                body: Html::text(&format!(
                    "Customer code is composed: {}. Build and install a code-free release before switching to Shared service.",
                    customer.join(", ")
                )),
                ..Default::default()
            })?);
        }
        let form = render(Form {
            id: "installation-policy",
            event: "save_installation_mode",
            body,
            actions: render(Button {
                id: "installation-save",
                button_type: "submit",
                variant: ButtonVariant::Primary,
                disabled: !enrolled,
                body: Html::text("Save installation mode"),
                ..Default::default()
            })?,
            ..Default::default()
        })?;
        let mut content = render(Header {
            title: Html::text("Installation policy"),
            subtitle: "Choose whether this whole server may run customer code Extensions.",
            ..Default::default()
        })?;
        content.push(&render(FlashGroup {
            flash: FlashMap {
                error: self.flash.as_deref().unwrap_or(""),
                ..Default::default()
            },
            ..Default::default()
        })?);
        content.push(&render(Card {
            id: "installation-policy-card",
            body: form,
            ..Default::default()
        })?);
        Ok(render(Page {
            variant: PageVariant::Form,
            body: content,
            ..Default::default()
        })?
        .to_string())
    }
}
impl LiveState<Scope, Error> for Editor {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            if let LiveInput::Event(value) = request.input {
                match value.get("type").and_then(Value::as_str) {
                    Some("clear-flash") => self.flash = None,
                    Some("save_installation_mode") => {
                        let proof = value.get("proof").and_then(Value::as_str);
                        let (mode, scope) = match (self.pending, proof) {
                            (Some(mode), Some(proof)) => {
                                (mode, request.scope.clone().presenting_step_up(proof))
                            }
                            (None, None) => {
                                let mode =
                                    match value.get("installation[mode]").and_then(Value::as_str) {
                                        Some("shared") => InstallationMode::Shared,
                                        Some("isolated") => InstallationMode::Isolated,
                                        _ => return Err(Error::Invalid),
                                    };
                                (mode, request.scope.clone())
                            }
                            _ => return Err(Error::Invalid),
                        };
                        self.pending = None;
                        self.flash = None;
                        match self.pages.instances.change_installation_mode(&scope, self.mode, mode).await {
                            Ok(()) => return Ok(LiveBody { redirect: Some("/system/installation".into()), flash: Some((StatusKind::Success, "Installation mode saved.".into())), ..Default::default() }),
                            Err(Error::StepUpRequired { reason: "code_required" | "invalid_code" | "locked", .. }) => {
                                self.pending = Some(mode);
                                if proof.is_some() {
                                    self.flash = Some("The 2FA code was refused.".into());
                                }
                            }
                            Err(Error::StepUpRequired { reason: "not_enrolled", .. }) => {
                                self.flash = Some("Set up 2FA to change the installation mode.".into());
                            }
                            Err(Error::Invalid) if mode == InstallationMode::Shared && !self.pages.instances.composition.customer_extensions().is_empty() => self.flash = Some("Shared service was refused because customer code is still composed. Build and install a code-free release first.".into()),
                            Err(Error::Forbidden) => self.flash = Some("Only the installation owner can change this setting.".into()),
                            Err(error) => return Err(error),
                        }
                    }
                    _ => return Err(Error::Invalid),
                }
            }
            Ok(LiveBody {
                html: self.body(request.scope).await?,
                ..Default::default()
            })
        })
    }
}
