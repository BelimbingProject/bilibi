/// The wording of the scheduled-report email, as a shared template
/// (`apps/base/templates/README.md`). The download link stays in code.
pub(crate) const SCHEDULED_REPORT: &str = "base.reports.scheduled_report";
inventory::submit! { bilibi_module_registry::Contribution::new(bilibi_templates::CATALOGUE, SCHEDULED_REPORT, bilibi_module_registry::Provenance {
    module: "base/reports", source: file!(), line: line!(),
}, &bilibi_templates::Template {
    key: SCHEDULED_REPORT,
    label: "Scheduled report email",
    audience: bilibi_templates::Audience::Staff,
    outputs: &[bilibi_templates::Output::PlainText],
    slots: &[
        bilibi_templates::Slot::subject(),
        bilibi_templates::Slot::text("intro", "Text before the download link"),
    ],
    variables: &[
        bilibi_templates::Variable::text("report_name", "Report name", "Monthly sales", &["subject", "intro"]),
    ],
    base_language: "en",
    defaults: &[bilibi_templates::DefaultText { language: "en", version: "1", slots: &[
        ("subject", "Scheduled report: {{report_name}}"),
        ("intro", "Your scheduled report {{report_name}} is ready.\n\nDownload it, signed in, before it expires:"),
    ] }],
}) }
