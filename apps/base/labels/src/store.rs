//! The label overrides and the facade that reads and writes them. Every
//! statement binds its values and carries the tenant predicate; every write
//! goes through the caller's mutation transaction and is audited.
use bilibi_database::{Change, Db, Error, MutationTransaction, Result};
use bilibi_menu::Menu;
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio_postgres::GenericClient;

/// Characters of one label.
pub const MAX_LABEL: usize = 60;
/// The audit kind of one override.
pub const KIND: &str = "label";

/// What a label names.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Kind {
    /// A menu item, by its menu ID.
    Menu,
    /// A record type, by its Grid table ID.
    Record,
}
impl Kind {
    pub fn key(self) -> &'static str {
        match self {
            Self::Menu => "menu",
            Self::Record => "record",
        }
    }
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "menu" => Some(Self::Menu),
            "record" => Some(Self::Record),
            _ => None,
        }
    }
    pub fn noun(self) -> &'static str {
        match self {
            Self::Menu => "Menu item",
            Self::Record => "Record type",
        }
    }
}

/// One word a tenant may rename: the module's own label for a stable ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Subject {
    pub kind: Kind,
    pub id: String,
    /// The module's own word.
    pub default: String,
    /// Where it sits, for telling same-named items apart (a menu item's
    /// parents); empty for a record type.
    pub trail: String,
}

/// A tenant's overrides as one read of the table, to ask word by word.
#[derive(Clone, Debug, Default)]
pub struct LabelSet(BTreeMap<(Kind, String), String>);
impl LabelSet {
    /// The label for a menu item: the tenant's, or `default`.
    pub fn menu(&self, id: &str, default: &str) -> String {
        self.word(Kind::Menu, id, default)
    }
    /// The label for a record type (a Grid table ID): the tenant's, or `default`.
    pub fn record(&self, table: &str, default: &str) -> String {
        self.word(Kind::Record, table, default)
    }
    fn word(&self, kind: Kind, id: &str, default: &str) -> String {
        self.0
            .get(&(kind, id.to_owned()))
            .map_or_else(|| default.to_owned(), Clone::clone)
    }
}

/// A stored override with the version a writer must still hold.
#[derive(Clone, Debug)]
pub struct Stored {
    pub id: i64,
    pub label: String,
    pub version: i64,
}

/// One rename or clear: the word's kind and stable ID, the new text (blank
/// clears), and the version of the override the writer read (0 when none).
#[derive(Clone, Copy, Debug)]
pub struct Edit<'a> {
    pub kind: Kind,
    pub subject: &'a str,
    pub text: &'a str,
    pub version: (i64, i64),
}

/// What a rename or clear would do, decided without writing.
#[derive(Clone, Debug)]
pub struct Plan {
    pub kind: Kind,
    pub subject: String,
    pub default: String,
    pub before: Option<String>,
    /// `None` clears the override.
    pub after: Option<String>,
}

pub struct Labels {
    pub(crate) db: Db,
    menu: Menu,
}

impl Labels {
    pub(crate) fn new(db: Db, menu: Menu) -> Self {
        Self { db, menu }
    }

    /// The tenant's overrides. Reads need no capability: a label is shown to
    /// everyone who sees the item.
    pub async fn labels(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &Scope,
    ) -> Result<LabelSet> {
        Ok(LabelSet(
            Self::stored(client, scope)
                .await?
                .into_iter()
                .map(|(key, stored)| (key, stored.label))
                .collect(),
        ))
    }

