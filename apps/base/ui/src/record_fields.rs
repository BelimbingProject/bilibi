//! Record facts, Grid columns and contract inputs share one field vocabulary.
use crate::{BuiltinColumn, Fact, Html};

/// A record fact. `column` names its built-in slot or scoped Grid path.
/// Long text, files and nested collections have no column; their reason stays
/// beside the declaration. Restrictions belong to the owner's scoped read or
/// Grid catalog, never to this presentation metadata.
#[derive(Clone, Copy, Debug)]
pub struct RecordField {
    pub id: &'static str,
    pub vocabulary: &'static str,
    pub ty: RecordFieldType,
    pub values: &'static [&'static str],
    pub picklist: Option<&'static str>,
    pub personal: Option<PersonalDataClass>,
    pub label: &'static str,
    pub column: Option<&'static str>,
    pub slot: Option<&'static str>,
    pub exclusion: Option<&'static str>,
}
impl RecordField {
    pub const fn new(id: &'static str, label: &'static str, ty: RecordFieldType) -> Self {
        Self {
            id,
            label,
            vocabulary: "",
            ty,
            values: &[],
            picklist: None,
            personal: None,
            column: Some(id),
            slot: None,
            exclusion: None,
        }
    }
    pub const fn choices(self, values: &'static [&'static str]) -> Self {
        Self { values, ..self }
    }
    pub const fn picklist(self, picklist: &'static str) -> Self {
        Self {
            picklist: Some(picklist),
            ..self
        }
    }
    /// None means unclassified; use `NonPersonal` only after owner review.
    pub const fn personal(self, kind: PersonalDataClass) -> Self {
        Self {
            personal: Some(kind),
            ..self
        }
    }
    /// Operation-specific limits, requiredness and references stay with the operation.
    /// Boot checks overrides of vocabulary metadata, rather than silently replacing them.
    pub fn input(self, constraints: serde_json::Value) -> serde_json::Value {
        let mut schema = self.schema();
        schema
            .as_object_mut()
            .expect("field schema")
            .extend(constraints.as_object().expect("field constraints").clone());
        schema["x-bilibi-field"] = self.vocabulary.into();
        schema
    }
    pub fn schema(self) -> serde_json::Value {
        use serde_json::json;
        let mut schema = json!({"type": self.ty.json_type(), "title": self.label});
        if let Some(format) = self.ty.format() {
            schema["format"] = format.into();
        }
        if !self.values.is_empty() {
            schema["enum"] = json!(self.values);
        }
        if let Some(picklist) = self.picklist {
            schema["x-bilibi-picklist"] = picklist.into();
        }
        if let Some(personal) = self.personal {
            schema["x-bilibi-personal-data"] = personal.key().into();
        }
        schema
    }
    pub const fn path(self, column: &'static str) -> Self {
        Self {
            column: Some(column),
            ..self
        }
    }
    /// An existing list slot displaying this field under a historical column id.
    pub const fn slot(self, slot: &'static str) -> Self {
        Self {
            slot: Some(slot),
            ..self
        }
    }
    pub const fn excluded(self, reason: &'static str) -> Self {
        Self {
            column: None,
            exclusion: Some(reason),
            ..self
        }
    }
    /// The page-owned slot for this fact. A walked path is projected into a
    /// DOM id by replacing path separators and aggregate punctuation with `-`.
    pub fn slot_id(self) -> Option<String> {
        self.column.map(|column| {
            self.slot.map_or_else(
                || {
                    column
                        .chars()
                        .map(|character| match character {
                            '.' | ':' => '-',
                            other => other,
                        })
                        .collect()
                },
                str::to_owned,
            )
        })
    }
    pub fn fact(self, value: Html) -> Fact<'static> {
        Fact {
            title: self.label,
            value,
            ..Fact::default()
        }
    }
    pub fn builtin(self) -> Option<BuiltinColumn> {
        self.slot_id().map(|id| BuiltinColumn::new(&id, self.label))
    }
}

/// Declare once, use each named field for a detail fact and ALL for its list.
/// Adding a scalar fact to this declaration makes it discoverable as a column.
#[macro_export]
macro_rules! record_fields {
    ($owner:literal, $vis:vis mod $module:ident { $($name:ident: $field:expr),* $(,)? }) => {
        $vis mod $module {
            $(pub const $name: $crate::RecordField = $crate::RecordField {
                vocabulary: concat!(module_path!(), "::", stringify!($name)), ..$field
            };
            inventory::submit! { bilibi_module_registry::Contribution::new(
                $crate::RECORD_FIELDS, $name.vocabulary,
                bilibi_module_registry::Provenance { module: $owner, source: file!(), line: line!() }, &$name
            ) })*
            pub const ALL: &[$crate::RecordField] = &[$($name),*];
            pub fn get(id: &str) -> $crate::RecordField {
                *ALL.iter().find(|field| field.id == id).expect("declared record field")
            }
        }
    };
}

/// Semantic types shared by record facts, Grid declarations and contract inputs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordFieldType {
    String,
    Integer,
    Float,
    Decimal,
    Boolean,
    Date,
    DateTime,
    Enum,
    Object,
    Array,
}
impl RecordFieldType {
    pub fn json_type(self) -> &'static str {
        match self {
            Self::Integer => "integer",
            Self::Float => "number",
            Self::Boolean => "boolean",
            Self::Object => "object",
            Self::Array => "array",
            _ => "string",
        }
    }
    pub fn format(self) -> Option<&'static str> {
        match self {
            Self::Date => Some("date"),
            Self::DateTime => Some("date-time"),
            _ => None,
        }
    }
}
pub const RECORD_FIELDS: &str = "base/ui.record_fields";
/// Resolve a contract's vocabulary reference without constructing another registry.
pub fn record_field(key: &str) -> Option<&'static RecordField> {
    inventory::iter::<bilibi_module_registry::Contribution>
        .into_iter()
        .find(|entry| entry.consumer == RECORD_FIELDS && entry.id == key)
        .and_then(|entry| entry.get::<RecordField>().ok())
}
fn validate(entries: &[bilibi_module_registry::Entry]) -> bilibi_module_registry::Result<()> {
    for entry in entries {
        let field = entry.get::<RecordField>()?;
        let mut choices = std::collections::BTreeSet::new();
        if field.id.is_empty()
            || field.id.len() > 64
            || !field
                .id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            || field.label.trim().is_empty()
            || field.label.chars().count() > 120
            || field.vocabulary != entry.id
            || (field.ty == RecordFieldType::Enum) == field.values.is_empty()
            || field
                .values
                .iter()
                .any(|value| value.trim().is_empty() || !choices.insert(value))
            || field.picklist.is_some_and(|key| {
                key.trim().is_empty() || key.len() > 255 || field.ty != RecordFieldType::String
            })
        {
            return Err(bilibi_module_registry::RegistryError::Contribution(
                format!("invalid record field {}", entry.id),
            ));
        }
    }
    Ok(())
}
inventory::submit! { bilibi_module_registry::Consumer::new::<RecordField>(RECORD_FIELDS, "base/ui", validate) }

/// Explicit non-personal differs from unknown: oversight denies unknown fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersonalDataClass {
    Name,
    Email,
    Phone,
    Identifier,
    Date,
    FreeText,
    NonPersonal,
}
impl PersonalDataClass {
    pub fn key(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Email => "email",
            Self::Phone => "phone",
            Self::Identifier => "identifier",
            Self::Date => "date",
            Self::FreeText => "free_text",
            Self::NonPersonal => "non_personal",
        }
    }
}
