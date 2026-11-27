//! `/users/{id}`: the account
//! as read-first facts. Name commits in place through the shared inline
//! editor; email is an installation-owned identity fact. The Person is a choice fact over the owner's list, written
//! through the account binder's commands, which prove and lock the target.
//! Desks are managed through the shared panel. Every write asks Authz and the
//! owners again; the mount-time flags only decide what renders.
use super::*;
use crate::{
    accounts::AccountField,
    contributions::{DELETE, UPDATE},
    Account, AccountError, LinkError,
};
use bilibi_agent_api::{page::Call, Error as ContractError};
use bilibi_authz::{
    grants::{Assignment, Direct},
    roles::{RoleSummary, UPDATE as UPDATE_ROLE},
    Effective, Reach,
};
use bilibi_database::Error;
use bilibi_party_ids::PersonId;
use bilibi_tenancy::OrganisationDirectory;
use bilibi_tenancy::OrganisationSearch;
use bilibi_ui::{
    lists::RecordLink,
    page::{LiveBody, LiveFuture, LiveInput, LiveRequest, LiveState, PanelSpec},
    ActionLink, BackLink, Badge, BadgeKind, BuiltinColumn, Button, ButtonVariant, Card, Choice,
    CommitStates, CommitStatus, ConfirmDialog, Fact, Facts, Field, FilterToolbar, FlashGroup,
    FlexRow, FlexTable, Header, IconButton, InlineChoice, InlineEdit, InlineField, InputKind,
    ListColumns, PageVariant, SectionHeading, TableEmpty, ToolbarControl,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

const FORBIDDEN: &str = "You do not have permission to edit users.";
const OUTRANKED: &str = "You cannot change this account: it has permissions you do not have.";
const IMPERSONATING: &str = "Accounts cannot be changed while viewing as someone else.";
const MANAGE_ROLES_FORBIDDEN: &str = "You do not have permission to manage roles.";
const MANAGE_CAPS_FORBIDDEN: &str = "You do not have permission to manage capabilities.";

#[derive(Template)]
#[template(path = "account_detail.html")]
struct View {
    id: String,
    flash: Html,
    header: Html,
    details: Html,
    access: Html,
    links: Html,
    danger: Html,
    dialog: Html,
}
#[derive(Template)]
#[template(path = "account_actions.html")]
struct Actions {
    impersonate: Html,
    recovery: Html,
    back: Html,
}
#[derive(Template)]
#[template(
    source = r#"<span class="text-ink-muted">{{ text }}</span>"#,
    ext = "html"
)]
struct Muted<'a> {
    text: &'a str,
}
#[derive(Template)]
#[template(
    source = r#"<span class="text-ink-muted tabular-nums">{{ value }}</span>"#,
    ext = "html"
)]
struct Stamp {
    value: Html,
}
#[derive(Template)]
#[template(path = "account_danger.html")]
struct Danger {
    button: Html,
}
#[derive(Template)]
#[template(
    source = r#"<form id="user-{{ field }}-form">{{ select }}</form>{{ note }}"#,
    ext = "html"
)]
struct ChoiceEditor {
    field: &'static str,
    select: Html,
    note: Html,
}

/// The Roles & Permissions card.
#[derive(Template)]
#[template(path = "account_access.html")]
struct AccessView {
    heading: Html,
    assignments: Html,
    assignment_fields: Html,
    can_edit_capabilities: bool,
    control: &'static str,
    show_assign: bool,
    role_search: String,
    roles: Vec<RoleOption>,
    selected_roles: usize,
    show_effective: bool,
    effective_count: usize,
    permission_search: String,
    permission_filter: Html,
    effective: Vec<DomainGroup>,
    denied: Vec<DomainGroup>,
    cap_control: &'static str,
    cap_search: String,
    caps: Html,
    selected_caps: usize,
}
struct RoleOption {
    id: i64,
    name: String,
    checked: bool,
}
struct DomainGroup {
    domain: String,
    fact_id: String,
    caps: Vec<CapChip>,
}
struct CapChip {
    key: String,
    dom_id: String,
    direct: bool,
    /// The chip's control: its id and accessible label.
    control_id: String,
    control_label: String,
}
fn dom_id(key: &str) -> String {
    key.replace('.', "-")
}

/// A held, not yet applied, destructive request; the dialog names it.
enum Pending {
    Delete,
    Unlink(String),
    Authz(AuthzRule),
}
#[derive(Clone)]
enum AuthzRule {
    SetRoles {
        ids: Vec<i64>,
        bounds: Assignment,
    },
    RemoveRole {
        id: i64,
        name: String,
        company: Option<i64>,
    },
    Deny(String),
    RemoveGrant(String),
    RemoveDenial(String),
}
impl AuthzRule {
    fn consequence(&self) -> String {
        match self {
            Self::SetRoles { ids, bounds } => format!(
                "Change {} role assignment(s) on your account for {}, from {} through {}? This may end your access.",
                ids.len(),
                bounds
                    .company_id
                    .map(|id| format!("company {id}"))
                    .unwrap_or_else(|| "every assigned workplace".into()),
                bounds.valid_from.as_deref().unwrap_or("no start date"),
                bounds.valid_until.as_deref().unwrap_or("no end date")
            ),
            Self::RemoveRole { name, .. } => {
                format!("The {name} role will be removed from your account.")
            }
            Self::Deny(key) => format!("{key} will be denied for your account."),
            Self::RemoveGrant(key) => {
                format!("The direct grant of {key} will be removed from your account.")
            }
            Self::RemoveDenial(key) => {
                format!("The deny rule for {key} will be removed from your account.")
            }
        }
    }
    fn verb(&self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::SetRoles { .. } => (
                "Change assignments",
                "Changing…",
                "confirm_role_assignments",
            ),
            Self::Deny(_) => ("Deny", "Denying…", "deny_capability"),
            Self::RemoveRole { .. } => ("Remove", "Removing…", "remove_role"),
            _ => ("Remove", "Removing…", "remove_capability"),
        }
    }
}

#[derive(Default)]
struct AccessUi {
    show_assign: bool,
    role_search: String,
    selected_roles: Vec<String>,
    assignment_company: String,
    /// The grant company's search text; its hits are read for each view.
    assignment_query: String,
    assignment_from: String,
    assignment_until: String,
    assignment_purpose: String,
    show_effective: bool,
    permission_search: String,
    cap_search: String,
    selected_caps: Vec<String>,
}

/// Everything one render reads, freshly, so a change by another operator
/// reaches the next patch.
struct Loaded {
    account: Account,
    can_manage: bool,
    can_manage_roles: bool,
    can_delete: bool,
    person: Option<(String, bool)>,
    persons: Option<Vec<PersonChoice>>,
}
impl Loaded {
    fn can_edit(&self) -> bool {
        self.can_manage
    }
    fn can_edit_roles(&self) -> bool {
        self.can_manage_roles
    }
}