    /// The overrides with their versions, for the page that edits them.
    pub(crate) async fn stored(
        client: &impl GenericClient,
        scope: &Scope,
    ) -> Result<BTreeMap<(Kind, String), Stored>> {
        let rows = client
            .query(
                "SELECT kind, subject, label, version, id FROM base_labels WHERE tenant_id=$1 LIMIT 5000",
                &[&scope.tenant()],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let kind = Kind::from_key(row.get(0))?;
                Some((
                    (kind, row.get::<_, String>(1)),
                    Stored {
                        id: row.get(4),
                        label: row.get(2),
                        version: row.get(3),
                    },
                ))
            })
            .collect())
    }

    /// Every word a tenant may rename, menu items first, by their trail.
    pub fn subjects(&self) -> Result<Vec<Subject>> {
        let mut subjects = Vec::new();
        for item in self.menu.items() {
            let item = item.item;
            let mut trail = Vec::new();
            let (mut at, mut portal) = (item.parent, item.id == bilibi_menu::PORTAL_ROOT);
            while let Some(parent) = at.and_then(|id| self.menu.fetch(id)) {
                portal |= parent.item.id == bilibi_menu::PORTAL_ROOT;
                trail.push(parent.item.label);
                at = parent.item.parent;
            }
            // The customer portal draws its own menu; an override here would show nowhere.
            if portal {
                continue;
            }
            trail.reverse();
            subjects.push(Subject {
                kind: Kind::Menu,
                id: item.id.to_owned(),
                default: item.label.to_owned(),
                trail: trail.join(" › "),
            });
        }
        subjects.sort_by(|a, b| (&a.trail, &a.default, &a.id).cmp(&(&b.trail, &b.default, &b.id)));
        let installed = bilibi_grid::installed().map_err(|_| Error::Internal)?;
        let mut records: Vec<Subject> = installed
            .tables()
            .map(|table| Subject {
                kind: Kind::Record,
                id: table.id().to_owned(),
                default: table.label().to_owned(),
                trail: String::new(),
            })
            .collect();
        records.sort_by(|a, b| (&a.default, &a.id).cmp(&(&b.default, &b.id)));
        subjects.extend(records);
        Ok(subjects)
    }

    fn subject(&self, kind: Kind, id: &str) -> Result<Subject> {
        self.subjects()?
            .into_iter()
            .find(|subject| subject.kind == kind && subject.id == id)
            .ok_or_else(|| Error::Refused(format!("{} “{id}” cannot be renamed.", kind.noun())))
    }

    /// Decides what setting `text` (blank clears) would do, or why not. Needs
    /// `base.labels.manage`. With `version`, the override must still be the one
    /// the writer read (0 when there was none).
    pub async fn plan(
        &self,
        sql: &impl GenericClient,
        scope: &Scope,
        edit: Edit<'_>,
        lock: bool,
    ) -> Result<Plan> {
        let Edit {
            kind,
            subject,
            text,
            version,
        } = edit;
        bilibi_authz::authorize_key(sql, scope, crate::MANAGE).await?;
        let known = self.subject(kind, subject)?;
        let text = text.trim();
        if text.chars().count() > MAX_LABEL || text.chars().any(char::is_control) {
            return Err(Error::Refused(format!(
                "A label is plain text of up to {MAX_LABEL} characters."
            )));
        }
        let lock = if lock { " FOR UPDATE" } else { "" };
        let current = sql
            .query_opt(
                &format!("SELECT label, version, id FROM base_labels WHERE tenant_id=$1 AND kind=$2 AND subject=$3{lock}"),
                &[&scope.tenant(), &kind.key(), &subject],
            )
            .await?;
        if version
            != current
                .as_ref()
                .map_or((0, 0), |row| (row.get::<_, i64>(2), row.get::<_, i64>(1)))
        {
            return Err(Error::Conflict);
        }
        Ok(Plan {
            kind,
            subject: known.id,
            default: known.default,
            before: current.map(|row| row.get(0)),
            after: Some(text.to_owned()).filter(|text| !text.is_empty()),
        })
    }

    /// Sets or clears one override in the caller's transaction (see [`Self::plan`]).
    pub async fn apply(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &Scope,
        edit: Edit<'_>,
    ) -> Result<Plan> {
        let plan = self.plan(tx.sql(), scope, edit, true).await?;
        let (tenant, kind_key, subject_id) =
            (scope.tenant(), edit.kind.key(), edit.subject.to_owned());
        let (before, after) = (plan.before.clone(), plan.after.clone());
        if before == after {
            return Ok(plan);
        }
        tx.mutate(move |sql| {
            Box::pin(async move {
                let (id, new) = match &after {
                    Some(label) => {
                        let query = if before.is_some() {
                            "UPDATE base_labels SET label=$4, version=version+1, updated_at=now() WHERE tenant_id=$1 AND kind=$2 AND subject=$3 RETURNING id"
                        } else {
                            "INSERT INTO base_labels(tenant_id,kind,subject,label) VALUES($1,$2,$3,$4) ON CONFLICT (tenant_id,kind,subject) DO NOTHING RETURNING id"
                        };
                        let row = sql.query_opt(query, &[&tenant, &kind_key, &subject_id, label]).await?.ok_or(Error::Conflict)?;
                        (row.get::<_, i64>(0), json!({"label": label}))
                    }
                    None => {
                        let row = sql
                            .query_one(
                                "DELETE FROM base_labels WHERE tenant_id=$1 AND kind=$2 AND subject=$3 RETURNING id",
                                &[&tenant, &kind_key, &subject_id],
                            )
                            .await?;
                        (row.get(0), Value::Null)
                    }
                };
                let old = before.map_or(Value::Null, |label| json!({"label": label}));
                let subject = json!({"kind": kind_key, "subject": subject_id});
                let (old, new) = (merge(old, &subject), merge(new, &subject));
                Ok(((), vec![Change { kind: KIND.into(), id, old, new }]))
            })
        })
        .await?;
        Ok(plan)
    }
}

/// An audit side that names the subject, left empty when it has no override.
fn merge(side: Value, subject: &Value) -> Value {
    match side {
        Value::Object(mut fields) => {
            if let Value::Object(more) = subject {
                fields.extend(more.clone());
            }
            Value::Object(fields)
        }
        other => other,
    }
}
