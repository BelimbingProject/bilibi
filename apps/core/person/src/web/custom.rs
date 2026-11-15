//! A person's custom fields on the pages: the record page's facts, edited in
//! place, the create form's controls and the list's columns and filters.
//! Each value is shown as its type reads (a date in the reader's format, a
//! choice or several by their labels, marked when retired, a record as its
//! pill with the reader's own access); every save goes through
//! `core.person.update` or `core.person.create`, which check it against the
//! tenant's definitions.
use super::{render, Pages};
use bilibi_custom_fields::{codes_of, Definition, Kind};
use bilibi_database::{Error, Result};
use bilibi_datetime::DisplayContext;
use bilibi_records::{Reference, Resolved};
use bilibi_tenancy::Scope;
use bilibi_ui::{
    Field, Html, InlineChoice, InlineEdit, InlineField, InputKind, MultiSelect, PickerHit,
    RecordPicker,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// The form a record field's picker posts in on the record page: its search
/// as `custom[cf_<key>_search]`.
pub(super) const REFERENCE_FORM: &str = "custom";
/// The record page's picker search event (the picker's form `change`).
pub(super) const REFERENCE_SEARCH: &str = "custom_reference_search";

/// The tenant's active person fields, in their order.
pub(super) async fn active(pages: &Pages, scope: &Scope) -> Result<Vec<Definition>> {
    let client = pages.db.get().await?;
    crate::custom::fields()?
        .active(&**client, scope, crate::custom::RECORD_TYPE)
        .await
}

/// A stored scalar value as text (an absent one is empty).
pub(super) fn text(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// The text an inline fact carries as its value: the held codes joined by
/// `,` for several choices (the inline editor's shape), the stored text
/// otherwise.
pub(super) fn inline_value(definition: &Definition, value: Option<&Value>) -> String {
    match definition.kind {
        Kind::Choices => codes_of(value).join(","),
        Kind::Text
        | Kind::Number
        | Kind::Date
        | Kind::Choice
        | Kind::Boolean
        | Kind::LongText
        | Kind::Link
        | Kind::Phone
        | Kind::Email
        | Kind::Reference => text(value),
    }
}

/// The events every record field's picker raises when a hit is chosen and
/// when its choice is cleared, with the field's name (`cf_<key>`) as `key`:
/// fixed names, so the page declares them with their contract.
pub(super) const PICK_EVENT: &str = "pick_custom_reference";
pub(super) const CLEAR_EVENT: &str = "clear_custom_reference";

/// The records the record fields of a page hold, named with the reader's own
/// access in one `base/records` call. A record the reader cannot see, or a
/// removed one, is unnamed and shows as unavailable.
#[derive(Default)]
pub(super) struct Names(HashMap<Reference, Resolved>);
impl Names {
    /// Names every record `definitions`' record fields hold in `values`.
    pub(super) async fn read<'a>(
        scope: &Scope,
        definitions: &[&Definition],
        values: impl IntoIterator<Item = &'a crate::Custom>,
    ) -> Result<Self> {
        let fields: Vec<(&str, &str)> = definitions
            .iter()
            .filter(|definition| definition.kind == Kind::Reference)
            .filter_map(|definition| Some((definition.key.as_str(), definition.target.as_deref()?)))
            .collect();
        if fields.is_empty() {
            return Ok(Self::default());
        }
        let mut wanted = BTreeSet::new();
        for custom in values {
            for (key, target) in &fields {
                if let Some(id) = custom.get(*key).and_then(Value::as_str) {
                    if !id.is_empty() {
                        wanted.insert(Reference::new(*target, id));
                    }
                }
            }
        }
        if wanted.is_empty() {
            return Ok(Self::default());
        }
        let wanted: Vec<Reference> = wanted.into_iter().collect();
        let resolved = crate::custom::fields()?
            .records()
            .resolve(scope, &wanted)
            .await?;
        Ok(Self(
            resolved
                .into_iter()
                .map(|resolved| (resolved.reference.clone(), resolved))
                .collect(),
        ))
    }
    /// The held record of a record field, when it holds one.
    fn get(&self, definition: &Definition, value: Option<&Value>) -> Option<&Resolved> {
        let id = value.and_then(Value::as_str).filter(|id| !id.is_empty())?;
        self.0
            .get(&Reference::new(definition.target.as_deref()?, id))
    }
}

/// What one page render needs to show values: the reader's display
/// settings, resolved once, and the named records.
pub(super) struct Shown<'a> {
    pub pages: &'a Pages,
    pub scope: &'a Scope,
    pub display: &'a DisplayContext,
    pub names: &'a Names,
    /// The page is framed in a workspace tile (a pill selects its record in
    /// the following tile).
    pub framed: bool,
}