pub(crate) struct Detail {
    pages: Pages,
    id: i64,
    framed: bool,
    statuses: CommitStates,
    flash: Flash,
    pending: Option<Pending>,
    access: AccessUi,
    assignment_columns: ListColumns,
    history: bool,
    desk_person: Option<PersonId>,
}

impl Detail {
    pub fn new(pages: Pages, id: i64, framed: bool) -> Result<Self> {
        Ok(Self {
            pages,
            id,
            framed,
            statuses: CommitStates::default(),
            flash: Flash::default(),
            pending: None,
            access: AccessUi::default(),
            assignment_columns: ListColumns::mount(
                "user-role-assignments",
                vec![
                    BuiltinColumn::new("role", "Role"),
                    BuiltinColumn::new("company", "Company"),
                    BuiltinColumn::new("from", "Valid from (UTC)"),
                    BuiltinColumn::new("until", "Valid until (UTC)"),
                    BuiltinColumn::new("purpose", "Purpose"),
                    BuiltinColumn::new("state", "State"),
                ],
            )
            .map_err(|_| Error::Internal)?,
            history: false,
            desk_person: None,
        })
    }

    async fn load(&self, scope: &Scope) -> Result<Loaded> {
        let account = self.pages.users.account(scope, self.id).await?;
        let can_manage = self.pages.allowed(scope, UPDATE).await?;
        let can_manage_roles = self.pages.allowed(scope, UPDATE_ROLE).await?;
        let can_delete = self.pages.allowed(scope, DELETE).await?;
        let person = match account.links.person {
            None => None,
            Some(person) => Some(match self.pages.users.persons.read(scope, person).await {
                Ok(person) => (person.name, person.retired_at.is_some()),
                Err(Error::Forbidden) | Err(Error::NotFound) => {
                    (format!("Person {}", person.get()), false)
                }
                Err(error) => return Err(error),
            }),
        };
        let persons = self.pages.person_choices(scope).await?;
        Ok(Loaded {
            account,
            can_manage,
            can_manage_roles,
            can_delete,
            person,
            persons,
        })
    }

    fn inline(&self, loaded: &Loaded, key: &'static str, label: &str, value: &str) -> InlineField {
        let mut field = InlineField::new(format!("user-{key}"), key, label, value)
            .version(loaded.account.version)
            .status(self.statuses.get(key));
        field.id_value = self.id.to_string();
        if !loaded.can_edit() {
            field = field.readonly();
        }
        field
    }

    async fn details(&self, scope: &Scope, loaded: &Loaded) -> Result<Html> {
        let client = self.pages.db.get().await?;
        let words = bilibi_grid::RecordWords::load(&**client, scope, "users").await?;
        drop(client);
        let display = self.pages.display(scope).await?;
        let account = &loaded.account;
        let editable = loaded.can_edit();
        // The Person choice: the owner's list decides what is offered.
        let person_value = account
            .links
            .person
            .map(|id| id.get().to_string())
            .unwrap_or_default();
        // As a company fact: a link only when the fact is read-only,
        // since an editable fact's display is the editor's trigger.
        let person_editable = editable && loaded.persons.is_some();
        let person_display = match (&loaded.person, account.links.person) {
            (Some((name, retired)), Some(id)) => {
                let mut html = if person_editable {
                    Html::text(name)
                } else {
                    render(&RecordLink {
                        workspace: self.framed,
                        kind: "core/person",
                        record_id: &id.get().to_string(),
                        href: &format!("/contacts/{}", id.get()),
                        class: "text-link hover:underline",
                        body: Html::text(name),
                    })?
                };
                if *retired {
                    html.push(&render(&Muted { text: " (retired)" })?);
                }
                html
            }
            _ => render(&Muted { text: "None" })?,
        };
        let mut person_field = self.inline(
            loaded,
            "person",
            words.get(&crate::record_fields::PERSON_ID),
            &person_value,
        );
        person_field = person_field.allow_empty();
        let person_editor = match &loaded.persons {
            Some(persons) if person_editable => {
                let mut options: Vec<Choice> = persons
                    .iter()
                    .map(|p| Choice::new(&p.label, p.id.get().to_string()))
                    .collect();
                if let (Some(id), Some((name, _))) = (account.links.person, &loaded.person) {
                    if !options.iter().any(|o| o.value == id.get().to_string()) {
                        options.insert(0, Choice::new(name, id.get().to_string()));
                    }
                }
                render(&ChoiceEditor {
                    field: "person",
                    select: render(
                        &Field::named("person")
                            .id("user-person-select")
                            .kind(InputKind::Select)
                            .value(&person_value)
                            .prompt("None")
                            .options(options)
                            .aria_label(words.get(&crate::record_fields::PERSON_ID))
                            .wrapper_class("mb-0"),
                    )?,
                    note: Html::EMPTY,
                })?
            }
            _ => {
                person_field = person_field.readonly();
                Html::EMPTY
            }
        };
        let verified = match account.email_verified_at {
            Some(at) => render(&Badge {
                kind: BadgeKind::Success,
                body: datetime(&display, "user-email-verified-at", at)?,
                ..Badge::default()
            })?,
            None => render(&Badge::status(BadgeKind::Warning, "unverified"))?,
        };
        let mut email_field = self.inline(
            loaded,
            "email",
            words.get(&crate::record_fields::EMAIL),
            &account.email,
        );
        email_field = email_field.readonly();
        let items = vec![
            Fact {
                title: words.get(&crate::record_fields::get("name")),
                id: "user-view-name",
                value: render(&InlineEdit {
                    field: self.inline(
                        loaded,
                        "name",
                        words.get(&crate::record_fields::get("name")),
                        &account.name,
                    ),
                })?,
            },
            Fact {
                title: words.get(&crate::record_fields::EMAIL),
                id: "user-view-email",
                value: render(&InlineEdit { field: email_field })?,
            },
            Fact {
                title: words.get(&crate::record_fields::PERSON_ID),
                id: "user-view-person",
                value: render(&InlineChoice {
                    field: person_field,
                    display: person_display,
                    editor: person_editor,
                })?,
            },
            Fact {
                title: words.get(&crate::record_fields::EMAIL_VERIFIED_AT),
                id: "user-view-email-verified",
                value: verified,
            },
            Fact {
                title: words.get(&crate::record_fields::CREATED_AT),
                id: "user-view-created",
                value: render(&Stamp {
                    value: datetime(&display, "user-created-at", account.created_at)?,
                })?,
            },
            Fact {
                title: words.get(&crate::record_fields::UPDATED_AT),
                id: "user-view-updated",
                value: render(&Stamp {
                    value: datetime(&display, "user-updated-at", account.updated_at)?,
                })?,
            },
        ];
        let mut body = render(&SectionHeading {
            id: "user-details-heading",
            title: "User Details",
            ..SectionHeading::default()
        })?;
        body.push(&render(&Facts {
            id: "user-details",
            items,
        })?);
        render(&Card {
            id: "user-details-card",
            inner_class: "p-5 sm:p-6",
            role: "region",
            labelledby: "user-details-heading",
            body,
            ..Card::default()
        })
    }

