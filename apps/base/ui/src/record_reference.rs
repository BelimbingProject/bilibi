//! A reference to another module's record, shown and chosen the same way on
//! every page: the `RelatedRecord` pill names and links it (or shows it as
//! unavailable), and the `RecordPicker` live control finds one by name.
//! `base/records` resolves names and hits with the reader's permissions; this
//! module only draws them, so a page writes no markup of its own.
use crate::{forms::InputKind, Button, Choice, Combobox, Field, Html, Restricted};
use askama::Template;

/// How a reference reads for this viewer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordState {
    /// The viewer may see it: its name and, when the kind has one, its page.
    Named { label: String, href: Option<String> },
    /// Hidden from this viewer, removed, or its module is not installed.
    Unavailable,
}

/// One reference as a pill. With a page, the name is a `RecordLink`: a link
/// that opens the record, or inside a workspace tile a control that selects
/// it in the following tile. A long name stays on one
/// line, at most 16rem (60% of a phone's width), and ends in "…", with the
/// whole name as the pill's title, so a table cell never grows to fit it.
#[derive(Template, Clone, Debug)]
#[template(
    source = r#"{%- import "components.html" as ui -%}{%- import "lists.html" as lists -%}
{%- match state -%}
{%- when RecordState::Named with { label, href } -%}
{%- let record_kind = kind -%}{%- let record_href = href -%}
<span id="{{ id }}" data-related-record title="{{ label }}" class="inline-flex min-w-0 max-w-[min(16rem,60vw)] align-middle">{% call ui::badge(kind=crate::BadgeKind::Neutral, dot=false, class="min-w-0 max-w-full normal-case") %}{% match record_href %}{% when Some with (target) %}{% call lists::record_link(workspace=workspace, kind=record_kind, record_id=record_id, href=target, class="min-w-0 truncate hover:underline") %}{{ label }}{% endcall %}{% when None %}<span class="min-w-0 truncate">{{ label }}</span>{% endmatch %}{% endcall %}</span>
{%- when RecordState::Unavailable -%}
<span id="{{ id }}" data-related-record>{{ unavailable }}</span>
{%- endmatch -%}"#,
    ext = "html"
)]
pub struct RelatedRecord {
    /// A stable id for this pill on the page.
    pub id: String,
    /// The page is framed: the name selects the record in a following tile.
    pub workspace: bool,
    /// The workspace kind: the module that registered the record kind.
    pub kind: String,
    pub record_id: String,
    pub state: RecordState,
    unavailable: Html,
}
impl RelatedRecord {
    pub fn new(
        id: impl Into<String>,
        kind: impl Into<String>,
        record_id: impl Into<String>,
        state: RecordState,
    ) -> Self {
        let id = id.into();
        let marker = Restricted {
            reason: "Not available to you: hidden, removed, or its module is not installed.".into(),
            ..Restricted::new(format!("{id}-unavailable"))
        };
        Self {
            unavailable: Html::render(&marker).unwrap_or(Html::EMPTY),
            id,
            workspace: false,
            kind: kind.into(),
            record_id: record_id.into(),
            state,
        }
    }
    /// The page is framed in a workspace tile.
    pub fn workspace(mut self, framed: bool) -> Self {
        self.workspace = framed;
        self
    }
}

/// One record a picker offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerHit {
    /// `kind:id`, what choosing it raises.
    pub value: String,
    pub label: String,
    /// The singular kind name for a search across kinds ("Company").
    /// Empty when the field searches only one kind.
    pub kind_label: String,
}

/// One kind of record a [`RecordPicker::named`] offers: its Base Records kind,
/// the prefix a hit raises (`<prefix>:<id>`), its name for people and, when it
/// has one, the page a chosen record links to (`<page>/<id>`).
#[derive(Clone, Copy, Debug)]
pub struct NamedKind<'a> {
    pub kind: &'a str,
    pub prefix: &'a str,
    pub label: &'a str,
    pub page: Option<&'a str>,
}
impl NamedKind<'_> {
    /// The id a hit of this kind raised as `record`, if it is one.
    pub fn picked(&self, record: &str) -> Option<i64> {
        record
            .strip_prefix(self.prefix)?
            .strip_prefix(':')?
            .parse()
            .ok()
            .filter(|id| *id > 0)
    }
}

