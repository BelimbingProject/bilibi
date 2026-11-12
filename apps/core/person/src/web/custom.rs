//! A person's custom fields on the pages: the record page's facts, edited in
//! place, and the create form's controls. Each value is shown as its type
//! reads (a date in the reader's format, a choice by its label, marked when
//! retired); every save goes through `core.person.update` or
//! `core.person.create`, which check it against the tenant's definitions.
use super::{render, Pages};
use bilibi_custom_fields::{Definition, Kind};
use bilibi_database::Result;
use bilibi_datetime::DisplayContext;
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
/// the reader's format, anything else as stored; `—` when empty. The caller
/// resolves `display` once for the page (`Pages::display` reads the locale
/// and display settings on a pooled connection each call).
pub(super) async fn shown(
    pages: &Pages,
    scope: &Scope,
    display: &DisplayContext,
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
            Ok(date) => display.render_date(date),
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
    display: &DisplayContext,
    definition: &Definition,
    value: &str,
    mut field: InlineField,
) -> Result<Html> {
    if let Some(help) = &definition.help {
        field = field.hint(help.clone());
    }
    let display = shown(pages, scope, display, definition, value).await?;
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

/// The list offers the first of the tenant's fields (in their order) as
/// columns, sorts and filters.
pub(super) const LIST_MAX: usize = 50;

/// One filter control's URL key and the condition it makes: a text or choice
/// field has one (`cf_<key>`), a number or date field a lower and an upper
/// bound (`cf_<key>-min`/`-max`, `cf_<key>-from`/`-to`). A key never holds
/// `-`, so no two fields' controls share a name.
pub(super) fn filter_keys(definition: &Definition) -> Vec<(String, &'static str)> {
    let name = crate::custom::name(&definition.key);
    match definition.kind {
        Kind::Text => vec![(name, "contains")],
        Kind::Choice => vec![(name, "eq")],
        Kind::Number => vec![
            (format!("{name}-min"), "gte"),
            (format!("{name}-max"), "lte"),
        ],
        Kind::Date => vec![
            (format!("{name}-from"), "gte"),
            (format!("{name}-to"), "lte"),
        ],
    }
}

/// The list's spec with each field's sort and filter keys added.
pub(super) fn spec(base: &bilibi_ui::ListSpec, definitions: &[Definition]) -> bilibi_ui::ListSpec {
    let mut spec = base.clone();
    for definition in definitions {
        spec.sortable.insert(
            crate::custom::name(&definition.key),
            bilibi_ui::SortDir::Asc,
        );
        // An empty condition leaves the address, as an empty search does.
        for (key, _) in filter_keys(definition) {
            spec.filters
                .insert(key.clone(), bilibi_ui::Filter::string(""));
            spec.omit_blank.push(key);
        }
    }
    spec
}

/// The conditions the list's filters make, and why a filter that cannot be
/// applied was not (by its URL key); a blank filter makes none.
pub(super) fn conditions(
    filters: &std::collections::BTreeMap<String, String>,
    definitions: &[Definition],
) -> (
    Vec<crate::CustomFilter>,
    std::collections::BTreeMap<String, String>,
) {
    let mut out = Vec::new();
    let mut errors = std::collections::BTreeMap::new();
    for definition in definitions {
        for (key, op) in filter_keys(definition) {
            let value = filters.get(&key).map(|v| v.trim()).unwrap_or_default();
            if value.is_empty() {
                continue;
            }
            let filter = crate::CustomFilter {
                key: definition.key.clone(),
                op: op.into(),
                value: value.into(),
            };
            if crate::custom::condition(definition, &filter, "x", 1).is_some() {
                out.push(filter);
            } else {
                errors.insert(
                    key,
                    match definition.kind {
                        Kind::Number => "Enter a number such as 12 or -3.5; not applied.",
                        Kind::Date => "Enter a date; not applied.",
                        _ => "Enter up to 256 characters; not applied.",
                    }
                    .to_owned(),
                );
            }
        }
    }
    (out, errors)
}

/// The filter sheet's controls for the fields: text contains, a choice from
/// its list, a number or a date between two bounds.
pub(super) async fn controls(
    pages: &Pages,
    scope: &Scope,
    definitions: &[Definition],
    form: &std::collections::HashMap<String, String>,
    errors: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<bilibi_ui::ToolbarControl>> {
    use bilibi_ui::ToolbarControl;
    let mut controls = Vec::new();
    for definition in definitions {
        for (key, op) in filter_keys(definition) {
            let id = format!("persons-{}", key.replace('_', "-"));
            let value = form.get(&key).map(String::as_str).unwrap_or_default();
            let label = match op {
                "gte" if definition.kind == Kind::Date => format!("{} from", definition.label),
                "lte" if definition.kind == Kind::Date => format!("{} to", definition.label),
                "gte" => format!("{} at least", definition.label),
                "lte" => format!("{} at most", definition.label),
                _ => definition.label.clone(),
            };
            let mut control = match (definition.kind, definition.picklist.as_deref()) {
                (Kind::Choice, Some(list)) => {
                    let mut options = vec![bilibi_ui::Choice::new(
                        format!("Any {}", definition.label.to_lowercase()),
                        String::new(),
                    )];
                    options.extend(options_of(pages, scope, list).await?);
                    ToolbarControl::select(&id, &key, &label, value, options)
                }
                (Kind::Date, _) => ToolbarControl::date(&id, &key, &label, value),
                (Kind::Number, _) => ToolbarControl::text(&id, &key, &label, value).numeric(),
                _ => ToolbarControl::text(&id, &key, &label, value),
            };
            // A toolbar label is screen-reader only; a date box shows its
            // label below (a text box shows it as its placeholder).
            if let Some(error) = errors.get(&key) {
                control = control.hint(error);
            } else if matches!(definition.kind, Kind::Date) {
                control = control.hint(&label);
            }
            controls.push(control);
        }
    }
    Ok(controls)
}

async fn options_of(pages: &Pages, scope: &Scope, list: &str) -> Result<Vec<bilibi_ui::Choice>> {
    let client = pages.db.get().await?;
    pages.picklists.choices(&**client, scope, list, None).await
}

/// Each field's value as the list shows it, per person on the page: a
/// choice by its label (marked when retired), a date in the reader's format.
/// One label read per distinct choice code on the page. `definitions` are
/// the fields whose column is on screen.
pub(super) async fn cells(
    pages: &Pages,
    scope: &Scope,
    display: &DisplayContext,
    definitions: &[&Definition],
    people: &[crate::Person],
) -> Result<std::collections::BTreeMap<(i64, String), String>> {
    let mut labels: std::collections::BTreeMap<(String, String), String> = Default::default();
    let mut out = std::collections::BTreeMap::new();
    for definition in definitions {
        let name = crate::custom::name(&definition.key);
        for person in people {
            let value = text(person.custom.get(&definition.key));
            if value.is_empty() {
                continue;
            }
            let label_key = (definition.key.clone(), value.clone());
            let shown = match labels.get(&label_key) {
                Some(shown) => shown.clone(),
                None => {
                    let shown = shown(pages, scope, display, definition, &value).await?;
                    labels.insert(label_key, shown.clone());
                    shown
                }
            };
            out.insert((person.id.get(), name.clone()), shown);
        }
    }
    Ok(out)
}
