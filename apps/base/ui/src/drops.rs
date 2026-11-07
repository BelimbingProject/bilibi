//! Dragging a record onto a page that can act on it (issue 265). The source
//! carries a reference only (`kind:id`, `FlexRow::record`); a target page
//! wraps itself in a [`DropZone`] (`DropZone::wrap`) and receives a
//! `drop_records` event; the same shortcut is always also an item of the
//! row's "Non-local actions" menu ([`AddToMenu`]), a plain link, so a phone, the
//! keyboard, a screen reader and an AI agent reach it without a drag. Which pages accept which kinds is
//! `base/records`' business (`bilibi_records::DropTarget`); this module only
//! draws it. Browser half: `assets/drops.js`. Contract: `DESIGN.md`.
use crate::Html;
use askama::Template;

/// A page that takes dropped records. Wrap the page's body with
/// [`DropZone::wrap`]; the browser lights the tile while a
/// record it accepts is dragged and sends the drop to the page as the
/// `drop_records` event.
#[derive(Clone, Debug, Default)]
pub struct DropZone {
    /// The record kinds the page accepts.
    pub accepts: Vec<String>,
    /// The most records one drop may carry.
    pub max: usize,
    /// What a drop does, as the tile says it while a record is over it.
    pub label: String,
}
impl DropZone {
    pub fn new(accepts: &[&str], max: usize, label: &str) -> Self {
        Self {
            accepts: accepts.iter().map(|kind| (*kind).to_owned()).collect(),
            max,
            label: label.to_owned(),
        }
    }

    /// `body` (a whole page) inside the zone: one element with the page's
    /// accepted kinds, count and action, which lays out as if it were not
    /// there. Nothing for an inactive zone. `id` is the page's own, stable
    /// across renders so the live patcher keeps the page inside it.
    pub fn wrap(&self, id: &str, body: Html) -> Html {
        if self.accepts.is_empty() {
            return body;
        }
        let escape = |text: &str| Html::text(text).to_string();
        Html::raw(format!(
            "<div id=\"{}\" data-drop-zone data-drop-accepts=\"{}\" data-drop-max=\"{}\" data-drop-label=\"{}\" class=\"contents\">{}</div>",
            escape(id),
            escape(&self.accepts.join(" ")),
            self.max,
            escape(&self.label),
            body,
        ))
    }
}

/// One shortcut in the "Non-local actions" menu: a link to the target page that opens
/// its review on the records. A target that takes fewer records than are
/// ticked stays in the menu, disabled, with the reason as its title.
#[derive(Clone, Debug)]
pub struct AddToItem {
    pub label: String,
    pub href: String,
    pub disabled: Option<String>,
}

/// The "Non-local actions" menu of a row or of a selection bar: an icon-only button
/// (Font Awesome `arrow-up-right-from-square`, tooltip "Non-local actions") that
/// opens the list of pages the record can be taken to. `DisclosureDismiss`
/// closes it on Escape and on a click outside.
///
/// Only actions that take the record to another module or kind of record
/// belong here (`bilibi_records::Records::add_to`). An action on the record
/// itself (edit, delete, archive, an on/off) is an `IconButton` on the row,
/// never an item of this menu (`DESIGN.md`, Lists, Row actions).
#[derive(Template, Clone, Debug)]
#[template(
    source = r##"{%- import "components.html" as ui -%}
{%- if !items.is_empty() -%}
<span id="{{ id }}-wrap" data-hook="DisclosureDismiss" class="floating-scope relative inline-block"><button id="{{ id }}" type="button" aria-expanded="false" aria-controls="{{ id }}-items" aria-label="Non-local actions for {{ subject }}" title="Non-local actions" class="peer floating-anchor {% call ui::icon_button_class(context=bilibi_ui::IconContext::Table, kind=bilibi_ui::IconKind::Neutral, chrome=bilibi_ui::Chrome::Utilities, busy=false, class="") %}{% endcall %} aria-expanded:bg-surface-sunken aria-expanded:text-ink">{% call ui::icon(name="fas-arrow-up-right-from-square", class="size-4") %}{% endcall %}</button><div id="{{ id }}-items" class="floating-list z-40 mt-0.5 hidden min-w-40 rounded-md border border-line bg-surface p-1 text-left shadow-lg peer-aria-expanded:block">{% for item in items %}{% if let Some(reason) = item.disabled %}<span id="{{ id }}-{{ loop.index0 }}" aria-disabled="true" title="{{ reason }}" class="block cursor-not-allowed rounded-sm px-2 py-1 text-xs text-ink-faint">{{ item.label }}</span>{% else %}<a id="{{ id }}-{{ loop.index0 }}" href="{{ item.href }}" class="block rounded-sm px-2 py-1 text-xs text-ink hover:bg-surface-muted focus-visible:bg-surface-muted focus-visible:outline-none">{{ item.label }}</a>{% endif %}{% endfor %}</div></span>
{%- endif -%}"##,
    ext = "html"
)]
pub struct AddToMenu {
    /// A stable id for this menu on the page.
    pub id: String,
    pub items: Vec<AddToItem>,
    /// What the actions act on, for the button's accessible name ("Actions
    /// for Acme Sdn Bhd…"): the record's name, or "the selected contacts".
    pub subject: String,
}
impl AddToMenu {
    pub fn new(id: impl Into<String>, subject: impl Into<String>, items: Vec<AddToItem>) -> Self {
        Self {
            id: id.into(),
            items,
            subject: subject.into(),
        }
    }
}

/// The drag attributes of a record row: `kind:id`, the one spelling the drop
/// event and the link carry. The browser (`assets/drops.js`) reads them.
pub fn drag_attributes(kind: &str, id: &str) -> Html {
    Html::raw(format!(
        " draggable=\"true\" data-drag-record=\"{}\"",
        Html::text(&format!("{kind}:{id}"))
    ))
}
