//! Prints a synthetic bilingual work sheet through the private Artifacts path.
//! Run on a fresh migrated database with the Artifacts walkthrough environment.
use bilibi_artifacts::{Artifacts, Operation, Owner, PdfOptions, PdfPaper, Reference};
use bilibi_branding::Branding;
use bilibi_database::{pool, Error, MutationBoundary};
use bilibi_party_ids::OrganisationId;
use bilibi_settings::{Settings, Target};
use bilibi_templates::{
    PrintContract, PrintDefault, PrintFacts, PrintField, PrintKind, PrintValue,
};
use bilibi_tenancy::{Authentication, ProofClient, ProofFuture, Scope};
use serde_json::json;
use std::{os::unix::fs::PermissionsExt, path::PathBuf};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const REFERENCE: &str = "WS-2026-0001";
const EN: &str = r#"{"blocks":[
    {"type":"heading","text":"Production work sheet"},
    {"type":"field","field":"issuer"},
    {"type":"field","field":"reference"},
    {"type":"field","field":"address","label":"Workplace"},
    {"type":"field","field":"validation"},
    {"type":"table","rows":"operations","columns":[{"field":"instructions"},{"field":"sequence"}]}
]}"#;
const MS: &str = r#"{"paper":"letter","blocks":[
    {"type":"heading","text":"Lembaran kerja pengeluaran"},
    {"type":"field","field":"issuer"},
    {"type":"field","field":"reference"},
    {"type":"field","field":"address","label":"Tempat kerja"},
    {"type":"field","field":"validation"},
    {"type":"table","rows":"operations","columns":[{"field":"instructions"},{"field":"sequence"}]}
]}"#;