/// A live type-ahead over the kinds a field allows. It sits in the owner's
/// `Form` with `change`: typing raises the form's event, the owner searches
/// (`base/records` `search`) and renders the hits; choosing one raises
/// `pick_event` with `record=kind:id`, and the owner keeps it as `selected`.
/// The chosen `kind:id` posts with the form as `{form}[{key}]`. The owner
/// still checks it on save; a chosen value is never proof.
#[derive(Template, Clone, Debug)]
#[template(
    source = r#"{%- import "forms.html" as forms -%}<div id="{{ id }}" data-record-picker class="space-y-1">
{%- if let Some(pill) = selected -%}
<div class="flex flex-wrap items-center gap-2"><span class="{% if in_fact %}sr-only{% else %}text-xs text-ink-muted{% endif %}">{{ label }}</span>{{ pill }}{{ self.clear() }}</div>{{ hidden }}{% call forms::errors(id=id, errors=errors) %}{% endcall %}
{%- else -%}
{%- let combo = self.search_combobox() -%}
{% call forms::combobox(combo=combo, server_search=true, query=query, pick_event=pick_event, key=key) %}{% endcall %}
{%- endif -%}
</div>"#,
    ext = "html"
)]
pub struct RecordPicker {
    pub id: String,
    pub label: String,
    query: String,
    selected: Option<Html>,
    search: Combobox,
    hidden: Html,
    clear_id: String,
    clear_event: String,
    clear_label: &'static str,
    in_fact: bool,
    pick_event: String,
    /// Each hit's `kind:id` and text.
    hits: Vec<(String, String, String)>,
    /// Sent with a pick or a clear as `key`: the field this picker serves.
    key: String,
    /// Shown under the chosen record; while searching, the search field shows them.
    errors: Vec<String>,
}
impl RecordPicker {
    pub fn search_name(mut self, name: impl Into<String>) -> Self {
        self.search.name = name.into();
        self
    }
    fn search_combobox(&self) -> Combobox {
        self.search
            .clone()
            .options(
                self.hits
                    .iter()
                    .map(|(value, text, detail)| Choice::new(text, value).detail(detail))
                    .collect(),
            )
            .errors(self.errors.clone())
    }

    /// `form` and `key` name the posted fields: the typed text as
    /// `{form}[{key}_search]`, the choice as `{form}[{key}]`.
    pub fn new(
        id: &str,
        form: &str,
        key: &str,
        label: &str,
        query: &str,
        hits: &[PickerHit],
        selected: Option<(&str, RelatedRecord)>,
        events: (&str, &str),
        errors: Vec<String>,
    ) -> Self {
        let (pick_event, clear_event) = events;
        let search = Combobox::new(format!("{id}-search"), format!("{form}[{key}_search]"))
            .label(label)
            .errors(errors.clone());
        let hits = hits
            .iter()
            .map(|hit| (hit.value.clone(), hit.label.clone(), hit.kind_label.clone()))
            .collect();
        let (selected, hidden) = match selected {
            Some((value, pill)) => (
                Some(Html::render(&pill).unwrap_or(Html::EMPTY)),
                Html::render(
                    &Field::in_form(form, key)
                        .id(format!("{id}-value"))
                        .kind(InputKind::Hidden)
                        .value(value),
                )
                .unwrap_or(Html::EMPTY),
            ),
            None => (None, Html::EMPTY),
        };
        Self {
            id: id.into(),
            label: label.into(),
            query: query.into(),
            selected,
            search,
            hidden,
            clear_id: format!("{id}-clear"),
            clear_event: clear_event.into(),
            clear_label: "Change",
            in_fact: false,
            pick_event: pick_event.into(),
            hits,
            key: String::new(),
            errors,
        }
    }
    /// The picker is a record page's fact, which already names it and saves
    /// a pick or a clear at once: the label beside the chosen record is for
    /// screen readers only, and the clear button says "Remove" rather than
    /// "Change", a change it would not make by itself.
    pub fn in_fact(mut self) -> Self {
        self.in_fact = true;
        self.clear_label = "Remove";
        self
    }
    /// One page holding several pickers under the same events (a record's
    /// custom fields): each pick and clear also sends `key`, so the events
    /// stay fixed names a page can declare.
    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = key.into();
        self
    }
    fn clear(&self) -> Html {
        Html::render(&Button {
            id: &self.clear_id,
            button_type: "button",
            event: &self.clear_event,
            data_key: &self.key,
            body: Html::text(self.clear_label),
            ..Button::default()
        })
        .unwrap_or(Html::EMPTY)
    }
    /// A picker whose hits and choice the owner names itself, as `(id, name)`
    /// pairs of one `kind` (a directory search rather than Base Records): each
    /// hit raises `record=<prefix>:<id>` and the chosen record posts its bare
    /// id as `{form}[{key}]`.
    pub fn named(
        kind: NamedKind<'_>,
        id: &str,
        form: &str,
        key: &str,
        label: &str,
        query: &str,
        hits: &[(i64, String)],
        chosen: Option<(i64, &str)>,
        events: (&str, &str),
        errors: Vec<String>,
    ) -> Self {
        let hits: Vec<PickerHit> = hits
            .iter()
            .map(|(record, name)| PickerHit {
                value: format!("{}:{record}", kind.prefix),
                label: name.clone(),
                kind_label: String::new(),
            })
            .collect();
        let chosen = chosen.map(|(record, name)| {
            let pill = RelatedRecord::new(
                format!("{id}-pill"),
                kind.kind,
                record.to_string(),
                RecordState::Named {
                    label: name.to_owned(),
                    href: kind.page.map(|page| format!("{page}/{record}")),
                },
            );
            (record.to_string(), pill)
        });
        Self::new(
            id,
            form,
            key,
            label,
            query,
            &hits,
            chosen
                .as_ref()
                .map(|(value, pill)| (value.as_str(), pill.clone())),
            events,
            errors,
        )
    }
}
