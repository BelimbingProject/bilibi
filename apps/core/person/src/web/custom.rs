//! A person's custom fields on the pages: the record page's facts, edited in
//! place, and the create form's controls. Each value is shown as its type
//! reads (a date in the reader's format, a choice by its label, marked when
//! retired); every save goes through `core.person.update` or
//! `core.person.create`, which check it against the tenant's definitions.
use super::{render, Pages};
use bilibi_custom_fields::{Definition, Kind};
use bilibi_database::Result;
use bilibi_tenancy::Scope;
use bilibi_ui::{Field, Html, InlineChoice, InlineEdit, InlineField, InputKind};
use serde_json::Value;

/// The tenant's active person fields, in their order.
pub(super) async fn active(pages: &Pages, scope: &Scope) -> Result<Vec<Definition>> {
    let client = pages.db.get().await?;
    crate::custom::fields()?
        .active(&**client, scope, crate::custom::RECORD_TYPE)
        .await
}

/// A stored value as text (an absent one is empty).
pub(super) fn text(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// How a value reads: a choice by its label (marked when retired), a date in
/// the reader's format, anything else as stored; `—` when empty.
pub(super) async fn shown(
    pages: &Pages,
    scope: &Scope,
    definition: &Definition,
    value: &str,
) -> Result<String> {
    if value.is_empty() {
        return Ok("—".to_owned());
    }
    Ok(match (definition.kind, definition.picklist.as_deref()) {
        (Kind::Choice, Some(list)) => {
            let client = pages.db.get().await?;
            match pages.picklists.label(&**client, scope, list, value).await? {
                Some(label) if label.retired => format!("{} (Retired)", label.text),
                Some(label) => label.text,
                None => value.to_owned(),
            }
        }
        (Kind::Date, _) => match value.parse::<jiff::civil::Date>() {
            Ok(date) => pages.display(scope).await?.render_date(date),
            Err(_) => value.to_owned(),
        },
        _ => value.to_owned(),
    })
}

/// A choice field's options: none, then the list's active values, and the
/// held value too when the list no longer offers it.
async fn options(
    pages: &Pages,
    scope: &Scope,
    list: &str,
    held: &str,
) -> Result<Vec<bilibi_ui::Choice>> {
    let client = pages.db.get().await?;
    let mut options = vec![bilibi_ui::Choice::new("—".to_owned(), String::new())];
    let offered = pages
        .picklists
        .choices(&**client, scope, list, None)
        .await?;
    if !held.is_empty() && !offered.iter().any(|c| c.value == held) {
        let label = pages
            .picklists
            .label(&**client, scope, list, held)
            .await?
            .map_or_else(|| held.to_owned(), |l| format!("{} (Retired)", l.text));
        options.push(bilibi_ui::Choice::new(label, held.to_owned()));
    }
    options.extend(offered);
    Ok(options)
}

/// The record page's fact for one field, edited in place by `save_custom_field`.
pub(super) async fn fact(
    pages: &Pages,
    scope: &Scope,
    definition: &Definition,
    value: &str,
    mut field: InlineField,
) -> Result<Html> {
    if let Some(help) = &definition.help {
        field = field.hint(help.clone());
    }
    let display = shown(pages, scope, definition, value).await?;
    match (definition.kind, definition.picklist.as_deref()) {
        (Kind::Choice, Some(list)) => {
            let editor = Field::named(&field.name)
                .id(format!("{}-select", field.id))
                .label_class("sr-only")
                .label(&definition.label)
                .kind(InputKind::Select)
                .value(value)
                .options(options(pages, scope, list, value).await?);
            render(InlineChoice {
                field,
                display: Html::text(&display),
                editor: render(editor)?,
            })
        }
        (Kind::Date, _) => render(InlineEdit {
            field: field.input_type("date").display_value(display),
        }),
        _ => render(InlineEdit { field }),
    }
}

/// The create form's control for one field, named `cf_<key>`.
pub(super) async fn control(
    pages: &Pages,
    scope: &Scope,
    definition: &Definition,
    value: &str,
    errors: Vec<String>,
) -> Result<Field> {
    let name = crate::custom::name(&definition.key);
    let mut field = Field::in_form("person", &name)
        .id(format!("person-{}", name.replace('_', "-")))
        .label(&definition.label)
        .value(value)
        .errors(errors);
    if let Some(help) = &definition.help {
        field = field.hint(help);
    }
    Ok(match (definition.kind, definition.picklist.as_deref()) {
        (Kind::Choice, Some(list)) => field
            .kind(InputKind::Select)
            .options(options(pages, scope, list, value).await?),
        (Kind::Date, _) => field.kind(InputKind::Date),
        (Kind::Number, _) => field.kind(InputKind::Text).attr("inputmode", "decimal"),
        _ => field.kind(InputKind::Text),
    })
}