const COLUMNS: &[PrintField] = &[
    PrintField {
        id: "instructions",
        kind: PrintKind::Text,
        optional: false,
        required_output: true,
        labels: &[("en", "Instructions"), ("ms", "Arahan")],
        sample: PrintValue::Text("Check tooling"),
    },
    PrintField {
        id: "sequence",
        kind: PrintKind::Integer,
        optional: false,
        required_output: true,
        labels: &[("en", "Sequence"), ("ms", "Turutan")],
        sample: PrintValue::Integer(1),
    },
];
const SAMPLE_ROW: PrintFacts<'static> = &[
    ("instructions", COLUMNS[0].sample),
    ("sequence", COLUMNS[1].sample),
];
const ADDRESS: &[&str] = &[
    "Unit 712, Example Industrial Centre, Persiaran Perusahaan",
    "Example Town, Selangor",
    "Malaysia",
];
const VALIDATION: &str = "https://example.test/document/WS-2026-0240/validate";
static CONTRACT: PrintContract = PrintContract {
    key: "base.artifacts.work_sheet",
    label: "Production work sheet",
    version: "1",
    states: &["issued"],
    fields: &[
        PrintField {
            id: "issuer",
            kind: PrintKind::Text,
            optional: false,
            required_output: true,
            labels: &[("en", "Issuer"), ("ms", "Pengeluar")],
            sample: PrintValue::Text("Example Works Sdn. Bhd."),
        },
        PrintField {
            id: "reference",
            kind: PrintKind::Text,
            optional: false,
            required_output: true,
            labels: &[("en", "Reference"), ("ms", "Rujukan")],
            sample: PrintValue::Text(REFERENCE),
        },
        PrintField {
            id: "address",
            kind: PrintKind::Address,
            optional: true,
            required_output: false,
            labels: &[],
            sample: PrintValue::Address(ADDRESS),
        },
        PrintField {
            id: "validation",
            kind: PrintKind::QrCode,
            optional: true,
            required_output: true,
            labels: &[("en", "Validation"), ("ms", "Pengesahan")],
            sample: PrintValue::QrCode(VALIDATION),
        },
        PrintField {
            id: "operations",
            kind: PrintKind::Rows(COLUMNS),
            optional: false,
            required_output: true,
            labels: &[],
            sample: PrintValue::Rows(&[SAMPLE_ROW]),
        },
    ],
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

struct WorkSheet;

impl Owner for WorkSheet {
    fn owner_id(&self) -> &'static str {
        "walkthrough.print"
    }

    fn authorize<'a>(
        &'a self,
        _: &'a dyn ProofClient,
        scope: &'a Scope,
        company: OrganisationId,
        _: Operation,
        reference: Option<&'a Reference>,
    ) -> ProofFuture<'a, ()> {
        Box::pin(async move {
            if scope.tenant() == 1
                && scope.user() == 1
                && company.get() == 1
                && reference.is_some_and(|reference| reference.subject == REFERENCE)
            {
                return Ok(());
            }
            Err(Error::Forbidden)
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("BILIBI_DISPOSABLE_ARTIFACTS_DEMO").as_deref() != Ok("yes") {
        return Err("explicit disposable DB opt-in required".into());
    }
    let root = std::env::var("BILIBI_ARTIFACTS_DEMO_ROOT")?;
    let out = PathBuf::from(std::env::var("BILIBI_PDF_DEMO_OUT")?);
    std::fs::create_dir_all(&root)?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    std::fs::create_dir_all(&out)?;

    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "https://business-print.localhost")?;
    let mut client = db.get().await?;
    let authentication = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let actor = authentication
        .authenticate(&**client, 1, 1, "walkthrough".into())
        .await?;
    let company = OrganisationId::new(1)?;
    let settings = app.services.get::<Settings>("base/settings.store")?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let mut tx = boundary
        .begin(
            &mut client,
            actor.mutation_context("artifacts.walkthrough.configure"),
        )
        .await?;
    for (key, value) in [
        ("artifacts.storage_root", json!(root)),
        (
            "pdf.chromium.binary",
            json!(std::env::var("BILIBI_PDF_CHROMIUM")?),
        ),
        ("pdf.paper.format", json!("A3")),
    ] {
        settings
            .put(&mut tx, &actor, Target::Global, key, value)
            .await?;
    }
    tx.commit().await?;

    let instructions: Vec<_> = (1..=230)
        .map(|number| format!("Operation {number:03}. Check tooling <guard> & gauges.\nSign the work sheet before the next operation."))
        .collect();
    let rows: Vec<_> = instructions
        .iter()
        .zip(1..=230)
        .map(|(description, sequence)| {
            [
                ("instructions", PrintValue::Text(description)),
                ("sequence", PrintValue::Integer(sequence)),
            ]
        })
        .collect();
    let rows: Vec<PrintFacts<'_>> = rows.iter().map(|row| row.as_slice()).collect();
    let facts = [
        ("issuer", CONTRACT.fields[0].sample),
        ("reference", PrintValue::Text(REFERENCE)),
        ("address", PrintValue::Address(ADDRESS)),
        ("operations", PrintValue::Rows(&rows)),
        ("validation", PrintValue::QrCode(VALIDATION)),
    ];
    let branding = app.services.get::<Branding>(bilibi_branding::SERVICE)?;
    let accent = branding.accent(&**client).await?;
    let store = app.services.get::<Artifacts>("base/artifacts.store")?;
    let owner = WorkSheet;
    let documents = store.for_owner(&actor, company, &owner);
    let document = CONTRACT
        .compile("en", EN)?
        .document("issued", &facts, accent)?;
    let peer = authentication
        .authenticate(&**client, 1, 2, "walkthrough".into())
        .await?;
    let reference = Reference {
        subject: REFERENCE.to_owned(),
        kind: "en".into(),
    };
    assert!(matches!(
        store
            .for_owner(&peer, company, &owner)
            .render_inline_document(&mut client, reference, document, &PdfOptions::default())
            .await,
        Err(bilibi_artifacts::Error::Database(Error::Forbidden)),
    ));
    for (locale, paper, name) in [
        ("en-MY", PdfPaper::A4, "work-sheet-en-a4.pdf"),
        ("ms-MY", PdfPaper::Letter, "work-sheet-ms-letter.pdf"),
    ] {
        let layout = CONTRACT.packaged(locale, None)?;
        let compiled = CONTRACT.compile(&layout.language, &layout.layout)?;
        let document = compiled.document("issued", &facts, accent)?;
        let html = document.html();
        assert_eq!(document.paper(), paper);
        assert!(html.contains("data:font/woff2;base64,"));
        assert!(html.contains("data:image/svg+xml;base64,"));
        assert!(!compiled
            .render("issued", &facts[..facts.len() - 1])?
            .contains("class=\"print-qr\""));
        assert!(!html.contains("<footer>"));
        assert!(html.contains("<title>Production work sheet</title>"));
        assert!(html.contains("&lt;guard&gt; &amp; gauges"));
        assert!(!html.contains("http:") && !html.contains("https:") && !html.contains("file:"));
        let reference = Reference {
            subject: REFERENCE.to_owned(),
            kind: layout.language,
        };
        let options = PdfOptions::default()
            .template_version("1")
            .data_version("1");
        let pdf = documents
            .render_inline_document(&mut client, reference, document, &options)
            .await?;
        let read = documents.read(&mut client, &pdf.document.id).await?;
        assert!(read.bytes.starts_with(b"%PDF-"));
        std::fs::write(out.join(name), read.bytes)?;
    }
    assert!(CONTRACT.compile("ms", &MS.replace("letter", "a3")).is_err());
    assert!(CONTRACT
        .compile(
            "en",
            &EN.replace(r#"{"type":"field","field":"validation"},"#, "")
        )
        .is_err());
    assert!(CONTRACT
        .compile(
            "en",
            &EN.replace(r#"{"type":"field","field":"issuer"},"#, "")
        )
        .is_err());
    assert!(CONTRACT
        .compile(
            "en",
            &EN.replace(
                r#"{"type":"field","field":"issuer"},"#,
                r#"{"type":"field","field":"issuer"},{"type":"field","field":"issuer"},"#,
            )
        )
        .is_err());
    println!("Business print passed for English A4, Bahasa Melayu Letter and 230 owner rows");
    Ok(())
}
