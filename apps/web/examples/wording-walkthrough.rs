//! Language selection and branded mail on a migrated, disposable database.
use bilibi_agent_api::plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans};
use bilibi_base::{
    branding::LOGO_MAX_BYTES,
    media::{Media, Upload},
    party_ids::OrganisationId,
    settings::{Settings, Target},
    templates::{Templates, Value, LANGUAGE},
};
use bilibi_database::pool;
use bilibi_mail::{Address, Attachment, InlineImage, Mailer, Message, MAX_MESSAGE_BYTES};
use bilibi_tenancy::{ProofFuture, Scope};
use bilibi_web::App;
use serde_json::json;
use std::{env, fs, path::Path, sync::Arc};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const NOTICE: &str = "core.portal.welcome_notice";
const MAIL: &str = "base.reports.scheduled_report";
// A tiny, valid PNG to check the bytes returned from the MIME parser.
const LOGO: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

struct Walkthrough {
    app: App,
    scope: Scope,
    token: String,
    plans: Arc<Plans>,
}
impl PlanAuthority for Walkthrough {
    fn verify<'a>(
        &'a self,
        tx: &'a tokio_postgres::Transaction<'_>,
    ) -> ProofFuture<'a, PlanIdentity> {
        Box::pin(async move {
            Ok(PlanIdentity {
                scope: self.app.security.sessions.hold(tx, &self.token).await?,
                credential_binding: format!("browser:{}", self.token),
            })
        })
    }
}
impl Walkthrough {
    async fn write(&self, key: &str, input: serde_json::Value) -> Result<serde_json::Value> {
        let ops = [PlannedOperation {
            key: key.into(),
            input,
        }];
        assert_eq!(self.plans.preview(self, &ops).await?["valid"], true);
        Ok(self.plans.commit(&self.scope, self, &ops).await?)
    }

