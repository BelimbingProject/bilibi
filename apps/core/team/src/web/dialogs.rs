//! The create dialog, the add-members review (also opened by a drop or by
//! "Add to team"), the remove confirmation, the role dialog and its
//! take-away confirmation, and the record's write events. Each write calls
//! its operation contract.
use super::{
    form_values, render, summary, text, AddDraft, CreateDraft, Picker, Record, RoleDraft, State,
};
use crate::{operations, MANAGE};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn button(label: &str, event: &str) -> Result<Html> {
    render(Button {
        body: Html::text(label),
        event,
        button_type: "button",
        ..Button::default()
    })
}
fn submit(label: &str, working: &str) -> Result<Html> {
    render(Button {
        body: Html::text(label),
        button_type: "submit",
        working,
        ..Button::default()
    })
}
fn errors(errors: &BTreeMap<String, Vec<String>>, key: &str) -> Vec<String> {
    errors.get(key).cloned().unwrap_or_default()
}
/// The shared staff picker of both dialogs.
fn picker(id: &str, name: &str, label: &str, picker: &Picker) -> Result<Html> {
    render(
        SearchMultiSelect::new(
            id,
            name,
            label,
            "staff_search",
            "staff_pick",
            "staff_remove",
        )
        .query(&picker.query)
        .selected(
            picker
                .selected
                .iter()
                .map(|(id, name)| SearchMultiOption::new(id.to_string(), name))
                .collect(),
        )
        .results(
            picker
                .hits
                .iter()
                .map(|hit| SearchMultiOption::new(hit.id.to_string(), &hit.name))
                .collect(),
        )
        .wrap_chips(),
    )
}
pub(super) fn create(
    state: &State,
    d: &CreateDraft,
    words: &bilibi_grid::RecordWords,
) -> Result<Html> {
    let value = |k: &str| d.values.get(k).cloned().unwrap_or_default();
    let field = |k: &str, label: &str| {
        Field::in_form("team", k)
            .id(format!("team-new-{}", k.replace('_', "-")))
            .label(words.field(k, label))
            .value(value(k))
            .errors(errors(&d.errors, k))
    };
    let mut parents = vec![Choice::new("No parent team", "")];
    parents.extend(
        state
            .team_choices
            .iter()
            .map(|(id, name)| Choice::new(name, id.to_string())),
    );
    let mut body = summary("team-new", &d.errors)?;
    body.push(&render(
        field("name", "Label")
            .hint("Example: Sales team")
            .required()
            .attr("maxlength", "200"),
    )?);
    body.push(&render(
        field("kind", "Kind")
            .kind(InputKind::Select)
            .options(state.kinds.clone())
            .required(),
    )?);
    body.push(&render(
        Combobox::new("team-new-parent-id", "team[parent_id]")
            .label(words.field("parent_id", "Parent team"))
            .value(value("parent_id"))
            .options(parents)
            .placeholder("No parent team")
            .errors(errors(&d.errors, "parent_id")),
    )?);
    body.push(&render(
        field("company_id", "Company")
            .kind(InputKind::Select)
            .options(d.companies.clone()),
    )?);
    body.push(&picker(
        "team-new-leads",
        "team[leads][]",
        "Leads",
        &d.leads,
    )?);
    let mut actions = button("Cancel", "close_new")?;
    actions.push(&submit("Create team", "Creating…")?);
    render(Modal {
        id: "team-new-dialog",
        title: "New team",
        cancel_event: "close_new",
        body: render(Form {
            id: "team-create",
            event: "create",
            body,
            actions,
            ..Form::default()
        })?,
        ..Modal::default()
    })
}
pub(super) fn add(state: &State, d: &AddDraft) -> Result<Html> {
    let mut body = summary("team-add", &d.errors)?;
    let team_name = state.record.as_ref().map(|r| r.team.name.clone());
    if team_name.is_none() {
        body.push(&render(
            Combobox::new("team-add-team-id", "add[team_id]")
                .label("Team")
                .value(d.team.map(|t| t.to_string()).unwrap_or_default())
                .options(
                    state
                        .team_choices
                        .iter()
                        .map(|(id, name)| Choice::new(name, id.to_string()))
                        .collect(),
                )
                .placeholder("Choose a team")
                .errors(errors(&d.errors, "team_id")),
        )?);
    }
    body.push(&picker(
        "team-add-people",
        "add[user_ids][]",
        "People",
        &d.people,
    )?);
    let mut actions = button("Cancel", "close_add")?;
    actions.push(&submit("Add to team", "Adding…")?);
    let title = match &team_name {
        Some(name) => format!("Add members to {name}"),
        None => "Add to team".into(),
    };
    render(Modal {
        id: "team-add-dialog",
        title: &title,
        cancel_event: "close_add",
        body: render(Form {
            id: "team-add",
            event: "add_members",
            body,
            actions,
            ..Form::default()
        })?,
        ..Modal::default()
    })
}
pub(super) fn remove(state: &State) -> Result<Html> {
    let team = state
        .record
        .as_ref()
        .map(|r| r.team.name.as_str())
        .unwrap_or("the team");
    let who = match state.removing.len() {
        1 => state
            .record
            .as_ref()
            .and_then(|r| r.members.iter().find(|m| m.user_id == state.removing[0]))
            .map(|m| m.name.clone())
            .unwrap_or_else(|| "1 person".into()),
        n => format!("{n} people"),
    };
    let consequence = format!("Remove {who} from {team}?");
    render(ConfirmDialog {
        id: "team-remove-dialog",
        consequence: &consequence,
        detail: "They leave the team now. The team's history keeps a record of it.",
        confirm: "Remove",
        working: "Removing…",
        confirm_event: "confirm_remove",
        cancel_event: "cancel_remove",
    })
}
/// "Add role": the roles the reader may give, with what giving one means.
pub(super) fn add_role(record: &Record, d: &RoleDraft) -> Result<Html> {
    let team = &record.team.name;
    let mut body = summary("team-role", &d.errors)?;
    let mut actions = button("Cancel", "close_role")?;
    if record.role_choices.is_empty() {
        body.push(&render(EmptyState::missing(
            "No role you can give",
            "You can give a team only a role whose permissions you hold yourself. A full-access role goes to a person, never a team.",
        ))?);
    } else {
        let hint = match &record.company_name {
            Some(company) if record.team.company_id.is_some() => format!(
                "Members hold the role's permissions in {company} only, for as long as they are in {team}. People added later get it too."
            ),
            _ => format!(
                "Members hold the role's permissions for as long as they are in {team}. People added later get it too."
            ),
        };
        body.push(&render(
            Combobox::new("team-role-role-id", "role[role_id]")
                .label("Role")
                .value(d.role.clone())
                .options(
                    record
                        .role_choices
                        .iter()
                        .map(|r| Choice::new(&r.name, r.id.to_string()))
                        .collect(),
                )
                .placeholder("Choose a role")
                .hint(hint)
                .errors(errors(&d.errors, "role_id")),
        )?);
        actions.push(&submit("Give role", "Giving…")?);
    }
    let title = format!("Give {team} a role");
    render(Modal {
        id: "team-role-dialog",
        title: &title,
        cancel_event: "close_role",
        body: render(Form {
            id: "team-role",
            event: "add_role",
            body,
            actions,
            ..Form::default()
        })?,
        ..Modal::default()
    })
}
/// The take-away confirmation: who loses what, and when.
pub(super) fn remove_role(record: &Record, role: i64) -> Result<Html> {
    let name = record
        .roles
        .iter()
        .find(|r| r.id == role)
        .map(|r| r.name.as_str())
        .unwrap_or("this role");
    let consequence = format!("Take {name} away from {}?", record.team.name);
    let detail = match record.member_total {
        0 => "Nobody is in the team yet, so nobody loses anything now.".to_owned(),
        1 => "Its member loses the role's permissions at their next action, unless they hold the role another way.".to_owned(),
        n => format!("Its {n} members lose the role's permissions at their next action, unless they hold the role another way."),
    };
    render(ConfirmDialog {
        id: "team-role-remove-dialog",
        consequence: &consequence,
        detail: &detail,
        confirm: "Take away",
        working: "Taking away…",
        confirm_event: "confirm_remove_role",
        cancel_event: "cancel_remove_role",
    })
}