/// A record field's picker on the record page: the text typed so far, its
/// hits and the refusal of the last save.
pub(super) struct Picking<'a> {
    pub query: &'a str,
    pub hits: &'a [PickerHit],
    pub errors: Vec<String>,
}

impl Shown<'_> {
    /// How a value reads, as text: a choice or several by their labels
    /// (marked when retired), yes/no, a date in the reader's format, anything
    /// else as stored; `—` when empty. A record field is a pill (`html`).
    async fn words(&self, definition: &Definition, value: Option<&Value>) -> Result<String> {
        let shown = match definition.kind {
            Kind::Choice | Kind::Choices => {
                let client = self.pages.db.get().await?;
                crate::custom::fields()?
                    .choice_text(&**client, self.scope, definition, value)
                    .await?
            }
            Kind::Boolean => match text(value).as_str() {
                "true" => "Yes".into(),
                "false" => "No".into(),
                other => other.to_owned(),
            },
            Kind::Date => {
                let value = text(value);
                match value.parse::<jiff::civil::Date>() {
                    Ok(date) => self.display.render_date(date),
                    Err(_) => value,
                }
            }
            Kind::Text
            | Kind::Number
            | Kind::LongText
            | Kind::Link
            | Kind::Phone
            | Kind::Email
            | Kind::Reference => text(value),
        };
        Ok(if shown.is_empty() {
            "—".into()
        } else {
            shown
        })
    }

    /// How a value reads on a page: as `words`, a record field as the shared
    /// pill (`dom_id`), never its id; `—` when empty.
    pub(super) async fn html(
        &self,
        definition: &Definition,
        value: Option<&Value>,
        dom_id: &str,
    ) -> Result<Html> {
        match definition.kind {
            Kind::Reference => match self.names.get(definition, value) {
                Some(resolved) => render(resolved.pill(dom_id, self.framed)),
                None => Ok(Html::text("—")),
            },
            Kind::Text
            | Kind::Number
            | Kind::Date
            | Kind::Choice
            | Kind::Boolean
            | Kind::LongText
            | Kind::Link
            | Kind::Phone
            | Kind::Email
            | Kind::Choices => Ok(Html::text(&self.words(definition, value).await?)),
        }
    }

    /// The record page's fact for one field, edited in place by
    /// `save_custom_field`. A record field is its pill, and for an editor
    /// (`picking`) a picker whose choice saves at once.
    pub(super) async fn fact(
        &self,
        definition: &Definition,
        value: Option<&Value>,
        mut field: InlineField,
        picking: Option<Picking<'_>>,
    ) -> Result<Html> {
        if let Some(help) = &definition.help {
            field = field.hint(help.clone());
        }
        let held = inline_value(definition, value);
        match definition.kind {
            Kind::Choice | Kind::Choices => {
                let codes: Vec<&str> = match definition.kind {
                    Kind::Choices => codes_of(value),
                    _ => value.and_then(Value::as_str).into_iter().collect(),
                };
                let options = options(self.pages, self.scope, definition, &codes).await?;
                let editor = if definition.kind == Kind::Choices {
                    field = field.allow_empty();
                    render(
                        MultiSelect::new(format!("{}-select", field.id), &field.name)
                            .in_fact()
                            .label(&definition.label)
                            .options(options)
                            .values(codes.iter().map(|code| (*code).to_owned()).collect()),
                    )?
                } else {
                    let mut choices = vec![bilibi_ui::Choice::new("—".to_owned(), String::new())];
                    choices.extend(options);
                    render(
                        Field::named(&field.name)
                            .id(format!("{}-select", field.id))
                            .label_class("sr-only")
                            .label(&definition.label)
                            .kind(InputKind::Select)
                            .value(&held)
                            .options(choices),
                    )?
                };
                render(InlineChoice {
                    display: Html::text(&self.words(definition, value).await?),
                    field,
                    editor,
                })
            }
            Kind::Reference => {
                let pill = self.names.get(definition, value).map(|resolved| {
                    (
                        format!("{}:{}", resolved.reference.kind, resolved.id),
                        resolved.pill(format!("{}-pill", field.id), self.framed),
                    )
                });
                let Some(picking) = picking else {
                    return match pill {
                        Some((_, pill)) => render(pill),
                        None => Ok(Html::text("—")),
                    };
                };
                let picker = render(
                    RecordPicker::new(
                        &field.id,
                        REFERENCE_FORM,
                        &field.name,
                        &definition.label,
                        picking.query,
                        picking.hits,
                        pill.as_ref()
                            .map(|(value, pill)| (value.as_str(), pill.clone())),
                        (PICK_EVENT, CLEAR_EVENT),
                        picking.errors,
                    )
                    .key(&field.name)
                    // Clearing saves at once on the record page.
                    .in_fact(),
                )?;
                render(bilibi_ui::Form {
                    id: &format!("{}-form", field.id),
                    event: REFERENCE_SEARCH,
                    change: true,
                    body: picker,
                    ..bilibi_ui::Form::default()
                })
            }
            Kind::LongText => render(bilibi_ui::InlineLongText { field, rows: 5 }),
            Kind::Boolean => render(InlineChoice {
                editor: render(
                    Field::named(&field.name)
                        .id(format!("{}-select", field.id))
                        .label(&definition.label)
                        .kind(InputKind::Switch)
                        .value(&held),
                )?,
                field,
                display: Html::text(match held.as_str() {
                    "true" => "Yes",
                    "false" => "No",
                    _ => "—",
                }),
            }),
            Kind::Email => render(InlineEdit {
                field: field.input_type("email"),
            }),
            Kind::Phone => render(InlineEdit {
                field: field.input_type("tel"),
            }),
            Kind::Link => render(InlineEdit {
                field: field.input_type("url"),
            }),
            Kind::Date => render(InlineEdit {
                field: field
                    .input_type("date")
                    .display_value(self.words(definition, value).await?),
            }),
            Kind::Text | Kind::Number => render(InlineEdit { field }),
        }
    }
}