    fn read_assignment(&mut self, value: &Value) {
        self.access.assignment_company = text(value, "assignment[company]");
        self.access.assignment_query = text(value, "assignment[company_search]");
        self.access.assignment_from = text(value, "assignment_from");
        self.access.assignment_until = text(value, "assignment_until");
        self.access.assignment_purpose = text(value, "assignment_purpose");
    }
    fn assignment(&self) -> Result<Assignment> {
        let optional = |value: &str| (!value.trim().is_empty()).then(|| value.trim().to_owned());
        let bounds = Assignment {
            company_id: optional(&self.access.assignment_company)
                .map(|v| v.parse::<i64>().map_err(|_| Error::Invalid))
                .transpose()?,
            valid_from: optional(&self.access.assignment_from),
            valid_until: optional(&self.access.assignment_until),
            purpose: self.access.assignment_purpose.trim().to_owned(),
        };
        bounds.validate()?;
        Ok(bounds)
    }
    async fn assignment_fields(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
    ) -> Result<Html> {
        let grants = &self.pages.grants;
        let chosen = match self
            .access
            .assignment_company
            .trim()
            .parse::<i64>()
            .ok()
            .and_then(|id| bilibi_party_ids::OrganisationId::new(id).ok())
        {
            Some(id) => {
                let ids = [id];
                grants
                    .assignable_companies(client, scope, OrganisationSearch::among(&ids))
                    .await?
                    .into_iter()
                    .next()
            }
            None => None,
        };
        let query = &self.access.assignment_query;
        let hits = if chosen.is_none() && !query.trim().is_empty() {
            grants
                .assignable_companies(client, scope, OrganisationSearch::text(query))
                .await?
        } else {
            Vec::new()
        };
        let mut body = super::company_picker(
            "role-assignment-company",
            "assignment",
            "company",
            "Company (empty: every workplace; a grant adds no desk)",
            query,
            &hits,
            chosen.as_ref(),
            ("pick_assignment_company", "change_assignment_company"),
            Vec::new(),
        )?;
        body.push(&render(
            &Field::named("assignment_from")
                .label("Valid from (UTC)")
                .kind(InputKind::Date)
                .value(&self.access.assignment_from)
                .hint("Leave empty to make the assignment effective now."),
        )?);
        body.push(&render(
            &Field::named("assignment_until")
                .label("Valid until (UTC)")
                .kind(InputKind::Date)
                .value(&self.access.assignment_until)
                .hint("Leave empty for no end date. Otherwise access ends after this UTC date."),
        )?);
        body.push(&render(
            &Field::named("assignment_purpose")
                .label("Purpose")
                .value(&self.access.assignment_purpose)
                .hint("Name the engagement and its appointment or authority reference."),
        )?);
        Ok(body)
    }
    async fn role_assignments(
        &self,
        client: &(impl tokio_postgres::GenericClient + Sync),
        scope: &Scope,
        can_edit: bool,
        records: &[bilibi_authz::roles::RoleAssignment],
        providers: &[RoleSummary],
        groups: &[bilibi_authz::roles::GroupAssignment],
    ) -> Result<Html> {
        let mut rows = Vec::new();
        for record in records {
            let key = format!(
                "{}/{}",
                record.role.id,
                record
                    .bounds
                    .company_id
                    .map(|id| id.to_string())
                    .unwrap_or_default()
            );
            let company = match record.bounds.company_id {
                // A grant's company need not own a role, so name it directly:
                // `lookup` answers every live company of the tenant.
                Some(id) => bilibi_company::Directory
                    .lookup(
                        client,
                        scope,
                        bilibi_party_ids::OrganisationId::new(id).map_err(|_| Error::Invalid)?,
                    )
                    .await?
                    .map(|c| c.name)
                    .unwrap_or_else(|| format!("Company {id}")),
                None => "All workspaces".into(),
            };
            let mut row = FlexRow::new(&key)
                .slot("role", Html::text(&record.role.name))
                .slot("company", Html::text(&company))
                .slot(
                    "from",
                    Html::text(
                        record
                            .bounds
                            .valid_from
                            .as_deref()
                            .unwrap_or("No start date"),
                    ),
                )
                .slot(
                    "until",
                    Html::text(
                        record
                            .bounds
                            .valid_until
                            .as_deref()
                            .unwrap_or("No end date"),
                    ),
                )
                .slot("purpose", Html::text(&record.bounds.purpose))
                .slot(
                    "state",
                    Html::text(if record.effective {
                        "Current"
                    } else {
                        "Outside its dates or requirements"
                    }),
                );
            if can_edit {
                row.actions = render(&IconButton {
                    event: "request_remove_role",
                    name: "role_id",
                    value: &key,
                    ..IconButton::new("trash", "Remove this role assignment")
                })?;
            }
            rows.push(row);
        }
        for role in providers {
            rows.push(
                FlexRow::new(format!("provider-{}", role.id))
                    .slot("role", Html::text(&role.name))
                    .slot("company", Html::text("All workspaces"))
                    .slot("state", Html::text("Managed by sign-in provider")),
            );
        }
        // A role held through a team says which team, unless the team is one
        // the reader may not see (a company they do not work in): then only
        // that it comes from one. Its company is the role's scope.
        let reach: Vec<i64> = if groups.is_empty() {
            Vec::new()
        } else {
            bilibi_company::Directory
                .reachable(client, scope)
                .await?
                .into_iter()
                .map(|c| c.id.get())
                .collect()
        };
        for held in groups {
            let company = match held.company_id {
                Some(id) => bilibi_company::Directory
                    .lookup(
                        client,
                        scope,
                        bilibi_party_ids::OrganisationId::new(id).map_err(|_| Error::Invalid)?,
                    )
                    .await?
                    .map(|c| c.name)
                    .unwrap_or_else(|| format!("Company {id}")),
                None => "All workspaces".into(),
            };
            let state = if held.company_id.is_none_or(|id| reach.contains(&id)) {
                format!("Via {} {}", held.source, held.label)
            } else {
                format!("Via a {} in a company you do not work in", held.source)
            };
            rows.push(
                FlexRow::new(format!(
                    "{}-{}-{}",
                    held.source, held.group_id, held.role.id
                ))
                .slot("role", Html::text(&held.role.name))
                .slot("company", Html::text(&company))
                .slot("state", Html::text(&state)),
            );
        }
        let mut table = FlexTable::new(
            "user-role-assignments",
            self.assignment_columns.flex_columns(),
            rows,
        );
        table.event = "assignment_grid";
        table.zoom = self.assignment_columns.zoom;
        table.add_query = &self.assignment_columns.add_query;
        table.suggestions = self.assignment_columns.flex_suggestions();
        table.actions = can_edit;
        table.empty = Some(TableEmpty::Content(Html::text("No role assignments.")));
        render(&table)
    }