    async fn language(&self, company: OrganisationId, language: &str) -> Result<()> {
        let mut client = self.app.db.get().await?;
        let settings: Arc<Settings> = self.app.services.get("base/settings.store")?;
        let mut tx = self
            .app
            .mutations
            .begin(
                &mut client,
                self.scope.mutation_context("wording.walkthrough"),
            )
            .await?;
        settings
            .put(
                &mut tx,
                &self.scope,
                Target::Company(company),
                LANGUAGE,
                json!(language),
            )
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn notice(&self, company: Option<OrganisationId>) -> Result<String> {
        let templates: Arc<Templates> = self.app.services.get(bilibi_base::templates::SERVICE)?;
        let client = self.app.db.get().await?;
        let values = [
            ("site_title", Value::Text("Example Portal")),
            ("email", Value::Text("customer@example.test")),
        ];
        let wording = templates
            .wording(
                &**client,
                &self.scope,
                NOTICE,
                company,
                "en-MY",
                (&values, &[]),
            )
            .await?;
        Ok(wording["body"].clone())
    }

    async fn set_brand(&self, output: &Path) -> Result<()> {
        let mut client = self.app.db.get().await?;
        let settings: Arc<Settings> = self.app.services.get("base/settings.store")?;
        let mut tx = self
            .app
            .mutations
            .begin(
                &mut client,
                self.scope.mutation_context("wording.walkthrough"),
            )
            .await?;
        settings
            .put(
                &mut tx,
                &self.scope,
                Target::Global,
                "media.disks",
                json!({"local": output}),
            )
            .await?;
        settings
            .put(
                &mut tx,
                &self.scope,
                Target::Global,
                MAX_MESSAGE_BYTES,
                json!(8192),
            )
            .await?;
        tx.commit().await?;
        let media: Arc<Media> = self.app.services.get(bilibi_base::media::SERVICE)?;
        let logo = media
            .put_public_raster_image(
                &mut client,
                &self.scope,
                "branding",
                Upload {
                    filename: "logo.png".into(),
                    bytes: LOGO.to_vec(),
                },
                LOGO_MAX_BYTES,
            )
            .await?;
        drop(client);
        self.write(
            "base.branding.set",
            json!({
                "name":"Example & Trading", "accent":"violet", "logo_asset_id":logo.id
            }),
        )
        .await?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if env::var("BILIBI_WORDING_DEMO").as_deref() != Ok("disposable") {
        return Err("Use BILIBI_WORDING_DEMO=disposable with a fresh migrated database".into());
    }
    let output = std::path::PathBuf::from(env::var("BILIBI_WORDING_PREVIEW")?);
    let db = pool(&env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    let app = App::new(db, &env::var("APP_ORIGIN")?)?;
    let login = app
        .user
        .login("ai@agent.my", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let scope = app.security.guard(&login.token).await?;
    let walk = Walkthrough {
        plans: app.services.get("base/agent_api.plans")?,
        app,
        scope,
        token: login.token,
    };
    let company = OrganisationId::new(1)?;
    let other = OrganisationId::new(2)?;
    let english = "You are signed in to Example Portal as customer@example.test. Use the menu above to open what Example Portal shares with you.";
    assert_eq!(walk.notice(Some(company)).await?, english);
    walk.write(
        "base.settings_editor.tenant.set",
        json!({"key":LANGUAGE, "value":"ms-MY"}),
    )
    .await?;
    assert!(walk
        .notice(None)
        .await?
        .starts_with("Anda telah log masuk ke Example Portal sebagai customer@example.test."));

    let publish = |company, body| {
        json!({
            "key":NOTICE, "company_id":company, "language":"ms-MY", "revision":0,
            "slots":{"body":body}
        })
    };
    walk.write(
        "base.templates.template.publish",
        publish(None, "Selamat datang ke {{site_title}}."),
    )
    .await?;
    walk.write(
        "base.templates.template.publish",
        publish(Some(company.get()), "Portal syarikat {{site_title}}."),
    )
    .await?;
    assert_eq!(
        walk.notice(Some(company)).await?,
        "Portal syarikat Example Portal."
    );
    assert_eq!(
        walk.notice(Some(other)).await?,
        "Selamat datang ke Example Portal."
    );
    walk.language(company, "en-GB").await?;
    assert_eq!(walk.notice(Some(company)).await?, english);
    walk.language(company, "fr-CA").await?;
    assert_eq!(
        walk.notice(Some(company)).await?,
        "Portal syarikat Example Portal."
    );
    let templates: Arc<Templates> = walk.app.services.get(bilibi_base::templates::SERVICE)?;
    let client = walk.app.db.get().await?;
    let explicit = templates
        .resolve(&**client, &walk.scope, NOTICE, None, "en", None)
        .await?;
    assert_eq!(explicit.language, "en");
    drop(client);
    println!(
        "PASS exact and base tags, company and tenant revisions, packaged and tenant fallback"
    );

    walk.set_brand(&output).await?;
    let client = walk.app.db.get().await?;
    let fact = "<b>Sales & stock</b>";
    let wording = templates
        .wording(
            &**client,
            &walk.scope,
            MAIL,
            Some(other),
            "en",
            (&[("report_name", Value::Text(fact))], &[]),
        )
        .await?;
    drop(client);
    assert!(wording["subject"].starts_with("Laporan berjadual:"));
    let link = "https://example.test/reports/weekly?format=csv&download=1";
    let message = Message::text(
        Address::new(None, "report@example.test")?,
        &wording["subject"],
        format!("{}\n\n{link}\n", wording["intro"]),
    );
    let mailer: Arc<Mailer> = walk.app.services.get(bilibi_mail::SERVICE)?;
    assert!(mailer.dev_mailbox().is_some());
    let sent = mailer
        .deliver_linked_for(walk.scope.tenant(), &message, Some(link))
        .await?;
    let parsed = bilibi_mail::parse(&sent.bytes, &mailer.parse_limits().await?)?;
    assert_eq!(
        parsed.text.as_deref().map(str::trim_end),
        Some(message.text.trim_end())
    );
    assert_eq!(parsed.subject.as_deref(), Some(message.subject.as_str()));
    let html = parsed.html.ok_or("missing HTML alternative")?;
    assert!(html.contains("Powered by Bilibi") && html.contains("#8b5cf6"));
    assert!(html.contains("Example &#38; Trading"));
    assert!(!html.contains(fact) && html.contains("&#60;b&#62;Sales &#38; stock&#60;/b&#62;"));
    assert_eq!(html.matches("href=").count(), 1);
    assert!(html.contains("format=csv&#38;download=1"));
    let logo = parsed.attachments.first().ok_or("missing inline logo")?;
    assert_eq!(logo.disposition, bilibi_mail::Disposition::Inline);
    assert_eq!(logo.bytes, LOGO);
    let preview = bilibi_mail::inline_images(
        &html,
        [InlineImage {
            content_id: logo.content_id.as_deref().ok_or("missing content ID")?,
            content_type: &logo.declared_type,
            bytes: &logo.bytes,
        }],
        mailer.limits().await?.max_message_bytes,
    )?;
    assert!(preview.contains("src=\"data:image/png;"));
    fs::write(output.join("mail-preview.html"), preview)?;
    println!("PASS localized mail with escaped fact, trusted link, plain alternative, accent and embedded logo");
    let mut large = Message::text(
        message.to.clone(),
        "Meeting invitation",
        "Meeting description. ".repeat(250),
    );
    let calendar = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n";
    large.attachments.push(Attachment {
        filename: "meeting.ics".into(),
        content_type: "text/calendar".into(),
        bytes: calendar.to_vec(),
    });
    let sent = mailer.deliver_for(walk.scope.tenant(), &large).await?;
    let parsed = bilibi_mail::parse(&sent.bytes, &mailer.parse_limits().await?)?;
    assert!(parsed.html.is_none());
    assert_eq!(
        parsed.text.as_deref().map(str::trim_end),
        Some(large.text.trim_end())
    );
    assert_eq!(parsed.attachments.len(), 1);
    assert_eq!(parsed.attachments[0].bytes, calendar);
    println!("PASS plain mail with its attachment when branded HTML exceeds the wire limit");
    Ok(())
}
