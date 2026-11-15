//! Scalar read declarations projected from the shared record vocabulary.
/// Field types a contribution may declare.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldType {
    Integer,
    Float,
    Decimal,
    String,
    Boolean,
    Date,
    DateTime,
    Enum,
}
impl FieldType {
    /// Whether values order on a number line (bars, bands and heat).
    pub fn numeric(self) -> bool {
        matches!(
            self,
            Self::Integer | Self::Float | Self::Decimal | Self::Date | Self::DateTime
        )
    }
    /// Whether sum/avg make sense.
    pub fn number(self) -> bool {
        matches!(self, Self::Integer | Self::Float | Self::Decimal)
    }
    pub fn dated(self) -> bool {
        matches!(self, Self::Date | Self::DateTime)
    }
}

/// What kind of personal data a field holds (ADR 05). The kind shapes a partial
/// mask and names the field in an audit erasure; `base/privacy` refuses boot
/// when a table marks one and its module contributes no personal-data handler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersonalKind {
    Name,
    Email,
    Phone,
    Identifier,
    Date,
    FreeText,
}
impl PersonalKind {
    pub fn key(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Email => "email",
            Self::Phone => "phone",
            Self::Identifier => "identifier",
            Self::Date => "date",
            Self::FreeText => "free_text",
        }
    }
}

/// One value a column can show. `column` is the key the owner's source selects it
/// under; it defaults to the field id. `hidden` fields exist for links to join on
/// and never appear as columns. `protected` marks a field no operator may
/// restrict because the owner's own logic needs every reader to see it. A table's
/// key, label and time fields, hidden fields and join fields are protected
/// without saying so.
#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub id: &'static str,
    pub label: &'static str,
    pub ty: FieldType,
    pub column: Option<&'static str>,
    pub hidden: bool,
    /// The closed set an enum field takes, which the band lens colours.
    pub values: &'static [&'static str],
    pub picklist: Option<&'static str>,
    pub protected: bool,
    /// Set when the field holds a person's data; see [`PersonalKind`].
    pub personal: Option<PersonalKind>,
    /// Explicit owner classification, required for cross-tenant oversight.
    /// `personal == None` alone means unknown, not non-personal.
    pub classified: bool,
    pub vocabulary: Option<bilibi_ui::RecordField>,
}
impl Field {
    pub const fn new(id: &'static str, label: &'static str, ty: FieldType) -> Self {
        Self {
            id,
            label,
            ty,
            column: None,
            hidden: false,
            values: &[],
            picklist: None,
            protected: false,
            personal: None,
            classified: false,
            vocabulary: None,
        }
    }
    /// Read metadata comes from the owner's vocabulary; SQL and presentation stay here.
    pub const fn record(field: bilibi_ui::RecordField) -> Self {
        use bilibi_ui::{PersonalDataClass as P, RecordFieldType as T};
        let ty = match field.ty {
            T::Integer => FieldType::Integer,
            T::Float => FieldType::Float,
            T::Decimal => FieldType::Decimal,
            T::String => FieldType::String,
            T::Boolean => FieldType::Boolean,
            T::Date => FieldType::Date,
            T::DateTime => FieldType::DateTime,
            T::Enum => FieldType::Enum,
            T::Object | T::Array => panic!("a Grid field must be scalar"),
        };
        let personal = match field.personal {
            Some(P::Name) => Some(PersonalKind::Name),
            Some(P::Email) => Some(PersonalKind::Email),
            Some(P::Phone) => Some(PersonalKind::Phone),
            Some(P::Identifier) => Some(PersonalKind::Identifier),
            Some(P::Date) => Some(PersonalKind::Date),
            Some(P::FreeText) => Some(PersonalKind::FreeText),
            _ => None,
        };
        Self {
            values: field.values,
            picklist: field.picklist,
            personal,
            classified: field.personal.is_some(),
            vocabulary: Some(field),
            ..Self::new(field.id, field.label, ty)
        }
    }
    pub const fn choice(
        id: &'static str,
        label: &'static str,
        values: &'static [&'static str],
    ) -> Self {
        Self {
            values,
            ..Self::new(id, label, FieldType::Enum)
        }
    }
    pub const fn column(self, column: &'static str) -> Self {
        Self {
            column: Some(column),
            ..self
        }
    }
    pub const fn hidden(self) -> Self {
        Self {
            hidden: true,
            ..self
        }
    }
    pub const fn protected(self) -> Self {
        Self {
            protected: true,
            ..self
        }
    }
    pub const fn personal(self, kind: PersonalKind) -> Self {
        Self {
            personal: Some(kind),
            classified: true,
            ..self
        }
    }
    /// The owner has reviewed this value (including derived/hidden values)
    /// and determined it contains no person's data. Not the default.
    pub const fn non_personal(self) -> Self {
        Self {
            personal: None,
            classified: true,
            ..self
        }
    }
    /// The key of the owner's source this field is read from.
    pub fn source_key(&self) -> &'static str {
        self.column.unwrap_or(self.id)
    }
}
