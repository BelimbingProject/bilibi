//! The wording of the mail this module sends, as shared templates
//! (`apps/base/templates/README.md`). Which lines a message has, their order
//! and its link stay in code; an activity's own description is data and is
//! never a template.
use crate::Activity;
use bilibi_database::Result;
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_templates::{
    bounded, Audience, DefaultText, Output, Slot, Template, Templates, Value, Variable, CATALOGUE,
};
use bilibi_tenancy::ReadClient;

pub(crate) const INVITATION: &str = "core.activity.invitation";
pub(crate) const GUEST_REMINDER: &str = "core.activity.guest_reminder";
pub(crate) const REMINDER: &str = "core.activity.reminder";

/// A title as a message shows it: short enough for a subject line.
pub(crate) fn title(title: &str) -> String {
    bounded(title, 150)
}

/// The subject and text of a guest's message about `activity`: what, when,
/// where and the join link, the description when `described`, and who sent
/// it. Internal notes are never in it.
pub(crate) async fn guest_mail(
    templates: &Templates,
    client: &dyn ReadClient,
    tenant: i64,
    key: &str,
    (activity, sender, when): (&Activity, &str, &str),
    described: bool,
) -> Result<(String, String)> {
    let kind = activity.kind.label().to_lowercase();
    let title = title(&activity.title);
    let mut values = vec![
        ("sender_name", Value::Text(sender)),
        ("kind", Value::Text(&kind)),
        ("title", Value::Text(&title)),
        ("when", Value::Text(when)),
    ];
    if let Some(location) = &activity.location {
        values.push(("location", Value::Text(location)));
    }
    if let Some(url) = &activity.join_url {
        values.push(("join_url", Value::Text(url)));
    }
    let text = templates
        .tenant_wording(client, tenant, key, &values)
        .await?;
    let mut body = format!("{}\n\n{}\n", text["intro"], text["when_line"]);
    if activity.location.is_some() {
        body.push_str(&format!("{}\n", text["where_line"]));
    }
    if activity.join_url.is_some() {
        body.push_str(&format!("{}\n", text["join_line"]));
    }
    if let Some(description) = activity.description.as_ref().filter(|_| described) {
        body.push_str(&format!("\n{description}\n"));
    }
    body.push_str(&format!("\n{}\n", text["closing"]));
    Ok((text["subject"].clone(), body))
}

macro_rules! provenance {
    () => {
        Provenance {
            module: "core/activity",
            source: file!(),
            line: line!(),
        }
    };
}

const GUEST_SLOTS: &[Slot] = &[
    Slot::subject(),
    Slot::text("intro", "Opening line"),
    Slot::text("when_line", "Line that says when"),
    Slot::text("where_line", "Line that says where"),
    Slot::text("join_line", "Line with the link to join"),
    Slot::text("closing", "Closing text"),
];
const GUEST_ALL: &[&str] = &[
    "subject",
    "intro",
    "when_line",
    "where_line",
    "join_line",
    "closing",
];
const GUEST_VARIABLES: &[Variable] = &[
    Variable::text("sender_name", "Sender's name", "Ahmad Faiz", GUEST_ALL),
    Variable::text("kind", "Kind of activity", "meeting", GUEST_ALL),
    Variable::text("title", "Title", "Quarterly review", GUEST_ALL),
    Variable::text(
        "when",
        "Date and time",
        "2026-11-20 09:15 to 10:00 (Asia/Kuala_Lumpur)",
        GUEST_ALL,
    ),
    Variable::optional_text("location", "Place", "Meeting room 2", &["where_line"]),
    Variable::optional_text(
        "join_url",
        "Link to join",
        "https://meet.example.test/quarterly",
        &["join_line"],
    ),
];

inventory::submit! { Contribution::new(CATALOGUE, INVITATION, provenance!(), &Template {
    key: INVITATION,
    label: "Activity invitation email",
    audience: Audience::Customer,
    outputs: &[Output::PlainText],
    slots: GUEST_SLOTS,
    variables: GUEST_VARIABLES,
    base_language: "en",
    defaults: &[DefaultText { language: "en", version: "1", slots: &[
        ("subject", "Invitation: {{title}}"),
        ("intro", "{{sender_name}} invites you to a {{kind}}: {{title}}"),
        ("when_line", "When: {{when}}"),
        ("where_line", "Where: {{location}}"),
        ("join_line", "Join: {{join_url}}"),
        ("closing", "Please reply to {{sender_name}} to accept or decline."),
    ] }],
}) }

inventory::submit! { Contribution::new(CATALOGUE, GUEST_REMINDER, provenance!(), &Template {
    key: GUEST_REMINDER,
    label: "Activity reminder email to guests",
    audience: Audience::Customer,
    outputs: &[Output::PlainText],
    slots: GUEST_SLOTS,
    variables: GUEST_VARIABLES,
    base_language: "en",
    defaults: &[DefaultText { language: "en", version: "1", slots: &[
        ("subject", "Reminder: {{title}}"),
        ("intro", "A reminder of the {{kind}} {{sender_name}} invited you to: {{title}}"),
        ("when_line", "When: {{when}}"),
        ("where_line", "Where: {{location}}"),
        ("join_line", "Join: {{join_url}}"),
        ("closing", "If you can no longer come, please reply to {{sender_name}}."),
    ] }],
}) }

const OWN_ALL: &[&str] = &["subject", "intro", "due_line", "starts_line", "link_label"];
inventory::submit! { Contribution::new(CATALOGUE, REMINDER, provenance!(), &Template {
    key: REMINDER,
    label: "Activity reminder email to the assignee",
    audience: Audience::Staff,
    outputs: &[Output::PlainText],
    slots: &[
        Slot::subject(),
        Slot::text("intro", "Opening line"),
        Slot::text("due_line", "Line that says when a task is due"),
        Slot::text("starts_line", "Line that says when a call or meeting starts"),
        Slot::text("link_label", "Text before the link"),
    ],
    variables: &[
        Variable::text("what", "What is due", "Meeting", OWN_ALL),
        Variable::text("title", "Title", "Quarterly review", OWN_ALL),
        Variable::optional_text("when", "Date and time", "2026-11-20 09:15 UTC", &["due_line", "starts_line"]),
    ],
    base_language: "en",
    defaults: &[DefaultText { language: "en", version: "1", slots: &[
        ("subject", "Reminder: {{title}}"),
        ("intro", "{{what}}: {{title}}"),
        ("due_line", "Due {{when}}"),
        ("starts_line", "Starts {{when}}"),
        ("link_label", "Open it, signed in:"),
    ] }],
}) }
