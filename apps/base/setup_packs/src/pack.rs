use crate::{schema, SetupPacks};
use bilibi_agent_api::{invalid, Result};
use bilibi_custom_fields::{Definition, NewField};
use bilibi_database::Error;
use bilibi_labels::{Edit, Kind, Labels};
use bilibi_tenancy::Scope;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use tokio_postgres::Transaction;

/// Bound the number of owner checks and the size of one reviewed transaction.
const MAX_ENTRIES: usize = 100;

#[derive(Clone, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct LabelKey {
    kind: String,
    subject: String,
}

impl LabelKey {
    fn kind(&self) -> Result<Kind> {
        Kind::from_key(&self.kind).ok_or_else(|| invalid("kind", "Choose menu, record or field."))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Label {
    #[serde(flatten)]
    key: LabelKey,
    label: String,
}

impl Label {
    pub(crate) fn edit(&self) -> Result<Edit<'_>> {
        Ok(Edit {
            kind: self.key.kind()?,
            subject: &self.key.subject,
            text: &self.label,
            version: (0, 0),
        })
    }
}

#[derive(Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct FieldKey {
    record_type: String,
    key: String,
}

/// Version 1 carries only the facts someone could enter when defining a field.
#[derive(Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Field {
    record_type: String,
    key: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    help: Option<String>,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    picklist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    rule_condition: bool,
}

impl From<Definition> for Field {
    fn from(field: Definition) -> Self {
        Self {
            record_type: field.record_type,
            key: field.key,
            label: field.label,
            help: field.help,
            kind: field.kind.as_str().into(),
            picklist: field.picklist,
            target: field.target,
            rule_condition: field.rule_condition,
        }
    }
}

