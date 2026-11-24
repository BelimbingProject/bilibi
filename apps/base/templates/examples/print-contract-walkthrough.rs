//! Synthetic owner for the print compiler and boot checks. Never linked into the app.
use bilibi_module_registry::{Catalog, Consumer, Contribution, Layer, Module, Provenance};
use bilibi_templates::{
    Audience, Origin, PrintContract, PrintDefault, PrintFacts, PrintField, PrintKind, PrintValue,
    PRINT_CATALOGUE,
};

const KEY: &str = "base.templates.work_sheet";
const EN: &str = r#"{"blocks":[
    {"type":"heading","text":"Work sheet"},
    {"type":"field","field":"reference","label":"Reference"},
    {"type":"if_present","field":"address","blocks":[
        {"type":"field","field":"address","label":"Address"}
    ]},
    {"type":"table","rows":"lines","columns":[
        {"field":"description","heading":"Description"},
        {"field":"quantity","heading":"Quantity"}
    ]}
]}"#;
const MS: &str = r#"{"blocks":[
    {"type":"heading","text":"Lembaran kerja"},
    {"type":"field","field":"reference","label":"Rujukan"},
    {"type":"table","rows":"lines","columns":[
        {"field":"description","heading":"Perihal"},
        {"field":"quantity","heading":"Kuantiti"}
    ]}
]}"#;

const COLUMNS: &[PrintField] = &[
    PrintField {
        id: "description",
        label: "Description",
        kind: PrintKind::Text,
        optional: false,
        sample: PrintValue::Text("Sample <material>"),
    },
    PrintField {
        id: "quantity",
        label: "Quantity",
        kind: PrintKind::Integer,
        optional: false,
        sample: PrintValue::Integer(2),
    },
];
const SAMPLE_ROW: PrintFacts<'static> = &[
    ("description", COLUMNS[0].sample),
    ("quantity", COLUMNS[1].sample),
];

const STATES: &[&str] = &["issued"];
const DEFAULTS: &[PrintDefault] = &[
    PrintDefault {
        language: "en",
        version: "1",
        layout: EN,
    },
    PrintDefault {
        language: "ms",
        version: "2",
        layout: MS,
    },
];
static CONTRACT: PrintContract = PrintContract {
    key: KEY,
    label: "Work sheet",
    audience: Audience::Staff,
    version: "1",
    states: STATES,
    fields: &[
        PrintField {
            id: "reference",
            label: "Reference",
            kind: PrintKind::Text,
            optional: false,
            sample: PrintValue::Text("WS-1"),
        },
        PrintField {
            id: "address",
            label: "Address",
            kind: PrintKind::Address,
            optional: true,
            sample: PrintValue::Address(&["Sample building", "Sample city"]),
        },
        PrintField {
            id: "lines",
            label: "Lines",
            kind: PrintKind::Rows(COLUMNS),
            optional: false,
            sample: PrintValue::Rows(&[SAMPLE_ROW, SAMPLE_ROW]),
        },
    ],
    base_language: "en",
    defaults: DEFAULTS,
};

inventory::submit! { Contribution::new(
    PRINT_CATALOGUE,
    KEY,
    Provenance { module: "base/templates", source: file!(), line: line!() },
    &CONTRACT,
) }

// An independent catalogue lets us prove boot rejection without installing invalid declarations.
const INVALID_MODULE: &str = "base/invalid_print_proof";
const INVALID_KEY: &str = "base.invalid_print_proof.work_sheet";
static INVALID: PrintContract = PrintContract {
    key: INVALID_KEY,
    defaults: &[PrintDefault {
        language: "en",
        version: "1",
        layout: r#"{"blocks":[{"type":"field","field":"undeclared","label":"Unknown"}]}"#,
    }],
    ..CONTRACT
};
static PROOF_MODULE: Module = Module {
    id: INVALID_MODULE,
    layer: Layer::Base,
    dependencies: &["base/templates"],
    migrations: &[],
    services: &[],
};

static INVALID_ENTRY: Contribution = Contribution::new(
    PRINT_CATALOGUE,
    INVALID_KEY,
    Provenance {
        module: INVALID_MODULE,
        source: file!(),
        line: line!(),
    },
    &INVALID,
);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let modules = bilibi_module_registry::discover()?;
    let mut with_invalid = modules.clone();
    with_invalid.push(&PROOF_MODULE);
    let consumers = inventory::iter::<Consumer>.into_iter().collect();
    let mut contributions: Vec<_> = inventory::iter::<Contribution>.into_iter().collect();
    contributions.push(&INVALID_ENTRY);
    assert!(Catalog::build(&with_invalid, consumers, contributions)
        .err()
        .is_some_and(|error| error.to_string().contains("undeclared")));
    let catalogue = Catalog::discover(&modules)?;
    let contract = catalogue
        .entries::<PrintContract>(PRINT_CATALOGUE)?
        .iter()
        .find(|entry| entry.id == KEY)
        .expect("the sample contract was registered")
        .get::<PrintContract>()?;

    let malay = contract.packaged("ms-MY", Some("en"))?;
    assert_eq!(malay.language, "ms");
    assert_eq!(
        malay.origin,
        Origin::Packaged {
            version: "2".to_owned()
        }
    );
    assert_eq!(contract.packaged("de-DE", Some("ms"))?.language, "ms");
    assert_eq!(contract.packaged("de-DE", None)?.language, "en");

    let layout = contract.compile(EN)?;
    let preview = layout.preview()?;
    assert!(preview.contains("Sample building<br>Sample city"));
    assert!(preview.contains("Sample &lt;material&gt;"));
    let rows = vec![SAMPLE_ROW; 230];
    let values = [
        ("reference", PrintValue::Text("<script>")),
        ("lines", PrintValue::Rows(&rows)),
    ];
    let document = layout.render("issued", &values)?;
    assert_eq!(document.matches("<tr>").count(), rows.len() + 1);
    assert!(document.contains("&lt;script&gt;"));
    assert!(!document.contains("<script>"));
    assert!(!document.contains("Sample building"));
    let oversized = vec![SAMPLE_ROW; 2_001];
    assert!(layout
        .render(
            "issued",
            &[values[0], ("lines", PrintValue::Rows(&oversized))]
        )
        .is_err());
    assert!(layout.render("draft", &values).is_err());
    assert!(layout.render("issued", &values[1..]).is_err());
    let invalid_rows: &[PrintFacts<'_>] = &[&[("quantity", PrintValue::Integer(1))]];
    assert_eq!(
        layout
            .render(
                "issued",
                &[values[0], ("lines", PrintValue::Rows(invalid_rows))]
            )
            .unwrap_err()
            .field,
        "lines[1].description"
    );
    assert!(contract
        .compile(&EN.replace("description", "unknown"))
        .is_err());
    assert!(contract
        .compile(&EN.replace("address", "reference"))
        .is_err());
    assert!(contract
        .compile(&EN.replace("\"Work sheet\"", "\"Work sheet\",\"html\":\"<script>\""))
        .is_err());
    println!("Print registry, locale resolution, conditional facts, escaped output and repeated rows passed.");
    Ok(())
}