    async fn access(&self, scope: &Scope, loaded: &Loaded) -> Result<Html> {
        let client = self.pages.db.get().await?;
        let user = self.id;
        let target: Effective =
            bilibi_authz::effective_capabilities_of(&**client, scope, user).await?;
        let assignments = bilibi_authz::roles::assignments(&**client, scope, user).await?;
        let providers = bilibi_authz::roles::provider_assignments(&**client, scope, user).await?;
        let groups = bilibi_authz::roles::group_assignments(&**client, scope, user).await?;
        let actor = bilibi_authz::effective_capabilities_of(&**client, scope, scope.user()).await?;
        let companies = self
            .pages
            .grants
            .visible_companies(&**client, scope)
            .await?;
        let all_roles = bilibi_authz::roles::list(
            &**client,
            scope,
            &companies,
            &bilibi_authz::roles::Query {
                search: String::new(),
                sort: bilibi_authz::roles::Sort::Name,
                descending: false,
                page: 1,
                page_size: bilibi_authz::roles::MAX_PAGE_SIZE,
            },
        )
        .await?
        .entries;
        let unassigned: Vec<&RoleSummary> = all_roles.iter().collect();
        let grants = bilibi_authz::roles::grants_of(
            &**client,
            scope,
            &unassigned.iter().map(|r| r.id).collect::<Vec<_>>(),
        )
        .await?;
        // The escalation guard: a role is grantable only by an actor holding
        // everything it grants at least as far; a grant-all role only by a
        // grant-all actor.
        let grantable = |id: i64| -> bool {
            match grants.get(&id) {
                Some((true, _)) => actor.grant_all,
                Some((false, capabilities)) => capabilities
                    .iter()
                    .all(|(key, reach)| actor.reaches(key, *reach)),
                None => false,
            }
        };
        let available: Vec<&RoleSummary> = unassigned
            .iter()
            .copied()
            .filter(|r| grantable(r.id))
            .collect();
        let control = if !loaded.can_manage_roles {
            "forbidden"
        } else if target.grant_all {
            "grant_all"
        } else if all_roles.is_empty() {
            "no_roles"
        } else if unassigned.is_empty() {
            "all_assigned"
        } else if available.is_empty() {
            "none_grantable"
        } else {
            "available"
        };
        let needle = self.access.role_search.trim().to_lowercase();
        let roles: Vec<RoleOption> = available
            .iter()
            .filter(|r| needle.is_empty() || r.name.to_lowercase().contains(&needle))
            .map(|r| RoleOption {
                id: r.id,
                name: r.name.clone(),
                checked: self.access.selected_roles.contains(&r.id.to_string()),
            })
            .collect();
        // `prefix` names the chip's control: a direct grant and a deny rule
        // are removed, a role-granted capability is denied.
        let group =
            |keys: &[String], direct: &BTreeSet<String>, prefix: &str| -> Vec<DomainGroup> {
                let mut groups: BTreeMap<String, Vec<CapChip>> = BTreeMap::new();
                for key in keys {
                    let domain = key.split('.').next().unwrap_or("other").to_owned();
                    let is_direct = direct.contains(key);
                    let (control_id, control_label) = match (prefix, is_direct) {
                        ("denied", _) => (
                            format!("remove-denial-{}", dom_id(key)),
                            format!("Remove the deny rule for {key}"),
                        ),
                        (_, true) => (
                            format!("remove-direct-cap-{}", dom_id(key)),
                            format!("Remove the direct grant of {key}"),
                        ),
                        (_, false) => (format!("deny-cap-{}", dom_id(key)), format!("Deny {key}")),
                    };
                    groups.entry(domain).or_default().push(CapChip {
                        key: key.clone(),
                        dom_id: dom_id(key),
                        direct: is_direct,
                        control_id,
                        control_label,
                    });
                }
                groups
                    .into_iter()
                    .map(|(domain, caps)| DomainGroup {
                        fact_id: format!("{prefix}-domain-{domain}"),
                        domain,
                        caps,
                    })
                    .collect()
            };
        let effective_keys: Vec<String> = target.allowed.iter().map(|k| (*k).to_owned()).collect();
        let denied_keys: Vec<String> = target.direct_denied.iter().cloned().collect();
        let excluded: BTreeSet<&str> = effective_keys
            .iter()
            .chain(denied_keys.iter())
            .map(String::as_str)
            .collect();
        let vocabulary = bilibi_authz::vocabulary()?;
        let remaining: Vec<&str> = vocabulary
            .capabilities
            .keys()
            .copied()
            .filter(|key| !excluded.contains(key))
            .collect();
        let available_caps: Vec<&str> = remaining
            .iter()
            .copied()
            .filter(|key| actor.reaches(key, Reach::All))
            .collect();
        let cap_control = if !loaded.can_manage {
            "forbidden"
        } else if remaining.is_empty() {
            "all_in_effect"
        } else if available_caps.is_empty() {
            "none_grantable"
        } else {
            "available"
        };
        let cap_needle = self.access.cap_search.trim().to_lowercase();
        let caps: Vec<bilibi_ui::PermissionChoice> = available_caps
            .iter()
            .filter(|key| cap_needle.is_empty() || key.to_lowercase().contains(&cap_needle))
            .map(|key| bilibi_ui::PermissionChoice {
                key: (*key).to_owned(),
                dom_id: dom_id(key),
                checked: self.access.selected_caps.iter().any(|k| k == key),
            })
            .collect();
        let heading = render(&SectionHeading {
            id: "user-roles-heading",
            title: "Roles & Permissions",
            count: Some(assignments.len() + providers.len() + groups.len()),
            ..SectionHeading::default()
        })?;
        let body = render(&AccessView {
            heading,
            assignments: self
                .role_assignments(
                    &**client,
                    scope,
                    loaded.can_edit_roles(),
                    &assignments,
                    &providers,
                    &groups,
                )
                .await?,
            assignment_fields: if loaded.can_edit_roles() {
                self.assignment_fields(&**client, scope).await?
            } else {
                Html::EMPTY
            },
            can_edit_capabilities: loaded.can_edit(),
            control,
            show_assign: self.access.show_assign,
            role_search: self.access.role_search.clone(),
            roles,
            selected_roles: self.access.selected_roles.len(),
            show_effective: self.access.show_effective,
            effective_count: effective_keys.len(),
            permission_search: self.access.permission_search.clone(),
            permission_filter: {
                let mut toolbar = FilterToolbar::new(
                    "permission-filter",
                    vec![ToolbarControl::search(
                        "permission-search-input",
                        "search",
                        "Search permissions",
                        &self.access.permission_search,
                        "Search permissions",
                    )],
                );
                toolbar.event = "search_permissions";
                render(&toolbar)?
            },
            effective: group(
                &effective_keys
                    .iter()
                    .filter(|k| {
                        k.to_lowercase()
                            .contains(&self.access.permission_search.trim().to_lowercase())
                    })
                    .cloned()
                    .collect::<Vec<_>>(),
                &target.direct_allowed,
                "permissions",
            ),
            denied: group(
                &denied_keys
                    .iter()
                    .filter(|k| {
                        k.to_lowercase()
                            .contains(&self.access.permission_search.trim().to_lowercase())
                    })
                    .cloned()
                    .collect::<Vec<_>>(),
                &target.direct_denied,
                "denied",
            ),
            cap_control,
            cap_search: self.access.cap_search.clone(),
            caps: render(&bilibi_ui::PermissionChoices {
                options: caps,
                hidden_selected: Vec::new(),
                search: self.access.cap_search.clone(),
                selected_count: self.access.selected_caps.len(),
            })?,
            selected_caps: self.access.selected_caps.len(),
        })?;
        render(&Card {
            id: "user-roles-card",
            inner_class: "p-5 sm:p-6",
            role: "region",
            labelledby: "user-roles-heading",
            body,
            ..Card::default()
        })
    }

