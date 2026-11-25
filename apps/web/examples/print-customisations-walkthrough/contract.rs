use bilibi_base::templates::{
    PrintContract, PrintDefault, PrintField, PrintKind, PrintValue, PRINT_CATALOGUE,
};
use bilibi_module_registry::{Contribution, Provenance};

pub const KEY: &str = "base.templates.production_sheet";
pub const EN: &str = r#"{"blocks":[{"type":"heading","text":"Production sheet"},{"type":"field","field":"reference"}]}"#;
const MS: &str = r#"{"paper":"letter","blocks":[{"type":"heading","text":"Lembaran pengeluaran"},{"type":"field","field":"reference"}]}"#;

static CONTRACT: PrintContract = PrintContract {
    key: KEY,
    label: "Production sheet",
    version: "1",
    states: &["issued"],
    fields: &[PrintField {
        id: "reference",
        kind: PrintKind::Text,
        optional: false,
        required_output: true,
        labels: &[("en", "Reference"), ("ms", "Rujukan")],
        sample: PrintValue::Text("WS-2026-0240"),
    }],
    base_language: "en",
    defaults: &[
        PrintDefault {
            language: "en",
            version: "1",
            layout: EN,
        },
        PrintDefault {
            language: "ms",
            version: "1",
            layout: MS,
        },
    ],
};

inventory::submit! { Contribution::new(PRINT_CATALOGUE, KEY, Provenance {
    module: "base/templates", source: file!(), line: line!(),
}, &CONTRACT) }
