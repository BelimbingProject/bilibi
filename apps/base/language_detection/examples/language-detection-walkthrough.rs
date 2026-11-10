//! Executable real-model walkthrough, without a test harness or assertions.
//! Use only with the disposable walkthrough database; prints outcomes for review.
use bilibi_database::{pool, MutationBoundary};
use bilibi_language_detection::{LanguageDetector, MODEL_VERSION, SERVICE};
use bilibi_module_registry::Services;
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::Authentication;
use serde_json::json;
use std::{sync::Arc, time::Instant};

const ENGLISH: &str = "Please send me the updated delivery schedule for the order we placed yesterday. We need the materials before Friday morning.";
const TAMIL: &str = "வணக்கம், நேற்று நாங்கள் கொடுத்த ஆர்டரின் விநியோக விவரங்களை அனுப்புங்கள். பொருட்கள் எப்போது வந்து சேரும் என்று தெரிந்து கொள்ள விரும்புகிறோம்.";
const SINHALA: &str =
    "ආයුබෝවන්, අපි ඊයේ ඇණවුම් කළ භාණ්ඩ ලබා දෙන දිනය දැන ගැනීමට කැමතියි. කරුණාකර අලුත් විස්තර අප වෙත එවන්න.";
const MALAY: &str = "Bahasa Melayu merupakan bahasa rasmi dan bahasa kebangsaan di Malaysia. Bahasa ini digunakan secara meluas di seluruh negara termasuk dalam urusan kerajaan, pendidikan dan kehidupan seharian masyarakat.";
const MALAY_CONVERSATION: &str = "Selamat pagi, saya ingin bertanya tentang pesanan saya yang masih belum sampai. Boleh pihak tuan menyemak status penghantaran barang tersebut? Terima kasih atas bantuan pihak tuan.";
const MALAY_SHARED: &str = "Sila hantarkan jadual penghantaran terkini untuk pesanan yang kami buat semalam. Kami memerlukan bahan tersebut sebelum pagi Jumaat.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("BILIBI_DISPOSABLE_LANGUAGE_WALKTHROUGH").as_deref() != Ok("yes") {
        return Err(
            "Requires BILIBI_DISPOSABLE_LANGUAGE_WALKTHROUGH=yes and a disposable DATABASE_URL"
                .into(),
        );
    }
    let path = std::env::args()
        .nth(1)
        .ok_or("Pass an absolute model file path (or a missing/corrupt specimen path)")?;
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let mut services = Services::default();
    services.provide("bootstrap.database", Arc::new(db.clone()))?;
    services.provide("bootstrap.composition", Arc::new(bilibi_web::composition()))?;
    services.install(&modules)?;
    let settings = services.get::<Settings>("base/settings.store")?;
    let boundary = services.get::<MutationBoundary>("base/database.mutations")?;
    let authentication = services.get::<Authentication>("base/tenancy.authentication")?;
    let detector = services.get::<LanguageDetector>(SERVICE)?;
    let mut client = db.get().await?;
    let scope = authentication
        .authenticate(&**client, 1, 1, "language walkthrough".into())
        .await?;
    let mut tx = boundary
        .begin(
            &mut client,
            scope.mutation_context("language_detection.walkthrough.setup"),
        )
        .await?;
    for (key, value) in [
        ("language_detection.model_path", json!(path)),
        ("language_detection.min_score", json!(0.8)),
        ("language_detection.min_margin", json!(0.2)),
        ("language_detection.min_letters", json!(20)),
        ("language_detection.max_bytes", json!(8192)),
        ("language_detection.max_concurrent", json!(2)),
    ] {
        settings
            .put(&mut tx, &scope, Target::Global, key, value)
            .await?;
    }
    tx.commit().await?;
    println!("Model: {MODEL_VERSION}; service: {SERVICE}");
    let reader: &dyn bilibi_tenancy::ReadClient = &**client;
    let concurrent = tokio::join!(
        detector.detect(reader, ENGLISH),
        detector.detect(reader, TAMIL),
        detector.detect(reader, SINHALA),
        detector.detect(reader, MALAY),
    );
    println!("cold concurrent calls (budget 2; excess => Busy): {concurrent:?}");
    if std::env::args().nth(2).as_deref() == Some("pause") {
        println!("Cache proof: move the supplied model file aside, then press Enter. Restore it after this process exits.");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
    }
    let mixed = format!("{TAMIL}\n{ENGLISH}");
    let large = "a".repeat(8193);
    for (label, text) in [
        ("empty => Unknown(Empty)", ""),
        ("whitespace => Unknown(Empty)", " \t\n"),
        (
            "unsupported symbols => Unknown(NoLetters)",
            "🙂 12345 !!! 🛒",
        ),
        ("short => Unknown(TooShort)", "hello"),
        ("oversize => Unknown(TooLong)", large.as_str()),
        ("control => Unknown(ControlCharacters)", "hello\0message"),
        (
            "model token => Unknown(ReservedToken)",
            "hello </s> message",
        ),
        ("Tamil => ta", TAMIL),
        ("Sinhala => si", SINHALA),
        ("English => en", ENGLISH),
        ("Malay => ms", MALAY),
        (
            "Malay conversation => inspect threshold refusal",
            MALAY_CONVERSATION,
        ),
        (
            "Malay with shared vocabulary => inspect threshold refusal",
            MALAY_SHARED,
        ),
        (
            "ambiguous shared words => unknown",
            "bank international hotel restaurant central station taxi internet",
        ),
        ("mixed Tamil/English => unknown", mixed.as_str()),
        (
            "Romanized Tamil => inspect only; no reliability claim",
            "vanakkam neenga eppadi irukkeenga enakku udhavi venum",
        ),
    ] {
        let started = Instant::now();
        println!(
            "{label}: {:?} ({} us)",
            detector.detect(&**client, text).await,
            started.elapsed().as_micros()
        );
    }
    // Renaming the provisioned file between these calls externally is another
    // way to observe caching. No model path is reopened after the first load.
    for index in 1..=5 {
        let started = Instant::now();
        println!(
            "repeat {index}: {:?} ({} us)",
            detector.detect(&**client, ENGLISH).await,
            started.elapsed().as_micros()
        );
    }
    for (key, value) in [
        ("language_detection.min_score", json!(1.0)),
        ("language_detection.min_score", json!(0.8)),
        ("language_detection.min_margin", json!(1.0)),
        ("language_detection.min_margin", json!(0.2)),
    ] {
        let mut tx = boundary
            .begin(
                &mut client,
                scope.mutation_context("language_detection.walkthrough.policy"),
            )
            .await?;
        settings
            .put(&mut tx, &scope, Target::Global, key, value.clone())
            .await?;
        tx.commit().await?;
        println!(
            "live {key}={value}: {:?}",
            detector.detect(&**client, ENGLISH).await
        );
    }
    for (key, value) in [
        ("language_detection.min_score", json!(1.1)),
        ("language_detection.min_margin", json!(-0.1)),
        ("language_detection.min_letters", json!(0)),
        ("language_detection.max_bytes", json!(65537)),
        ("language_detection.max_concurrent", json!(0)),
        ("language_detection.model_path", json!("relative/model.bin")),
    ] {
        let mut tx = boundary
            .begin(
                &mut client,
                scope.mutation_context("language_detection.walkthrough.invalid"),
            )
            .await?;
        println!(
            "invalid {key} => {:?}",
            settings
                .put(&mut tx, &scope, Target::Global, key, value)
                .await
        );
        tx.rollback().await?;
    }
    let mut tx = boundary
        .begin(
            &mut client,
            scope.mutation_context("language_detection.walkthrough.bounds"),
        )
        .await?;
    settings
        .put(
            &mut tx,
            &scope,
            Target::Global,
            "language_detection.min_letters",
            json!(1000),
        )
        .await?;
    println!(
        "incompatible input bounds => {:?}",
        settings
            .put(
                &mut tx,
                &scope,
                Target::Global,
                "language_detection.max_bytes",
                json!(128)
            )
            .await
    );
    tx.rollback().await?;
    Ok(())
}