    fn dialog(&self, loaded: &Loaded) -> Result<Html> {
        let name = &loaded.account.name;
        Ok(match &self.pending {
            None => Html::EMPTY,
            Some(Pending::Delete) => render(&ConfirmDialog {
                id: "delete-user-confirm",
                consequence: &format!("{name}'s membership in this workspace will be removed."),
                detail: "Their portable identity and access to other workspaces remain available.",
                confirm: "Remove",
                working: "Removing…",
                confirm_event: "delete",
                cancel_event: "cancel_delete",
            })?,
            Some(Pending::Unlink(person)) => render(&ConfirmDialog {
                id: "unlink-person-confirm",
                consequence: &format!("{person} will be unlinked from {name}."),
                detail: "The person, affiliations and desks are kept and can be linked again. The account then has no workplace, and its open sessions end.",
                confirm: "Unlink",
                working: "Unlinking…",
                confirm_event: "unlink",
                cancel_event: "cancel_unlink",
            })?,
            Some(Pending::Authz(rule)) => {
                let (confirm, working, event) = rule.verb();
                render(&ConfirmDialog {
                    id: "user-authz-confirm",
                    consequence: &rule.consequence(),
                    detail: "This changes your own access, and it takes effect at once.",
                    confirm,
                    working,
                    confirm_event: event,
                    cancel_event: "cancel_authz",
                })?
            }
        })
    }

    /// The guard of the users list: the capability, never the signed-in
    /// account and never while already impersonating. Hidden, not disabled,
    /// when any of those fails, as on the source page.
    async fn impersonate(&self, scope: &Scope) -> Result<Html> {
        if self.id == scope.user()
            || scope.impersonator().is_some()
            || !self
                .pages
                .allowed(scope, crate::impersonation::IMPERSONATE)
                .await?
        {
            return Ok(Html::EMPTY);
        }
        render(&ActionLink {
            id: "user-impersonate",
            icon: "impersonate",
            href: &format!("/admin/impersonate/{}", self.id),
            method: "post",
            title: "Impersonate this user",
            body: Html::text("Impersonate"),
        })
    }

    async fn view(&mut self, scope: &Scope) -> Result<LiveBody> {
        let loaded = self.load(scope).await?;
        self.history = self.pages.allowed(scope, "admin.audit.log.list").await?;
        let account = &loaded.account;
        let subtitle = scope.tenant_name.clone();
        let header = render(&Header {
            title_actions: Html::EMPTY,
            title: Html::text(&account.name),
            subtitle: &subtitle,
            actions: render(&Actions {
                impersonate: self.impersonate(scope).await?,
                recovery: if loaded.can_edit()
                    && account.status == "active"
                    && account.email_verified
                    && scope.impersonator().is_none()
                {
                    render(&Button {
                        id: "user-send-recovery",
                        button_type: "button",
                        event: "send_recovery",
                        body: Html::text("Send recovery email"),
                        ..Button::default()
                    })?
                } else {
                    Html::EMPTY
                },
                back: render(&BackLink {
                    href: "/users",
                    id: "user-back",
                    title: "Back to users",
                    ..BackLink::default()
                })?,
            })?,
        })?;
        let details = self.details(scope, &loaded).await?;
        let access = self.access(scope, &loaded).await?;
        self.desk_person = if self.pages.allowed(scope, bilibi_company::VIEW).await? {
            account.links.person
        } else {
            None
        };
        let links = if self.desk_person.is_some() {
            Html::render(&DeskAnchor).map_err(|_| Error::Internal)?
        } else {
            Html::EMPTY
        };
        let danger = if loaded.can_delete && scope.user() != self.id {
            render(&Danger {
                button: render(&Button {
                    id: "user-delete",
                    button_type: "button",
                    event: "request_delete",
                    variant: ButtonVariant::Danger,
                    body: Html::text("Remove user"),
                    ..Button::default()
                })?,
            })?
        } else {
            Html::EMPTY
        };
        let body = render(&View {
            id: self.id.to_string(),
            flash: render(&FlashGroup {
                id: "flash-group",
                flash: self.flash.map(),
            })?,
            header,
            details,
            access,
            links,
            danger,
            dialog: self.dialog(&loaded)?,
        })?;
        Ok(LiveBody {
            html: render(&bilibi_ui::Page {
                id: "user-detail",
                variant: PageVariant::Detail,
                body,
                ..bilibi_ui::Page::default()
            })?
            .to_string(),
            rows: 1,
            ..LiveBody::default()
        })
    }

    /// The outcome of one in-place commit, on the fact that made it.
    fn fact_outcome(
        &mut self,
        field: &str,
        label: &str,
        submitted: &str,
        result: std::result::Result<(), AccountError>,
    ) -> Result<()> {
        let status = match result {
            Ok(()) => CommitStatus::Saved,
            Err(AccountError::Validation(errors)) => CommitStatus::refusal(
                label,
                submitted,
                &errors
                    .iter()
                    .map(|e| e.message.as_str())
                    .collect::<Vec<_>>(),
            ),
            Err(AccountError::Conflict) => CommitStatus::refusal(
                label,
                submitted,
                &["has changed since this edit began; reload before trying again"],
            ),
            Err(AccountError::NotFound) => {
                CommitStatus::refusal(label, submitted, &["is not in this workspace"])
            }
            Err(AccountError::Access(Error::Unauthorized)) => return Err(Error::Unauthorized),
            // The host sends the browser to confirm with a code and back.
            Err(AccountError::Access(error @ Error::StepUpRequired { .. })) => return Err(error),
            Err(AccountError::Access(Error::Forbidden)) => {
                self.statuses.clear_saved();
                self.flash.error(FORBIDDEN);
                CommitStatus::Error(FORBIDDEN.into())
            }
            Err(AccountError::Outranked) => {
                self.statuses.clear_saved();
                self.flash.error(OUTRANKED);
                CommitStatus::Error(OUTRANKED.into())
            }
            Err(AccountError::Impersonating) => {
                self.statuses.clear_saved();
                self.flash.error(IMPERSONATING);
                CommitStatus::Error(IMPERSONATING.into())
            }
            Err(AccountError::Access(Error::Conflict)) => CommitStatus::refusal(
                label,
                submitted,
                &["changed while you were editing; reload before trying again"],
            ),
            Err(_) => CommitStatus::failure(),
        };
        self.statuses.put(field, status);
        Ok(())
    }

