//! Outgoing mail: one validated message type and two transports. Send through the
//! `base/mail.mailer` service; never open your own SMTP connection or write
//! message files, which would skip the header validation in `render`. SMTP is used
//! when `MAIL_HOST` is configured; otherwise the dev mailbox keeps every message
//! as an `.eml` file so a reset link can be read without a mail server. The
//! registered mailer sends to files only while the stored instance role is
//! development: elsewhere `deliver` refuses with `Error::Config` naming
//! `MAIL_HOST`, so no caller records as sent a message that only reached a file.
//!
//! Received mail: `parse` reads one message's bytes into its parts (bounces and
//! complaints included) and `sanitise_html` makes its HTML safe to store and show.
mod bounds;
mod delivery;
mod limits;
mod mailbox;
mod message;
mod parse;
mod report;
mod sanitise;
mod smtp;

pub use delivery::{
    reference, DeliveryReport, DeliveryReports, ObserveFuture, Observer, Outcome, OBSERVERS,
    REFERENCE_HEADER, REPORTS,
};
pub use limits::{Limits, MAX_MESSAGE_BYTES, MAX_RECIPIENTS};
pub use mailbox::{Mailbox, MailboxEntry};
pub use message::{
    render, Address, Attachment, Envelope, ListUnsubscribe, Message, Rendered, Sent,
};
pub use parse::{header_block, parse, Disposition, ParseLimits, Parsed, Part};
pub use report::{Action, DeliveryRecipient, Report};
pub use sanitise::{inline_images, sanitise_html, InlineImage};
pub use smtp::{Smtp, SmtpConfig, Tls};

use bilibi_database::Db;
use bilibi_module_registry::{Module, Service};
use bilibi_settings::Settings;

pub const SERVICE: &str = "base/mail.mailer";
/// `base/system`'s stored instance role, read by key: System Info shows this
/// mailer, so mail cannot depend on that module.
const INSTANCE_ROLE: &str = "instance.role";
use std::{future::Future, pin::Pin, sync::Arc};

pub type Result<T> = std::result::Result<T, Error>;
pub type SendFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Invalid email message")]
    Invalid,
    #[error("Mail is not configured: {0}")]
    Config(String),
    /// Transport detail for operators' logs; callers show a generic failure.
    #[error("Mail delivery failed: {0}")]
    Delivery(String),
    /// An owner could not record a delivery report.
    #[error("The delivery report could not be recorded: {0}")]
    Report(String),
}

/// A transport receives a message already validated and rendered to RFC 5322.
pub trait Transport: Send + Sync {
    fn send_raw<'a>(&'a self, envelope: &'a Envelope, bytes: &'a [u8]) -> SendFuture<'a>;
    /// A concrete development mailbox, never inferred from a log description.
    fn dev_mailbox(&self) -> Option<&Mailbox> {
        None
    }
    /// Secret-free description for the boot log.
    fn describe(&self) -> String;
}