/// A choice or several-choices field's options: the list's active values and
/// the held ones it no longer offers, marked retired.
async fn options(
    pages: &Pages,
    scope: &Scope,
    definition: &Definition,
    held: &[&str],
) -> Result<Vec<bilibi_ui::Choice>> {
    let client = pages.db.get().await?;
    crate::custom::fields()?
        .choice_options(&**client, scope, definition, held)
        .await
}

/// What the create dialog holds for one field: the typed text, the ticked
/// codes of several choices, or a record field's search, hits and choice.
pub(super) struct Entered<'a> {
    pub text: &'a str,
    pub codes: &'a [String],
    pub query: &'a str,
    pub hits: &'a [PickerHit],
    pub chosen: Option<&'a Resolved>,
}

/// The create form's control for one field, named `cf_<key>` in form
/// `person`: several choices post every tick (`person[cf_<key>][]`), a
/// record field posts its choice as `kind:id` and its search as
/// `person[cf_<key>_search]`, raising `pick_custom_reference` and `clear_custom_reference` with `key`.
pub(super) async fn control(
    pages: &Pages,
    scope: &Scope,
    definition: &Definition,
    entered: Entered<'_>,
    errors: Vec<String>,
    enabled: bool,
) -> Result<Html> {
    let name = crate::custom::name(&definition.key);
    let id = format!("person-{}", name.replace('_', "-"));
    let field = |kind: InputKind| {
        let mut field = Field::in_form("person", &name)
            .id(&id)
            .label(&definition.label)
            .value(entered.text)
            .errors(errors.clone())
            .kind(kind)
            .attr("data-debounce", "300");
        if let Some(help) = &definition.help {
            field = field.hint(help);
        }
        if !enabled {
            field = field.disabled();
        }
        field
    };
    match definition.kind {
        Kind::Choice => {
            let mut choices = vec![bilibi_ui::Choice::new("—".to_owned(), String::new())];
            choices.extend(options(pages, scope, definition, &[entered.text]).await?);
            render(field(InputKind::Select).options(choices))
        }
        Kind::Choices => {
            let codes: Vec<&str> = entered.codes.iter().map(String::as_str).collect();
            let mut select = MultiSelect::in_form("person", &name)
                .id(&id)
                .label(&definition.label)
                .options(options(pages, scope, definition, &codes).await?)
                .values(entered.codes.to_vec())
                .errors(errors);
            if let Some(help) = &definition.help {
                select = select.hint(help);
            }
            select.attrs.disabled = !enabled;
            render(select)
        }
        Kind::Reference => {
            let chosen = entered.chosen.map(|resolved| {
                (
                    format!("{}:{}", resolved.reference.kind, resolved.id),
                    resolved.pill(format!("{id}-pill"), false),
                )
            });
            render(
                RecordPicker::new(
                    &id,
                    "person",
                    &name,
                    &definition.label,
                    entered.query,
                    entered.hits,
                    chosen
                        .as_ref()
                        .map(|(value, pill)| (value.as_str(), pill.clone())),
                    (PICK_EVENT, CLEAR_EVENT),
                    errors,
                )
                .key(&name),
            )
        }
        Kind::Date => render(field(InputKind::Date)),
        Kind::Number => render(field(InputKind::Text).attr("inputmode", "decimal")),
        Kind::LongText => render(field(InputKind::Textarea).attr("rows", "5")),
        Kind::Boolean => render(field(InputKind::Switch)),
        Kind::Email => render(field(InputKind::Email)),
        Kind::Phone => render(field(InputKind::Tel)),
        Kind::Link => render(field(InputKind::Url)),
        Kind::Text => render(field(InputKind::Text)),
    }
}