    async fn save_field(&mut self, scope: &Scope, value: &Value) -> Result<()> {
        let field = value["field"].as_str().ok_or(Error::Invalid)?.to_owned();
        if value["id"].as_str() != Some(&self.id.to_string()) {
            return Err(Error::Invalid);
        }
        let submitted = value["value"].as_str().ok_or(Error::Invalid)?.to_owned();
        let version: i64 = value["version"]
            .as_str()
            .and_then(|v| v.parse().ok())
            .ok_or(Error::Invalid)?;
        let users = self.pages.users.clone();
        match field.as_str() {
            "name" => {
                let kind = AccountField::Name;
                let result = users
                    .update_account(scope, self.id, version, kind, &submitted)
                    .await
                    .map(|_| ());
                self.fact_outcome(&field, "Name", &submitted, result)
            }
            "person" => {
                let person_id = |value: &str| {
                    if value.is_empty() {
                        Ok(None)
                    } else {
                        value
                            .parse::<i64>()
                            .ok()
                            .and_then(|id| PersonId::new(id).ok())
                            .map(Some)
                            .ok_or(AccountError::NotFound)
                    }
                };
                let original = value["original"].as_str().ok_or(Error::Invalid)?;
                let expected = person_id(original).map_err(|_| Error::Invalid)?;
                let result = match person_id(submitted.trim()) {
                    Ok(person) => match users
                        .set_person_link(scope, self.id, person, Some(expected))
                        .await
                    {
                        Err(LinkError::Access(Error::Conflict)) => return Err(Error::Conflict),
                        result => result.map(|_| ()).map_err(AccountError::from),
                    },
                    Err(error) => Err(error),
                };
                let label = self.choice_label(scope, "person", &submitted).await;
                self.fact_outcome("person", "Person", &label, result)
            }
            _ => Err(Error::Invalid),
        }
    }

    /// What the operator chose, as a refusal names it: the option's label when
    /// the owner lists it, and the raw value for anything else.
    async fn choice_label(&self, scope: &Scope, field: &str, submitted: &str) -> String {
        if field == "person" {
            if let Ok(Some(persons)) = self.pages.person_choices(scope).await {
                if let Some(choice) = persons.iter().find(|p| p.id.get().to_string() == submitted) {
                    return choice.label.clone();
                }
            }
        }
        submitted.to_owned()
    }

