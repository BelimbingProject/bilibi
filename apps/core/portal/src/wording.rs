//! The wording of the mail this module sends, as shared templates
//! (`apps/base/templates/README.md`). Each link and its place in the message
//! stay in code.
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_templates::{
    Audience, DefaultText, Output, Slot, Template, Text, Variable, PLATFORM_CATALOGUE,
};

pub(crate) const REGISTRATION: &str = "core.portal.registration_confirmation";
pub(crate) const INVITATION: &str = "core.portal.invitation";

macro_rules! provenance {
    () => {
        Provenance {
            module: "core/portal",
            source: file!(),
            line: line!(),
        }
    };
}

/// The text of a message whose link stands alone between two passages.
pub(crate) fn around_link(text: &Text, link: &str) -> String {
    format!("{}\n\n{link}\n\n{}\n", text["intro"], text["closing"])
}

const SLOTS: &[Slot] = &[
    Slot::subject(),
    Slot::text("intro", "Text before the link"),
    Slot::text("closing", "Text after the link"),
];
const ALL: &[&str] = &["subject", "intro", "closing"];
const VARIABLES: &[Variable] = &[
    Variable::text(
        "site_title",
        "Portal name",
        "Example Trading customer portal",
        ALL,
    ),
    Variable::text("days", "Days the link works", "7", ALL),
];

inventory::submit! { Contribution::new(PLATFORM_CATALOGUE, REGISTRATION, provenance!(), &Template {
    key: REGISTRATION,
    label: "Customer account request confirmation email",
    audience: Audience::Customer,
    outputs: &[Output::PlainText],
    slots: SLOTS,
    variables: VARIABLES,
    base_language: "en",
    defaults: &[DefaultText { language: "en", version: "1", slots: &[
        ("subject", "Confirm your request for {{site_title}}"),
        ("intro", "You asked for an account on {{site_title}}.\n\nOpen this link to confirm your email address:"),
        ("closing", "The link expires in {{days}} day(s). {{site_title}} will then review your request. If you did not ask for this, you can ignore this email."),
    ] }],
}) }

inventory::submit! { Contribution::new(PLATFORM_CATALOGUE, INVITATION, provenance!(), &Template {
    key: INVITATION,
    label: "Customer invitation email",
    audience: Audience::Customer,
    outputs: &[Output::PlainText],
    slots: SLOTS,
    variables: VARIABLES,
    base_language: "en",
    defaults: &[DefaultText { language: "en", version: "1", slots: &[
        ("subject", "You're invited to {{site_title}}"),
        ("intro", "You are invited to {{site_title}}.\n\nOpen this link to choose a password and finish setting up your account:"),
        ("closing", "The link works once and expires in {{days}} day(s). If you did not expect this, you can ignore this email."),
    ] }],
}) }