pub struct Mailer {
    sender: Address,
    transport: Arc<dyn Transport>,
    limits: Limits,
    settings: Option<(Db, Arc<Settings>)>,
}
impl Mailer {
    pub fn new(sender: Address, transport: Arc<dyn Transport>, limits: Limits) -> Self {
        Self {
            sender,
            transport,
            limits,
            settings: None,
        }
    }
    pub fn describe(&self) -> String {
        format!("{} from {}", self.transport.describe(), self.sender.email)
    }
    pub fn dev_mailbox(&self) -> Option<&Mailbox> {
        self.transport.dev_mailbox()
    }
    /// Whether `deliver` may send now. SMTP always may. The dev mailbox may
    /// only on an installation whose stored role is development; elsewhere
    /// this is `Error::Config` naming `MAIL_HOST`. A standalone mailer (built
    /// with `new` or `from_env`, not the registered service) reads no role.
    pub async fn ready(&self) -> Result<()> {
        if self.transport.dev_mailbox().is_none() {
            return Ok(());
        }
        let Some((db, settings)) = &self.settings else {
            return Ok(());
        };
        let unavailable = || Error::Config("the instance role is unavailable".into());
        let client = db.get().await.map_err(|_| unavailable())?;
        let role = settings
            .get_global(&**client, INSTANCE_ROLE)
            .await
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(unavailable)?;
        if role == "development" {
            Ok(())
        } else {
            Err(Error::Config(format!(
                "set MAIL_HOST to send mail; a {role} installation does not keep mail in files"
            )))
        }
    }
    /// Registered mailers read current installation settings on every render/send.
    /// Standalone mailers use the snapshot passed to `new`.
    pub async fn limits(&self) -> Result<Limits> {
        let Some((db, settings)) = &self.settings else {
            return Ok(self.limits);
        };
        let client = db
            .get()
            .await
            .map_err(|_| Error::Config("mail limits unavailable".into()))?;
        let read = async |key| {
            settings
                .get_global(&**client, key)
                .await
                .ok()
                .and_then(|value| value.as_u64())
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| Error::Config("mail limits unavailable".into()))
        };
        Ok(Limits {
            max_message_bytes: read(MAX_MESSAGE_BYTES).await?,
            max_recipients: read(MAX_RECIPIENTS).await?,
        })
    }
    /// Limits for `parse` from the current `mail.max_message_bytes`.
    pub async fn parse_limits(&self) -> Result<ParseLimits> {
        Ok(self.limits().await?.into())
    }
    pub async fn render(&self, message: &Message) -> Result<Rendered> {
        render(message, &self.sender, self.limits().await?)
    }
    pub async fn deliver(&self, message: &Message) -> Result<Sent> {
        self.deliver_rendered(self.render(message).await?).await
    }
    /// Send a trusted renderer's output without rebuilding headers or MIME.
    /// Taking ownership returns the exact wire allocation in `Sent`.
    pub async fn deliver_rendered(&self, rendered: Rendered) -> Result<Sent> {
        let limits = self.limits().await?;
        if rendered.bytes.len() > limits.max_message_bytes
            || rendered.envelope.recipients.len() > limits.max_recipients
        {
            return Err(Error::Invalid);
        }
        rendered.envelope.smtp()?;
        self.ready().await?;
        self.transport
            .send_raw(&rendered.envelope, &rendered.bytes)
            .await?;
        Ok(Sent {
            message_id: rendered.message_id,
            bytes: rendered.bytes,
        })
    }
    /// SMTP when `MAIL_HOST` is set, with every other `MAIL_*` variable then
    /// required; the dev mailbox otherwise. A half-configured relay fails boot
    /// rather than silently falling back to files; an unset one boots, and
    /// [`Mailer::ready`] refuses to send unless the instance is development.
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let value = |name: &str| get(name).filter(|value| !value.trim().is_empty());
        let required =
            |name: &str| value(name).ok_or_else(|| Error::Config(format!("{name} must be set")));
        if value("MAIL_HOST").is_none() {
            let sender = Address::new(
                Some(&value("MAIL_FROM_NAME").unwrap_or_else(|| "Bilibi".into())),
                &value("MAIL_FROM_ADDRESS").unwrap_or_else(|| "noreply@bilibi.localhost".into()),
            )?;
            let dir = value("BILIBI_MAIL_DIR").unwrap_or_else(|| ".local/mailbox".into());
            return Ok(Self::new(
                sender,
                Arc::new(Mailbox::new(dir.into(), 100)),
                Limits::default(),
            ));
        }
        let sender = Address::new(
            Some(&required("MAIL_FROM_NAME")?),
            &required("MAIL_FROM_ADDRESS")?,
        )?;
        let port = required("MAIL_PORT")?
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(|| {
                Error::Config("MAIL_PORT must be an integer between 1 and 65535".into())
            })?;
        let tls = match required("MAIL_TLS_MODE")?.as_str() {
            "starttls" => Tls::StartTls,
            "implicit_tls" => Tls::Implicit,
            _ => {
                return Err(Error::Config(
                    "MAIL_TLS_MODE must be either starttls or implicit_tls".into(),
                ))
            }
        };
        let smtp = Smtp::new(SmtpConfig {
            host: required("MAIL_HOST")?,
            port,
            username: required("MAIL_USERNAME")?,
            password: required("MAIL_PASSWORD")?,
            tls,
        })?;
        Ok(Self::new(sender, Arc::new(smtp), Limits::default()))
    }
}

inventory::submit! {
    Module {
        id: "base/mail", layer: bilibi_module_registry::Layer::Base,
        dependencies: &["base/module_registry", "base/database", "base/settings"], migrations: &[],
        services: &[Service { id: REPORTS, factory: |_| Ok(Arc::new(DeliveryReports::from_inventory())) }, Service { id: SERVICE, factory: |services| {
            let mut mailer = Mailer::from_env().map_err(|error| bilibi_module_registry::RegistryError::Service(error.to_string()))?;
            mailer.settings = Some((services.get::<Db>("base/database.pool")?.as_ref().clone(), services.get("base/settings.store")?));
            eprintln!("mail: {}", mailer.describe());
            Ok(Arc::new(mailer))
        }}],
    }
}

inventory::submit! { bilibi_module_registry::Consumer::new::<Observer>(OBSERVERS, "base/mail", delivery::validate) }