    async fn set_role(
        &self,
        scope: &Scope,
        credential: &str,
        event: &str,
        role: i64,
        assigned: bool,
        bounds: &Assignment,
    ) -> Result<()> {
        let mut input = json!({"assignment":bounds});
        input["user_id"] = json!(self.id);
        input["role_id"] = json!(role);
        input["assigned"] = json!(assigned);
        let call = Call {
            scope,
            credential,
            service: "core/user.pages",
            event,
            contract: bilibi_authz_agent::PRINCIPAL_ROLE_SET,
        };
        match self.pages.contracts.submit(&call, input).await {
            Ok(_) => Ok(()),
            Err(ContractError::Backend(error)) => Err(error),
            Err(ContractError::Forbidden) => Err(Error::Forbidden),
            Err(ContractError::InvalidInput(fields)) => Err(Error::Refused(
                fields
                    .values()
                    .flatten()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" "),
            )),
            Err(_) => Err(Error::Invalid),
        }
    }

    async fn authz(
        &mut self,
        scope: &Scope,
        credential: &str,
        event: &str,
        rule: AuthzRule,
    ) -> Result<()> {
        let grants = self.pages.grants.clone();
        let forbidden = |rule: &AuthzRule| match rule {
            AuthzRule::RemoveRole { .. } | AuthzRule::SetRoles { .. } => MANAGE_ROLES_FORBIDDEN,
            _ => MANAGE_CAPS_FORBIDDEN,
        };
        let outcome = match &rule {
            AuthzRule::SetRoles { ids, bounds } => {
                async {
                    for id in ids {
                        self.set_role(scope, credential, event, *id, true, bounds)
                            .await?;
                    }
                    Ok(())
                }
                .await
            }
            AuthzRule::RemoveRole { id, company, .. } => {
                self.set_role(
                    scope,
                    credential,
                    event,
                    *id,
                    false,
                    &Assignment {
                        company_id: *company,
                        ..Assignment::default()
                    },
                )
                .await
            }
            AuthzRule::Deny(key) => {
                grants
                    .set_direct(scope, self.id, key, Some(Direct::Deny))
                    .await
            }
            AuthzRule::RemoveGrant(key) | AuthzRule::RemoveDenial(key) => {
                grants.set_direct(scope, self.id, key, None).await
            }
        };
        match outcome {
            // Base Step-up: the host sends the browser to confirm and back.
            Err(error @ Error::StepUpRequired { .. }) => return Err(error),
            Ok(()) => self.flash.success(match &rule {
                AuthzRule::SetRoles { .. } => "Role assignments updated.".into(),
                AuthzRule::RemoveRole { name, .. } => format!("The {name} role was removed."),
                AuthzRule::Deny(key) => format!("{key} is denied for this user."),
                AuthzRule::RemoveGrant(key) => format!("The direct grant of {key} was removed."),
                AuthzRule::RemoveDenial(key) => format!("The deny rule for {key} was removed."),
            }),
            Err(Error::Unauthorized) => return Err(Error::Unauthorized),
            Err(Error::Forbidden) => self.flash.error(forbidden(&rule)),
            Err(_) => self
                .flash
                .error("The change was not saved. Reload the page and try again."),
        }
        Ok(())
    }

    /// A change to another account commits on click; one to the acting
    /// account's own is held and confirmed first, since the pickers offer
    /// only what the acting account holds and so may not offer it back.
    async fn apply_or_hold(
        &mut self,
        scope: &Scope,
        credential: &str,
        event: &str,
        rule: AuthzRule,
    ) -> Result<()> {
        if scope.user() == self.id {
            self.flash.clear();
            self.pending = Some(Pending::Authz(rule));
            Ok(())
        } else {
            self.authz(scope, credential, event, rule).await
        }
    }

    async fn guard_write(
        &mut self,
        scope: &Scope,
        capability: &str,
        forbidden: &str,
    ) -> Result<bool> {
        if !self.pages.allowed(scope, capability).await? {
            self.flash.error(forbidden);
            return Ok(false);
        }
        Ok(true)
    }

    async fn handle(&mut self, scope: &Scope, credential: &str, value: &Value) -> Result<()> {
        match event(value)? {
            "save_field" => self.save_field(scope, value).await?,
            "clear-flash" => self.flash.dismiss(value.get("key").and_then(Value::as_str)),
            "send_recovery" => {
                match self
                    .pages
                    .users
                    .send_password_recovery(scope, self.id)
                    .await
                {
                    Ok(()) => self.flash.success("Recovery email requested."),
                    Err(AccountError::Access(Error::Unauthorized)) => {
                        return Err(Error::Unauthorized)
                    }
                    Err(_) => self.flash.error("Recovery email could not be requested."),
                }
            }
            "request_delete" => {
                if !self.pages.allowed(scope, DELETE).await? {
                    self.flash
                        .error("You do not have permission to remove users.");
                } else if scope.user() == self.id {
                    self.flash.error("You cannot remove your own membership.");
                } else {
                    self.flash.clear();
                    self.pending = Some(Pending::Delete);
                }
            }
            "cancel_delete" | "cancel_unlink" | "cancel_authz" => self.pending = None,
            "delete" => {
                if matches!(self.pending, Some(Pending::Delete)) {
                    self.pending = None;
                    let name = self.load(scope).await?.account.name;
                    match self.pages.users.remove_membership(scope, self.id).await {
                        Ok(()) => {
                            self.flash
                                .success(format!("{name}'s membership was removed."));
                            return Ok(());
                        }
                        Err(AccountError::Access(Error::Unauthorized)) => {
                            return Err(Error::Unauthorized);
                        }
                        Err(AccountError::Access(Error::Forbidden)) => self
                            .flash
                            .error("You do not have permission to remove users."),
                        Err(AccountError::OwnAccount) => {
                            self.flash.error("You cannot remove your own membership.")
                        }
                        Err(AccountError::Outranked) => self.flash.error(format!(
                            "You cannot remove {name}: they have permissions you do not have."
                        )),
                        Err(AccountError::Impersonating) => self.flash.error(IMPERSONATING),
                        Err(_) => self.flash.error(format!(
                            "{name} was not removed. Reload the page and try again."
                        )),
                    }
                }
            }
            "request_unlink" => {
                if self.guard_write(scope, UPDATE, FORBIDDEN).await? {
                    let loaded = self.load(scope).await?;
                    if let Some((person, _)) = loaded.person {
                        self.flash.clear();
                        self.pending = Some(Pending::Unlink(person));
                    }
                }
            }
            "unlink" => {
                if let Some(Pending::Unlink(person)) = self.pending.take() {
                    let name = self.load(scope).await?.account.name;
                    match self.pages.users.unlink(scope, self.id).await {
                        Ok(_) => self
                            .flash
                            .success(format!("{person} was unlinked from {name}.")),
                        Err(LinkError::Access(Error::Unauthorized)) => {
                            return Err(Error::Unauthorized);
                        }
                        Err(LinkError::Access(Error::Forbidden)) => self.flash.error(FORBIDDEN),
                        Err(_) => self.flash.error(format!(
                            "{person} was not unlinked. Reload the page and try again."
                        )),
                    }
                }
            }
            "toggle_assign_roles" => {
                if !self.access.show_assign {
                    self.access.assignment_company.clear();
                    self.access.assignment_query.clear();
                    self.access.assignment_from.clear();
                    self.access.assignment_until.clear();
                    self.access.assignment_purpose.clear();
                    self.access.selected_roles.clear();
                }
                self.access.show_assign = !self.access.show_assign;
            }
            "search_roles" => self.access.role_search = text(value, "value"),
            "pick_assignment_company" => {
                self.access.assignment_company = super::picked_company(value)?.get().to_string();
                self.access.assignment_query.clear();
            }
            "change_assignment_company" => self.access.assignment_company.clear(),
            "select_roles" => {
                self.access.selected_roles = list(value, "role_ids[]");
                self.read_assignment(value);
            }
            "assignment_grid" => {
                self.assignment_columns.handle(
                    &bilibi_ui::params::string_values_checked(
                        value,
                        bilibi_ui::params::FIELD_LIMIT,
                    )
                    .map_err(Error::refused)?,
                );
            }
            "assign_selected_roles" => {
                self.read_assignment(value);
                let bounds = match self.assignment() {
                    Ok(bounds) => bounds,
                    Err(_) => {
                        self.flash.error("Choose a company, valid dates in order and a purpose of at most 1000 characters.");
                        return Ok(());
                    }
                };
                if self
                    .guard_write(scope, UPDATE_ROLE, MANAGE_ROLES_FORBIDDEN)
                    .await?
                {
                    let chosen = {
                        let posted = list(value, "role_ids[]");
                        if posted.is_empty() {
                            self.access.selected_roles.clone()
                        } else {
                            posted
                        }
                    };
                    let ids: Vec<i64> = chosen.iter().filter_map(|id| id.parse().ok()).collect();
                    if !ids.is_empty() {
                        if scope.user() == self.id {
                            self.pending =
                                Some(Pending::Authz(AuthzRule::SetRoles { ids, bounds }));
                            return Ok(());
                        }
                        let mut assigned = 0;
                        let mut refused = false;
                        for id in &ids {
                            match self
                                .set_role(
                                    scope,
                                    credential,
                                    "assign_selected_roles",
                                    *id,
                                    true,
                                    &bounds,
                                )
                                .await
                            {
                                Ok(()) => assigned += 1,
                                Err(Error::Unauthorized) => return Err(Error::Unauthorized),
                                // Base Step-up: confirm, come back, assign again.
                                Err(error @ Error::StepUpRequired { .. }) => return Err(error),
                                Err(error) => {
                                    self.flash.error(error.to_string());
                                    refused = true;
                                }
                            }
                        }
                        if refused && assigned == 0 {
                            // Keep the entered bounds and the owner's refusal.
                        } else {
                            self.flash.success(if assigned == 1 {
                                "Assigned 1 role.".to_owned()
                            } else {
                                format!("Assigned {assigned} roles.")
                            });
                            self.access.selected_roles.clear();
                            self.access.show_assign = false;
                        }
                    }
                }
            }
            "confirm_role_assignments" => {
                if let Some(Pending::Authz(rule @ AuthzRule::SetRoles { .. })) = self.pending.take()
                {
                    if self
                        .guard_write(scope, UPDATE_ROLE, MANAGE_ROLES_FORBIDDEN)
                        .await?
                    {
                        self.authz(scope, credential, "confirm_role_assignments", rule)
                            .await?;
                    }
                }
            }
            "request_remove_role" => {
                if self
                    .guard_write(scope, UPDATE_ROLE, MANAGE_ROLES_FORBIDDEN)
                    .await?
                {
                    let key = text(value, "role_id");
                    let (id, company) = key.split_once('/').ok_or(Error::Invalid)?;
                    let id: i64 = id.parse().map_err(|_| Error::Invalid)?;
                    let company = if company.is_empty() {
                        None
                    } else {
                        Some(company.parse::<i64>().map_err(|_| Error::Invalid)?)
                    };
                    let client = self.pages.db.get().await?;
                    let assigned =
                        bilibi_authz::roles::assignments(&**client, scope, self.id).await?;
                    drop(client);
                    if let Some(role) = assigned
                        .into_iter()
                        .find(|r| r.role.id == id && r.bounds.company_id == company)
                    {
                        self.apply_or_hold(
                            scope,
                            credential,
                            "request_remove_role",
                            AuthzRule::RemoveRole {
                                id,
                                name: role.role.name,
                                company,
                            },
                        )
                        .await?;
                    }
                }
            }
            "remove_role" => {
                if let Some(Pending::Authz(rule @ AuthzRule::RemoveRole { .. })) =
                    self.pending.take()
                {
                    if self
                        .guard_write(scope, UPDATE_ROLE, MANAGE_ROLES_FORBIDDEN)
                        .await?
                    {
                        self.authz(scope, credential, event(value)?, rule).await?;
                    }
                }
            }
            "toggle_effective_permissions" => {
                self.access.show_effective = !self.access.show_effective
            }
            "search_permissions" => {
                let fields = bilibi_ui::params::form_values_checked(
                    value,
                    "filters",
                    bilibi_ui::params::FIELD_LIMIT,
                )
                .map_err(Error::refused)?;
                self.access.permission_search = fields.get("search").cloned().unwrap_or_default();
            }
            "search_capabilities" => self.access.cap_search = text(value, "value"),
            "select_capabilities" => self.access.selected_caps = list(value, "capability_keys[]"),
            "add_selected_capabilities" => {
                if self
                    .guard_write(scope, UPDATE, MANAGE_CAPS_FORBIDDEN)
                    .await?
                {
                    let chosen = {
                        let posted = list(value, "capability_keys[]");
                        if posted.is_empty() {
                            self.access.selected_caps.clone()
                        } else {
                            posted
                        }
                    };
                    if !chosen.is_empty() {
                        let client = self.pages.db.get().await?;
                        let actor =
                            bilibi_authz::effective_capabilities_of(&**client, scope, scope.user())
                                .await?;
                        drop(client);
                        if chosen.iter().any(|key| !actor.reaches(key, Reach::All)) {
                            self.flash
                                .error("You cannot grant capabilities you do not hold.");
                        } else {
                            let mut granted = 0;
                            for key in &chosen {
                                match self
                                    .pages
                                    .grants
                                    .set_direct(
                                        scope,
                                        self.id,
                                        key,
                                        Some(Direct::Allow(Reach::All)),
                                    )
                                    .await
                                {
                                    Ok(()) => granted += 1,
                                    Err(Error::Unauthorized) => return Err(Error::Unauthorized),
                                    // Base Step-up: confirm, come back, grant again.
                                    Err(error @ Error::StepUpRequired { .. }) => return Err(error),
                                    Err(_) => {}
                                }
                            }
                            if granted == 0 {
                                self.flash.error(MANAGE_CAPS_FORBIDDEN);
                            } else {
                                self.flash.success(if granted == 1 {
                                    "Granted 1 capability.".to_owned()
                                } else {
                                    format!("Granted {granted} capabilities.")
                                });
                                self.access.selected_caps.clear();
                            }
                        }
                    }
                }
            }
            "request_deny_capability" => {
                if self
                    .guard_write(scope, UPDATE, MANAGE_CAPS_FORBIDDEN)
                    .await?
                {
                    let key = text(value, "capability_key");
                    let client = self.pages.db.get().await?;
                    let target =
                        bilibi_authz::effective_capabilities_of(&**client, scope, self.id).await?;
                    drop(client);
                    if target.allowed.contains(key.as_str()) {
                        self.apply_or_hold(
                            scope,
                            credential,
                            "request_deny_capability",
                            AuthzRule::Deny(key),
                        )
                        .await?;
                    }
                }
            }
            "deny_capability" => {
                if let Some(Pending::Authz(rule @ AuthzRule::Deny(_))) = self.pending.take() {
                    if self
                        .guard_write(scope, UPDATE, MANAGE_CAPS_FORBIDDEN)
                        .await?
                    {
                        self.authz(scope, credential, event(value)?, rule).await?;
                    }
                }
            }
            "request_remove_capability" => {
                if self
                    .guard_write(scope, UPDATE, MANAGE_CAPS_FORBIDDEN)
                    .await?
                {
                    let key = text(value, "capability_key");
                    let client = self.pages.db.get().await?;
                    let target =
                        bilibi_authz::effective_capabilities_of(&**client, scope, self.id).await?;
                    drop(client);
                    let rule = if target.direct_allowed.contains(&key) {
                        Some(AuthzRule::RemoveGrant(key))
                    } else if target.direct_denied.contains(&key) {
                        Some(AuthzRule::RemoveDenial(key))
                    } else {
                        None
                    };
                    if let Some(rule) = rule {
                        self.apply_or_hold(scope, credential, "request_remove_capability", rule)
                            .await?;
                    }
                }
            }
            "remove_capability" => {
                if let Some(Pending::Authz(
                    rule @ (AuthzRule::RemoveGrant(_) | AuthzRule::RemoveDenial(_)),
                )) = self.pending.take()
                {
                    if self
                        .guard_write(scope, UPDATE, MANAGE_CAPS_FORBIDDEN)
                        .await?
                    {
                        self.authz(scope, credential, event(value)?, rule).await?;
                    }
                }
            }
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }
}