/// A record field's picker hits for `query` (none for a blank one), among
/// the records the reader may select.
pub(super) async fn hits(
    scope: &Scope,
    definition: &Definition,
    query: &str,
) -> Result<Vec<PickerHit>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    crate::custom::fields()?
        .reference_hits(scope, definition, query)
        .await
}

/// The record a picker raised (`kind:id`) for a record field, named with
/// the reader's access; `Invalid` when it is not of the field's kind.
pub(super) async fn chosen(
    scope: &Scope,
    definition: &Definition,
    raised: &str,
) -> Result<Resolved> {
    let id = bilibi_custom_fields::picked(definition, raised).ok_or(Error::Invalid)?;
    crate::custom::fields()?
        .references(scope, definition, &[&id])
        .await?
        .into_iter()
        .next()
        .ok_or(Error::Internal)
}

/// The list offers the first of the tenant's fields (in their order) as
/// columns, sorts and filters.
pub(super) const LIST_MAX: usize = 50;

/// One filter control's URL key and the condition it makes: a text, choice
/// or several-choices field has one (`cf_<key>`), a number or date field a
/// lower and an upper bound (`cf_<key>-min`/`-max`, `cf_<key>-from`/`-to`),
/// a record field none. A key never holds `-`, so no two fields' controls
/// share a name.
pub(super) fn filter_keys(definition: &Definition) -> Vec<(String, &'static str)> {
    let name = crate::custom::name(&definition.key);
    match definition.kind {
        Kind::Text | Kind::LongText | Kind::Link | Kind::Phone | Kind::Email => {
            vec![(name, "contains")]
        }
        Kind::Boolean => vec![(name, "eq")],
        Kind::Choice => vec![(name, "eq")],
        Kind::Choices => vec![(name, "has")],
        Kind::Number => vec![
            (format!("{name}-min"), "gte"),
            (format!("{name}-max"), "lte"),
        ],
        Kind::Date => vec![
            (format!("{name}-from"), "gte"),
            (format!("{name}-to"), "lte"),
        ],
        Kind::Reference => Vec::new(),
    }
}