impl State {
    fn picker_mut(&mut self) -> Option<&mut Picker> {
        if let Some(d) = &mut self.add {
            return Some(&mut d.people);
        }
        self.create.as_mut().map(|d| &mut d.leads)
    }
    /// Fills the open dialog's picker; an empty query offers the first staff.
    pub(super) async fn search_staff(&mut self, scope: &Scope, query: String) -> Result<()> {
        let hits = match self.pages.users.search_staff(scope, MANAGE, &query).await {
            Ok(hits) => hits,
            Err(Error::Forbidden) => {
                self.refuse("You do not have permission to choose staff for a team.");
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if let Some(p) = self.picker_mut() {
            p.query = query;
            p.hits = hits;
        }
        Ok(())
    }
    fn refuse(&mut self, message: &str) {
        self.flash = Some((StatusKind::Error, message.into()));
    }
    pub(super) async fn dialog_event(
        &mut self,
        scope: &Scope,
        credential: &str,
        event: &str,
        v: &Value,
    ) -> Result<()> {
        // Every write event checks the reader may change teams before any work.
        let writes = [
            "new",
            "create",
            "add_open",
            "add_members",
            "staff_search",
            "staff_pick",
            "save_field",
            "set_lead",
            "remove_one",
            "remove_selected",
            "confirm_remove",
            "role_open",
            "add_role",
            "remove_role",
            "confirm_remove_role",
        ];
        if writes.contains(&event) && !self.can_create {
            self.refuse("You do not have permission to change teams.");
            return Ok(());
        }
        match event {
            "new" => {
                let companies = {
                    let client = self.pages.db.get().await?;
                    let mut companies = vec![Choice::new("No company", "")];
                    companies.extend(
                        self.pages
                            .parties
                            .reachable_organisations(&**client, scope)
                            .await?
                            .into_iter()
                            .filter(|c| !c.archived)
                            .map(|c| Choice::new(c.name, c.id.get().to_string())),
                    );
                    companies
                };
                let kind = self
                    .kinds
                    .first()
                    .map(|c| c.value.clone())
                    .unwrap_or_default();
                let mut values = std::collections::HashMap::from([("kind".to_owned(), kind)]);
                if let Some(parent) = v["parent"].as_str().filter(|p| p.parse::<i64>().is_ok()) {
                    values.insert("parent_id".into(), parent.into());
                }
                self.add = None;
                self.create = Some(CreateDraft {
                    values,
                    companies,
                    ..CreateDraft::default()
                });
                self.search_staff(scope, String::new()).await?;
            }
            "close_new" => self.create = None,
            "add_open" => {
                self.create = None;
                self.add = Some(AddDraft {
                    team: self.id,
                    ..AddDraft::default()
                });
                self.search_staff(scope, String::new()).await?;
            }
            "close_add" => self.add = None,
            "staff_search" => {
                let query: String = text(v, "query").trim().chars().take(100).collect();
                self.search_staff(scope, query).await?;
            }
            "staff_pick" | "staff_remove" => {
                let Ok(id) = text(v, "value").parse::<i64>() else {
                    return Ok(());
                };
                if let Some(p) = self.picker_mut() {
                    if let Some(at) = p.selected.iter().position(|(s, _)| *s == id) {
                        p.selected.remove(at);
                    } else if event == "staff_pick" && p.selected.len() < crate::MAX_BATCH {
                        if let Some(hit) = p.hits.iter().find(|h| h.id == id) {
                            p.selected.push((hit.id, hit.name.clone()));
                        }
                    }
                }
            }
            "create" => {
                let values = form_values(v, "team")?;
                let Some(mut draft) = self.create.take() else {
                    return Ok(());
                };
                let get = |k: &str| {
                    values
                        .get(k)
                        .map(|s| s.trim().to_owned())
                        .unwrap_or_default()
                };
                let number = |k: &str| get(k).parse::<i64>().ok();
                let input = json!({"name":get("name"),"kind":get("kind"),"parent_id":number("parent_id"),"company_id":number("company_id"),
                    "leads":draft.leads.selected.iter().map(|(id,_)|*id).collect::<Vec<_>>()});
                draft.values = values;
                match self
                    .submit(scope, credential, "create", operations::CREATE, input)
                    .await?
                {
                    Ok(result) => {
                        if let Some(id) = result["result"]["id"].as_i64() {
                            self.redirect = Some((format!("/teams/{id}"), "Team created.".into()));
                        }
                    }
                    Err(fields) => {
                        draft.errors = fields;
                        self.create = Some(draft);
                    }
                }
            }
            "add_members" => {
                let values = form_values(v, "add")?;
                let Some(mut draft) = self.add.take() else {
                    return Ok(());
                };
                let team = self
                    .id
                    .or_else(|| values.get("team_id").and_then(|t| t.parse().ok()));
                draft.team = team;
                let Some(team) = team else {
                    draft.errors =
                        [("team_id".to_owned(), vec!["Choose a team.".to_owned()])].into();
                    self.add = Some(draft);
                    return Ok(());
                };
                let users: Vec<i64> = draft.people.selected.iter().map(|(id, _)| *id).collect();
                let input = json!({"team_id":team,"user_ids":users});
                match self
                    .submit(
                        scope,
                        credential,
                        "add_members",
                        operations::ADD_MEMBERS,
                        input,
                    )
                    .await?
                {
                    Ok(result) => {
                        let message = added_message(&result["result"]);
                        if self.id == Some(team) {
                            self.flash = Some((StatusKind::Success, message));
                        } else {
                            // Land where the new members are.
                            self.redirect = Some((format!("/teams/{team}"), message));
                        }
                    }
                    Err(fields) => {
                        draft.errors = fields;
                        self.add = Some(draft);
                    }
                }
            }
            "save_field" => {
                let field = text(v, "field").to_owned();
                let value = text(v, "value").to_owned();
                let version = text(v, "version")
                    .parse::<i64>()
                    .map_err(|_| Error::Invalid)?;
                let id = self.id.ok_or(Error::Invalid)?;
                let patch = match field.as_str() {
                    "name" | "kind" => json!({ field.clone(): value }),
                    "parent_id" | "company_id" => {
                        json!({ field.clone(): value.parse::<i64>().ok() })
                    }
                    _ => return Err(Error::Invalid),
                };
                let saved = self
                    .submit(
                        scope,
                        credential,
                        "save_field",
                        operations::UPDATE,
                        json!({"id":id,"version":version,"patch":patch}),
                    )
                    .await?;
                let status = match saved {
                    Ok(_) => {
                        self.flash = None;
                        CommitStatus::Saved
                    }
                    Err(fields) => {
                        // The reason shows on the fact itself, once.
                        self.flash = None;
                        let messages: Vec<String> = fields.values().flatten().cloned().collect();
                        CommitStatus::Error(messages.join(" "))
                    }
                };
                self.commits.put(&field, status);
            }
            "set_lead" => {
                let id = self.id.ok_or(Error::Invalid)?;
                let (user, lead) = text(v, "lead").split_once(':').ok_or(Error::Invalid)?;
                let user = user.parse::<i64>().map_err(|_| Error::Invalid)?;
                let input = json!({"team_id":id,"user_id":user,"lead":lead == "1"});
                if let Ok(result) = self
                    .submit(scope, credential, "set_lead", operations::SET_LEAD, input)
                    .await?
                {
                    self.flash = Some((
                        StatusKind::Success,
                        result["result"]["summary"]
                            .as_str()
                            .unwrap_or("Saved.")
                            .to_owned()
                            + ".",
                    ));
                }
            }
            "remove_one" => {
                if let Ok(user) = text(v, "user").parse::<i64>() {
                    self.removing = vec![user];
                }
            }
            "remove_selected" => {
                if self.selection.all_matching {
                    self.refuse("Tick the members to remove on this page; removing every member at once is not offered.");
                } else {
                    self.removing = self
                        .selection
                        .selected
                        .iter()
                        .filter_map(|k| k.parse().ok())
                        .collect();
                    if self.removing.is_empty() {
                        self.refuse("Tick the members to remove first.");
                    }
                }
            }
            "cancel_remove" => self.removing.clear(),
            "role_open" => {
                self.create = None;
                self.add = None;
                self.role_add = Some(RoleDraft::default());
            }
            "close_role" => self.role_add = None,
            "add_role" => {
                let id = self.id.ok_or(Error::Invalid)?;
                let values = form_values(v, "role")?;
                let Some(mut draft) = self.role_add.take() else {
                    return Ok(());
                };
                draft.role = values.get("role_id").cloned().unwrap_or_default();
                let Ok(role) = draft.role.trim().parse::<i64>() else {
                    draft.errors =
                        [("role_id".to_owned(), vec!["Choose a role.".to_owned()])].into();
                    self.role_add = Some(draft);
                    return Ok(());
                };
                match self
                    .submit(
                        scope,
                        credential,
                        "add_role",
                        operations::ADD_ROLE,
                        json!({"team_id":id,"role_id":role}),
                    )
                    .await?
                {
                    Ok(result) => self.flash = Some((StatusKind::Success, done(&result["result"]))),
                    Err(fields) => {
                        draft.errors = fields;
                        self.role_add = Some(draft);
                    }
                }
            }
            "remove_role" => {
                if let Ok(role) = text(v, "role").parse::<i64>() {
                    self.role_removing = Some(role);
                }
            }
            "cancel_remove_role" => self.role_removing = None,
            "confirm_remove_role" => {
                let id = self.id.ok_or(Error::Invalid)?;
                let Some(role) = self.role_removing.take() else {
                    return Ok(());
                };
                if let Ok(result) = self
                    .submit(
                        scope,
                        credential,
                        "confirm_remove_role",
                        operations::REMOVE_ROLE,
                        json!({"team_id":id,"role_id":role}),
                    )
                    .await?
                {
                    self.flash = Some((StatusKind::Success, done(&result["result"])));
                }
            }
            "confirm_remove" => {
                let id = self.id.ok_or(Error::Invalid)?;
                let users = std::mem::take(&mut self.removing);
                let input = json!({"team_id":id,"user_ids":users});
                if let Ok(result) = self
                    .submit(
                        scope,
                        credential,
                        "confirm_remove",
                        operations::REMOVE_MEMBERS,
                        input,
                    )
                    .await?
                {
                    self.selection.clear();
                    self.flash = Some((
                        StatusKind::Success,
                        result["result"]["summary"]
                            .as_str()
                            .unwrap_or("Removed.")
                            .to_owned()
                            + ".",
                    ));
                }
            }
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }
}
fn done(result: &Value) -> String {
    result["summary"].as_str().unwrap_or("Saved").to_owned() + "."
}
fn added_message(result: &Value) -> String {
    let mut message = result["summary"]
        .as_str()
        .unwrap_or("Members added")
        .to_owned()
        + ".";
    let unchanged: Vec<&str> = result["unchanged"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !unchanged.is_empty() {
        message.push_str(&format!(" Already in the team: {}.", unchanged.join(", ")));
    }
    message
}