impl LiveState<Scope, Error> for Detail {
    fn update<'a>(&'a mut self, request: LiveRequest<'a, Scope>) -> LiveFuture<'a, Error> {
        Box::pin(async move {
            if let LiveInput::Event(value) = request.input {
                if event(value)? == "delete" {
                    self.handle(request.scope, request.credential, value)
                        .await?;
                    if self
                        .pages
                        .users
                        .account(request.scope, self.id)
                        .await
                        .is_err()
                    {
                        // The account is gone: back to the list, as `push_navigate` does.
                        return Ok(LiveBody {
                            redirect: Some("/users".into()),
                            flash: self.flash.take(),
                            ..LiveBody::default()
                        });
                    }
                } else {
                    self.handle(request.scope, request.credential, value)
                        .await?;
                }
            }
            self.view(request.scope).await
        })
    }
    fn panels(&self) -> Vec<PanelSpec> {
        let mut panels = Vec::new();
        if let Some(person) = self.desk_person {
            panels.push(PanelSpec {
                id: "user-desks".into(),
                embed: "user.person-desks".into(),
                path: HashMap::from([
                    ("panel".into(), "user-desks".into()),
                    ("id".into(), person.get().to_string()),
                ]),
                query: HashMap::new(),
            });
        }
        if self.history {
            panels.push(PanelSpec {
                id: "user-record-history".into(),
                embed: "audit.record-history".into(),
                path: HashMap::from([
                    ("panel".into(), "user-record-history".into()),
                    ("kinds".into(), "user".into()),
                    ("id".into(), self.id.to_string()),
                ]),
                query: HashMap::new(),
            });
        }
        panels
    }
}

#[derive(Template)]
#[template(
    source = r#"<div id="user-desks-slot" data-live-ignore><div id="user-desks"></div></div>"#,
    ext = "html"
)]
struct DeskAnchor;