/// The list's spec with each field's sort and filter keys added; several
/// choices and a record do not sort.
pub(super) fn spec(base: &bilibi_ui::ListSpec, definitions: &[Definition]) -> bilibi_ui::ListSpec {
    let mut spec = base.clone();
    for definition in definitions {
        if crate::custom::sortable(definition.kind) {
            spec.sortable.insert(
                crate::custom::name(&definition.key),
                bilibi_ui::SortDir::Asc,
            );
        }
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
    filters: &BTreeMap<String, String>,
    definitions: &[Definition],
) -> (Vec<crate::CustomFilter>, BTreeMap<String, String>) {
    let mut out = Vec::new();
    let mut errors = BTreeMap::new();
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
            if crate::custom::condition(definition, &filter, 1, 2).is_some() {
                out.push(filter);
            } else {
                errors.insert(
                    key,
                    match definition.kind {
                        Kind::Number => "Enter a number such as 12 or -3.5; not applied.",
                        Kind::Date => "Enter a date; not applied.",
                        Kind::Text
                        | Kind::Choice
                        | Kind::Boolean
                        | Kind::LongText
                        | Kind::Link
                        | Kind::Phone
                        | Kind::Email
                        | Kind::Choices
                        | Kind::Reference => "Enter up to 256 characters; not applied.",
                    }
                    .to_owned(),
                );
            }
        }
    }
    (out, errors)
}

/// The filter sheet's controls for the fields: text contains, a choice (or
/// one of several choices held) from its list, a number or a date between
/// two bounds.
pub(super) async fn controls(
    pages: &Pages,
    scope: &Scope,
    definitions: &[Definition],
    form: &HashMap<String, String>,
    errors: &BTreeMap<String, String>,
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
            let any = || {
                bilibi_ui::Choice::new(
                    format!("Any {}", definition.label.to_lowercase()),
                    String::new(),
                )
            };
            let mut control = match definition.kind {
                Kind::Choice | Kind::Choices => {
                    let mut choices = vec![any()];
                    // Active values only: a filter offers no retired one.
                    choices.extend(options(pages, scope, definition, &[]).await?);
                    ToolbarControl::select(&id, &key, &label, value, choices)
                }
                Kind::Boolean => ToolbarControl::select(
                    &id,
                    &key,
                    &label,
                    value,
                    vec![
                        any(),
                        bilibi_ui::Choice::new("Yes", "true"),
                        bilibi_ui::Choice::new("No", "false"),
                    ],
                ),
                Kind::Date => ToolbarControl::date(&id, &key, &label, value),
                Kind::Number => ToolbarControl::text(&id, &key, &label, value).numeric(),
                Kind::Text
                | Kind::LongText
                | Kind::Link
                | Kind::Phone
                | Kind::Email
                | Kind::Reference => ToolbarControl::text(&id, &key, &label, value),
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

/// Each field's value as the list shows it, per person on the page: a
/// choice or several by their labels (marked when retired), a date in the
/// reader's format, a record as its pill (named once for the page, in
/// `shown.names`). One label read per distinct value on the page.
/// `definitions` are the fields whose column is on screen.
pub(super) async fn cells(
    shown: &Shown<'_>,
    definitions: &[&Definition],
    people: &[crate::Person],
) -> Result<BTreeMap<(i64, String), Html>> {
    let mut labels: BTreeMap<(String, String), Html> = Default::default();
    let mut out = BTreeMap::new();
    for definition in definitions {
        let name = crate::custom::name(&definition.key);
        for person in people {
            let Some(value) = person.custom.get(&definition.key) else {
                continue;
            };
            let cell = if definition.kind == Kind::Reference {
                let dom_id = format!("person-{}-{}", person.id.get(), name.replace('_', "-"));
                shown.html(definition, Some(value), &dom_id).await?
            } else {
                let label_key = (definition.key.clone(), value.to_string());
                match labels.get(&label_key) {
                    Some(cell) => cell.clone(),
                    None => {
                        let cell = shown.html(definition, Some(value), "").await?;
                        labels.insert(label_key, cell.clone());
                        cell
                    }
                }
            };
            out.insert((person.id.get(), name.clone()), cell);
        }
    }
    Ok(out)
}
