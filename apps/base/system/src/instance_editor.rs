//! Core Administrator settings for instance identity.
use super::{
    render, Colour, Instance, Pages, Role, INSTANCE_COLOUR_KEY, INSTANCE_LABEL_KEY,
    INSTANCE_MANAGE, INSTANCE_ROLE_KEY,
};
use bilibi_database::{Db, Error, MutationBoundary, Result};
use bilibi_settings::{Settings, Target};
use bilibi_step_up::StepUp;
use bilibi_tenancy::Scope;
use bilibi_ui::{
    page::{LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState},
    Button, ButtonVariant, Card, Choice, Field, FlashGroup, FlashMap, Form, Header, Html,
    InputKind, Page, PageVariant, StatusKind,
};
use serde_json::{json, Value};
use std::sync::Arc;

pub const SERVICE: &str = "base/system.instances";
const CHANGE_ROLE: &str = "base.system.instance.role.change";
bilibi_step_up::declare!(
    "base/system",
    bilibi_step_up::Operation::new(CHANGE_ROLE, "Change the server role")
        .always()
        .guards_setting(INSTANCE_ROLE_KEY)
);

/// Core Administrator settings and the current 2FA enforcement policy.
pub struct Instances {
    pub(super) db: Db,
    pub(super) settings: Arc<Settings>,
    pub(super) boundary: Arc<MutationBoundary>,
    pub(super) step_up: Arc<StepUp>,
    pub(super) composition: Arc<bilibi_module_registry::Composition>,
}
/// One complete settings submission. The expected role fences a stale page.
#[derive(Clone)]
pub struct InstanceSettings {
    pub expected_role: Role,
    pub role: Role,
    pub label: String,
    pub colour: String,
}
impl Instances {
    async fn core_administrator(
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
    ) -> Result<bool> {
        Ok(scope.impersonator().is_none()
            && bilibi_authz::roles::assigned(client, scope, scope.user())
                .await?
                .iter()
                .any(|role| role.is_system && role.code.as_deref() == Some("core_admin")))
    }
    async fn authorize_in(
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
    ) -> Result<()> {
        bilibi_authz::authorize_operator(client, scope, INSTANCE_MANAGE).await?;
        if Self::core_administrator(client, scope).await? {
            Ok(())
        } else {
            Err(Error::Forbidden)
        }
    }
    /// Whether the signed-in account is the Core Administrator, as an unlogged
    /// read for the shell to choose a link. The caller adds the capability
    /// from its own effective set; the editor and `save` still run `authorize_in`.
    pub async fn is_core_administrator(&self, scope: &Scope) -> Result<bool> {
        let client = self.db.get().await?;
        Self::core_administrator(&**client, scope).await
    }

    pub async fn resolve(&self) -> Result<Instance> {
        let client = self.db.get().await?;
        Instance::resolve(&self.settings, &**client).await
    }
    pub async fn save(&self, scope: &Scope, input: InstanceSettings) -> Result<()> {
        let mut client = self.db.get().await?;
        let mut tx = self
            .boundary
            .begin(&mut client, scope.mutation_context("system.instance.save"))
            .await?;
        Self::authorize_in(tx.sql(), scope).await?;
        // Serialize role changes and compare the page's role before spending a code.
        tx.sql()
            .execute(
                "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
                &[&"base/system.instance"],
            )
            .await?;
        let before = Instance::resolve(&self.settings, tx.sql()).await?;
        if before.role != input.expected_role {
            return Err(Error::Invalid);
        }
        if input.role != before.role {
            self.step_up
                .require_code(
                    scope,
                    CHANGE_ROLE,
                    &format!(
                        "instance.role:{}:{}",
                        before.role.env_value(),
                        input.role.env_value()
                    ),
                )
                .await?;
            self.settings
                .put(
                    &mut tx,
                    scope,
                    Target::Global,
                    INSTANCE_ROLE_KEY,
                    json!(input.role.env_value()),
                )
                .await?;
        }
        for (key, value) in [
            (INSTANCE_LABEL_KEY, input.label),
            (INSTANCE_COLOUR_KEY, input.colour),
        ] {
            if key == INSTANCE_COLOUR_KEY && before.role == Role::Production {
                continue;
            }
            if value.trim().is_empty() {
                self.settings
                    .delete(&mut tx, scope, Target::Global, key)
                    .await?;
            } else {
                self.settings
                    .put(&mut tx, scope, Target::Global, key, json!(value.trim()))
                    .await?;
            }
        }
        tx.commit().await
    }
}
pub(super) struct EnforcementPolicy {
    pub db: Db,
    pub settings: Arc<Settings>,
}
impl bilibi_step_up::Enforcement for EnforcementPolicy {
    fn enforced(&self) -> bilibi_tenancy::ProofFuture<'_, bool> {
        Box::pin(async move {
            let client = self
                .db
                .get_independent(
                    "step-up asks this from inside the caller's write, through a hook that \
                     carries no client; one short read",
                )
                .await?;
            Ok(Instance::resolve(&self.settings, &**client).await?.role != Role::Dev)
        })
    }
}

