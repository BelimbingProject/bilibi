//! A tag in the status bar's centre that opens a panel: the AI tag Core Agent
//! Access contributes through the `shell.ai` embed. The tag is drawn like the
//! instance status (`templates/feedback.html`, `statusbar_tag`): the same text
//! in a bordered box in every state, so the border weight and the title repeat
//! the state and colour is never the only cue. The panel (`statusbar_panel`)
//! opens upward over the bar, spans the viewport on a phone, and its open or
//! closed state belongs to the shell's disclosure controller
//! (`assets/shell.js`), as the bell's dropdown does.
use askama::Template;

/// The button in the status bar. `quiet` draws the faint border of an idle
/// state (no AI app connected). The `on`, `warning` and `error` states use
/// their matching semantic colours; other active states are neutral.
/// `title` is the tooltip and the accessible name, so it names the state in
/// words ("No AI app connected" or "Claude · last used 5 min ago").
#[derive(Template, Debug, Default)]
#[template(
    source = r#"{% import "feedback.html" as ui %}{% call ui::statusbar_tag(tag=self) %}{% endcall %}"#,
    ext = "html"
)]
pub struct StatusbarTag<'a> {
    pub id: &'a str,
    pub label: &'a str,
    pub title: &'a str,
    /// The id of the panel it opens (`aria-controls`).
    pub controls: &'a str,
    /// A word for the state, stamped as `data-state` for styling and proof.
    pub state: &'a str,
    pub quiet: bool,
}

/// A shell-mounted status bar tag and its shared disclosure panel. Keep the
/// root even when empty so the live shell can show a newly arrived status.
#[derive(Template, Debug)]
#[template(
    source = r#"{% import "feedback.html" as fb %}<div id="{{ id }}" data-target="{{ id }}" class="relative flex shrink-0 items-center">{% if visible %}{{ tag|safe }}{% call fb::statusbar_panel(id=panel_id, label=label) %}<div class="max-h-[70vh] space-y-2 overflow-y-auto p-3">{{ body|safe }}</div>{% endcall %}{% endif %}</div>"#,
    ext = "html"
)]
pub struct StatusbarPanel<'a> {
    pub id: &'a str,
    pub panel_id: &'a str,
    pub label: &'a str,
    pub visible: bool,
    pub tag: crate::Html,
    pub body: crate::Html,
}