impl From<&Field> for NewField {
    fn from(field: &Field) -> Self {
        Self {
            record_type: field.record_type.clone(),
            key: field.key.clone(),
            label: field.label.clone(),
            help: field.help.clone(),
            kind: field.kind.clone(),
            picklist: field.picklist.clone(),
            target: field.target.clone(),
            rule_condition: field.rule_condition,
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct Selection {
    labels: Vec<LabelKey>,
    custom_fields: Vec<FieldKey>,
}

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct Pack {
    version: u8,
    labels: Vec<Label>,
    custom_fields: Vec<Field>,
}

#[derive(Deserialize)]
struct PicklistMap {
    from: String,
    to: String,
}

#[derive(Deserialize)]
pub(crate) struct Import {
    pack: Pack,
    #[serde(default)]
    picklist_map: Vec<PicklistMap>,
}

#[derive(Deserialize)]
pub(crate) struct ApplyRequest {
    #[serde(flatten)]
    pub import: Import,
    pub expected_additions: Selection,
}

#[derive(Serialize)]
struct Issue {
    entry: String,
    reason: String,
}

#[derive(Default, Serialize)]
pub(crate) struct Difference {
    pub ready: bool,
    pub additions: Additions,
    expected_additions: Selection,
    collisions: Vec<Issue>,
    missing: Vec<Issue>,
    invalid: Vec<Issue>,
}

#[derive(Default, Serialize)]
pub(crate) struct Additions {
    pub labels: Vec<Label>,
    pub custom_fields: Vec<Field>,
}

impl Difference {
    pub fn check_review(&self, expected: &Selection) -> Result<()> {
        let issues = self
            .collisions
            .iter()
            .chain(&self.missing)
            .chain(&self.invalid)
            .map(|issue| format!("{}: {}", issue.entry, issue.reason))
            .collect::<Vec<_>>();
        if !issues.is_empty() {
            return Err(bilibi_agent_api::Error::InvalidInput(BTreeMap::from([(
                "pack".into(),
                issues,
            )])));
        }
        count(expected.labels.len(), expected.custom_fields.len())?;
        if self
            .expected_additions
            .labels
            .iter()
            .any(|key| !expected.labels.contains(key))
            || self
                .expected_additions
                .custom_fields
                .iter()
                .any(|key| !expected.custom_fields.contains(key))
        {
            return Err(invalid(
                "expected_additions",
                "The destination needs new additions. Preview and review the pack again.",
            ));
        }
        Ok(())
    }
}

fn count(labels: usize, fields: usize) -> Result<()> {
    if labels + fields > MAX_ENTRIES {
        return Err(invalid(
            "pack",
            format!("Select at most {MAX_ENTRIES} entries in one pack."),
        ));
    }
    Ok(())
}

fn distinct<K: Ord>(keys: impl Iterator<Item = K>, field: &str) -> Result<()> {
    let mut seen = BTreeSet::new();
    for key in keys {
        if !seen.insert(key) {
            return Err(invalid(
                field,
                "An entry with this stable key appears more than once.",
            ));
        }
    }
    Ok(())
}

async fn authorize(
    tx: &Transaction<'_>,
    scope: &Scope,
    labels: bool,
    fields: bool,
    write: bool,
) -> Result<()> {
    if labels {
        let capability = if write {
            bilibi_labels::MANAGE
        } else {
            bilibi_labels::VIEW
        };
        bilibi_authz::authorize_key(tx, scope, capability).await?;
    }
    if fields {
        let capability = if write {
            bilibi_custom_fields::MANAGE
        } else {
            bilibi_custom_fields::VIEW
        };
        bilibi_authz::authorize_key(tx, scope, capability).await?;
    }
    Ok(())
}

impl SetupPacks {
    pub async fn export(
        &self,
        tx: &Transaction<'_>,
        scope: &Scope,
        selection: Selection,
    ) -> Result<Pack> {
        count(selection.labels.len(), selection.custom_fields.len())?;
        authorize(
            tx,
            scope,
            !selection.labels.is_empty(),
            !selection.custom_fields.is_empty(),
            false,
        )
        .await?;
        distinct(selection.labels.iter(), "labels")?;
        distinct(selection.custom_fields.iter(), "custom_fields")?;

        let subjects = self.label_subjects(!selection.labels.is_empty())?;
        let mut pack = Pack {
            version: schema::VERSION,
            ..Pack::default()
        };
        for key in selection.labels {
            let kind = key.kind()?;
            if !subjects.contains(&(kind, key.subject.clone())) {
                return Err(invalid(
                    "labels",
                    format!("The {} subject “{}” is unavailable.", key.kind, key.subject),
                ));
            }
            let stored = Labels::stored_override(tx, scope, kind, &key.subject, false)
                .await?
                .ok_or_else(|| {
                    invalid(
                        "labels",
                        format!("“{}” has no override to export.", key.subject),
                    )
                })?;
            pack.labels.push(Label {
                key,
                label: stored.label,
            });
        }
        let mut fields = Vec::new();
        for FieldKey { record_type, key } in selection.custom_fields {
            let field = self
                .fields
                .stored_definition(tx, scope, &record_type, &key, false)
                .await?
                .filter(|field| !field.archived())
                .ok_or_else(|| {
                    invalid(
                        "custom_fields",
                        format!("“{record_type}.{key}” is not an active definition."),
                    )
                })?;
            fields.push(field);
        }
        pack.labels.sort_by(|left, right| left.key.cmp(&right.key));
        fields.sort_by(|left, right| {
            (&left.record_type, left.position, &left.key).cmp(&(
                &right.record_type,
                right.position,
                &right.key,
            ))
        });
        pack.custom_fields = fields.into_iter().map(Field::from).collect();
        Ok(pack)
    }

    fn label_subjects(&self, needed: bool) -> Result<BTreeSet<(Kind, String)>> {
        if !needed {
            return Ok(BTreeSet::new());
        }
        Ok(self
            .labels
            .subjects()?
            .into_iter()
            .map(|subject| (subject.kind, subject.id))
            .collect())
    }

    pub async fn difference(
        &self,
        tx: &Transaction<'_>,
        scope: &Scope,
        request: Import,
        lock: bool,
    ) -> Result<Difference> {
        let Import {
            mut pack,
            picklist_map,
        } = request;
        count(pack.labels.len(), pack.custom_fields.len())?;
        authorize(
            tx,
            scope,
            !pack.labels.is_empty(),
            !pack.custom_fields.is_empty(),
            true,
        )
        .await?;

        for field in &mut pack.custom_fields {
            field.normalize();
        }
        distinct(pack.labels.iter().map(|label| &label.key), "pack.labels")?;
        distinct(
            pack.custom_fields
                .iter()
                .map(|field| (&field.record_type, &field.key)),
            "pack.custom_fields",
        )?;
        let mappings = picklist_mappings(&pack.custom_fields, &picklist_map)?;
        let mut diff = Difference::default();
        self.compare_labels(tx, scope, pack.labels, lock, &mut diff)
            .await?;
        self.compare_fields(tx, scope, pack.custom_fields, &mappings, lock, &mut diff)
            .await?;
        diff.ready =
            diff.collisions.is_empty() && diff.missing.is_empty() && diff.invalid.is_empty();
        diff.expected_additions = Selection {
            labels: diff
                .additions
                .labels
                .iter()
                .map(|label| label.key.clone())
                .collect(),
            custom_fields: diff
                .additions
                .custom_fields
                .iter()
                .map(|field| FieldKey {
                    record_type: field.record_type.clone(),
                    key: field.key.clone(),
                })
                .collect(),
        };
        Ok(diff)
    }

    async fn compare_labels(
        &self,
        tx: &Transaction<'_>,
        scope: &Scope,
        mut labels: Vec<Label>,
        lock: bool,
        diff: &mut Difference,
    ) -> Result<()> {
        let subjects = self.label_subjects(!labels.is_empty())?;
        labels.sort_by(|left, right| left.key.cmp(&right.key));
        for mut label in labels {
            let edit = label.edit()?;
            let entry = format!("{} label {}", label.key.kind, label.key.subject);
            if !subjects.contains(&(edit.kind, edit.subject.to_owned())) {
                diff.missing.push(Issue {
                    entry,
                    reason: "The label subject is unavailable.".into(),
                });
                continue;
            }
            let current = Labels::stored_override(tx, scope, edit.kind, edit.subject, lock).await?;
            if let Some(current) = current {
                if current.label != label.label.trim() {
                    diff.collisions.push(Issue {
                        entry,
                        reason: "The destination has a different override.".into(),
                    });
                }
                continue;
            }

            match self.labels.plan(tx, scope, edit, false).await {
                Ok(plan) => match plan.after {
                    Some(text) => {
                        label.label = text;
                        diff.additions.labels.push(label);
                    }
                    None => diff.invalid.push(Issue {
                        entry,
                        reason: "Supply the text of an override.".into(),
                    }),
                },
                Err(Error::Refused(reason)) => diff.invalid.push(Issue { entry, reason }),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

impl Field {
    fn normalize(&mut self) {
        self.record_type = self.record_type.trim().into();
        self.key = self.key.trim().into();
        self.label = self.label.trim().into();
        for text in [&mut self.help, &mut self.picklist, &mut self.target] {
            *text = text
                .take()
                .map(|text| text.trim().to_owned())
                .filter(|text| !text.is_empty());
        }
    }
}

fn picklist_mappings<'a>(
    fields: &[Field],
    mappings: &'a [PicklistMap],
) -> Result<BTreeMap<&'a str, &'a str>> {
    let mut mapped = BTreeMap::new();
    for mapping in mappings {
        let from = mapping.from.as_str();
        let to = mapping.to.as_str();
        if to.is_empty()
            || !fields
                .iter()
                .any(|field| field.picklist.as_deref() == Some(from))
        {
            return Err(invalid(
                "picklist_map",
                "Map a pick-list used by the pack to a destination key.",
            ));
        }
        if mapped.insert(from, to).is_some() {
            return Err(invalid(
                "picklist_map",
                "A source pick-list can be mapped only once.",
            ));
        }
    }
    Ok(mapped)
}

impl SetupPacks {
    async fn compare_fields(
        &self,
        tx: &Transaction<'_>,
        scope: &Scope,
        mut fields: Vec<Field>,
        mappings: &BTreeMap<&str, &str>,
        lock: bool,
        diff: &mut Difference,
    ) -> Result<()> {
        let picklists: BTreeSet<_> = self
            .fields
            .choice_lists()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        let mut pending = BTreeMap::new();
        // Hold record-type locks in the same order across imports. Preserve
        // the pack's definition order within each type for the owner's append.
        fields.sort_by(|left, right| left.record_type.cmp(&right.record_type));
        for mut field in fields {
            let entry = format!("custom field {}.{}", field.record_type, field.key);
            if let Some(to) = field.picklist.as_deref().and_then(|key| mappings.get(key)) {
                field.picklist = Some((*to).into());
            }

            let previous_missing = diff.missing.len();
            if self.fields.record_type(&field.record_type).is_err() {
                diff.missing.push(Issue {
                    entry: entry.clone(),
                    reason: format!("Record type “{}” is unavailable.", field.record_type),
                });
            }
            if let Some(list) = field
                .picklist
                .as_deref()
                .filter(|key| !picklists.contains(key))
            {
                diff.missing.push(Issue {
                    entry: entry.clone(),
                    reason: format!(
                        "Pick-list “{list}” is unavailable. Choose a destination pick-list mapping."
                    ),
                });
            }
            if let Some(key) = field
                .target
                .as_deref()
                .filter(|key| self.fields.records().kind(key).is_none())
            {
                diff.missing.push(Issue {
                    entry: entry.clone(),
                    reason: format!("Reference kind “{key}” is unavailable."),
                });
            }
            if diff.missing.len() != previous_missing {
                continue;
            }

            let current = self
                .fields
                .stored_definition(tx, scope, &field.record_type, &field.key, lock)
                .await?;
            if let Some(current) = current {
                if current.archived() || Field::from(current) != field {
                    diff.collisions.push(Issue {
                        entry,
                        reason: "The destination key is archived or has a different definition."
                            .into(),
                    });
                }
                continue;
            }

            let earlier = pending.entry(field.record_type.clone()).or_default();
            match self
                .fields
                .preview_define_after(tx, scope, &NewField::from(&field), *earlier)
                .await?
            {
                Ok(_) => {
                    *earlier += 1;
                    diff.additions.custom_fields.push(field);
                }
                Err(errors) => diff.invalid.extend(errors.into_iter().map(|error| Issue {
                    entry: entry.clone(),
                    reason: format!("{}: {}", error.field, error.message),
                })),
            }
        }
        Ok(())
    }
}