pub(super) struct Editor {
    pages: Pages,
    role: Role,
    flash: Option<String>,
    draft: Option<Value>,
    pending: Option<InstanceSettings>,
}
impl Editor {
    pub(super) async fn new(pages: Pages) -> Result<Self> {
        let role = pages.instances.resolve().await?.role;
        Ok(Self {
            pages,
            role,
            flash: None,
            draft: None,
            pending: None,
        })
    }
    pub(super) async fn body(&self, scope: &Scope) -> Result<String> {
        let client = self.pages.db.get().await?;
        Instances::authorize_in(&**client, scope).await?;
        drop(client);
        if self.pending.is_some() {
            return super::action_gate(
                &self.pages.instances.step_up,
                scope,
                "Server Instance",
                "save_instance",
                self.flash.as_deref(),
            )
            .await;
        }
        let instance = self.pages.instances.resolve().await?;
        let draft = |key: &str| {
            self.draft
                .as_ref()
                .and_then(|value| value.get(format!("instance[{key}]")))
                .and_then(Value::as_str)
        };
        let enrolled = self.pages.instances.step_up.enrolled(scope).await?;
        let role_field = render(
            Field::named("instance[role]")
                .id("instance-role")
                .kind(InputKind::Select)
                .label("Server Instance")
                .value(draft("role").unwrap_or(instance.role.env_value()))
                .options(vec![
                    Choice::new("Dev", "development"),
                    Choice::new("Staging", "staging"),
                    Choice::new("Production", "production"),
                ]),
        )?;
        let mut role_step = bilibi_ui::StepUp::new(enrolled, "/system/instance");
        role_step.body = role_field;
        role_step.reason = "to change the server role.".into();
        let mut body = render(role_step)?;
        let client = self.pages.db.get().await?;
        for (key, label, hint) in [
            (INSTANCE_LABEL_KEY, "Server Label", "Display server name"),
            (
                INSTANCE_COLOUR_KEY,
                "Dev and Staging banner colour",
                "Choose a colour, or follow the server role.",
            ),
        ] {
            if key == INSTANCE_COLOUR_KEY && self.role == Role::Production {
                continue;
            }
            let value = self
                .pages
                .instances
                .settings
                .get_global(&**client, key)
                .await?;
            let field = Field::named(format!("instance[{key}]"))
                .id(key.replace('.', "-"))
                .label(label)
                .hint(hint)
                .value(draft(key).unwrap_or_else(|| {
                    value.as_str().unwrap_or_else(|| {
                        if key == INSTANCE_LABEL_KEY {
                            self.role.label()
                        } else {
                            ""
                        }
                    })
                }));
            if key == INSTANCE_COLOUR_KEY {
                body.push(&render(super::instance::colour_input(
                    field,
                    self.role.env_value(),
                ))?);
            } else {
                body.push(&render(field)?);
            }
        }
        let form = render(Form {
            id: "instance-settings",
            event: "save_instance",
            body,
            actions: render(Button {
                id: "instance-save",
                button_type: "submit",
                variant: ButtonVariant::Primary,
                body: Html::text("Save"),
                ..Default::default()
            })?,
            ..Default::default()
        })?;
        let mut content = render(Header {
            title: Html::text("Server Instance"),
            subtitle: "Set this instance as Dev, Staging, or Production",
            ..Default::default()
        })?;
        content.push(
            &instance
                .banner("instance-settings-banner", "mb-4")
                .map_err(|_| Error::Internal)?,
        );
        content.push(&render(FlashGroup {
            flash: FlashMap {
                error: self.flash.as_deref().unwrap_or(""),
                ..Default::default()
            },
            ..Default::default()
        })?);
        content.push(&render(Card {
            id: "instance-settings-card",
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
    fn settings(&self, value: &Value) -> Result<InstanceSettings> {
        let text = |name: &str| {
            value
                .get(format!("instance[{name}]"))
                .and_then(Value::as_str)
                .unwrap_or("")
        };
        let role = match text("role") {
            "development" => Role::Dev,
            "staging" => Role::Staging,
            "production" => Role::Production,
            "" => self.role, // The shared enrollment guard disables the role field.
            _ => return Err(Error::Invalid),
        };
        let colour = text(INSTANCE_COLOUR_KEY);
        if self.role != Role::Production && !colour.is_empty() && Colour::parse(colour).is_none() {
            return Err(Error::Invalid);
        }
        Ok(InstanceSettings {
            expected_role: self.role,
            role,
            label: text(INSTANCE_LABEL_KEY).into(),
            colour: colour.into(),
        })
    }
}
impl LiveState<Scope, Error> for Editor {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            let client = self.pages.db.get().await?;
            Instances::authorize_in(&**client, request.scope).await?;
            drop(client);
            if let LiveInput::Event(value) = request.input {
                match value.get("type").and_then(Value::as_str) {
                    Some("clear-flash") => self.flash = None,
                    Some("save_instance") => {
                        let proof = value.get("proof").and_then(Value::as_str);
                        let input = match (self.pending.take(), proof) {
                            (Some(input), Some(_)) => Ok(input),
                            (None, None) => {
                                self.draft = Some(Value::Object(
                                    ["role", INSTANCE_LABEL_KEY, INSTANCE_COLOUR_KEY]
                                        .into_iter()
                                        .filter_map(|key| {
                                            let key = format!("instance[{key}]");
                                            value.get(&key).cloned().map(|value| (key, value))
                                        })
                                        .collect(),
                                ));
                                self.settings(value)
                            }
                            (pending, _) => {
                                self.pending = pending;
                                return Err(Error::Invalid);
                            }
                        };
                        let scope = request
                            .scope
                            .clone()
                            .presenting_step_up(proof.unwrap_or(""));
                        self.flash = None;
                        let result = match input {
                            Ok(input) => {
                                let result = self.pages.instances.save(&scope, input.clone()).await;
                                if matches!(
                                    result,
                                    Err(Error::StepUpRequired {
                                        reason: "code_required" | "invalid_code" | "locked",
                                        ..
                                    })
                                ) {
                                    self.pending = Some(input);
                                }
                                result
                            }
                            Err(error) => Err(error),
                        };
                        match result {
                            Ok(()) => return Ok(LiveBody { redirect: Some("/system/instance".into()), flash: Some((StatusKind::Success, "Instance settings saved.".into())), ..Default::default() }),
                            Err(Error::StepUpRequired { .. }) if self.pending.is_some() => {
                                if proof.is_some() {
                                    self.flash = Some("The 2FA code was refused.".into());
                                }
                            }
                            Err(error @ Error::StepUpRequired { reason: "not_enrolled", .. }) | Err(error @ Error::Unauthorized) => return Err(error),
                            Err(Error::Forbidden) => self.flash = Some("Only a Core Administrator can change instance settings.".into()),
                            Err(Error::Invalid) => self.flash = Some("The settings were refused. Check the values, then reload if the server role has changed.".into()),
                            Err(_) => self.flash = Some("The instance settings could not be saved. Try again.".into()),
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
