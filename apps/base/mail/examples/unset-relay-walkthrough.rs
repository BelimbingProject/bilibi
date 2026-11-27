//! Walkthrough of issue 549: without `MAIL_HOST`, the registered mailer keeps
//! mail in files only on a development installation. Run against an
//! explicitly supplied disposable, migrated DATABASE_URL (its stored instance
//! role is then Production), with no `MAIL_HOST` and a private
//! `BILIBI_MAIL_DIR`.
//!
//! It sends one message through `base/mail.mailer` under each stored role and
//! counts the files the dev mailbox holds after each. Each line says what was
//! sent and what answered; a line starting `FAIL` makes the run exit non-zero.
//!
//! Environment: BILIBI_DISPOSABLE_MAIL_DEMO=yes, DATABASE_URL, BILIBI_MAIL_DIR.
use bilibi_database::{pool, Error, Result};
use bilibi_mail::{Address, Mailbox, Mailer, Message};
use bilibi_module_registry::Services;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_MAIL_DEMO").as_deref() != Ok("yes") {
        eprintln!("Set BILIBI_DISPOSABLE_MAIL_DEMO=yes: this writes to the database it is given.");
        return Err(Error::Invalid);
    }
    if std::env::var("MAIL_HOST").is_ok_and(|host| !host.trim().is_empty()) {
        eprintln!("Unset MAIL_HOST: this walkthrough proves the dev mailbox.");
        return Err(Error::Invalid);
    }
    let dir = std::env::var("BILIBI_MAIL_DIR").map_err(|_| {
        eprintln!("Set BILIBI_MAIL_DIR to a private folder.");
        Error::Invalid
    })?;
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_database::migrate(&db, &modules).await?;
    let mut services = Services::default();
    services.provide("bootstrap.database", Arc::new(db.clone()))?;
    services.provide("bootstrap.composition", Arc::new(bilibi_web::composition()))?;
    services.install(&modules)?;
    let mailer = services.get::<Mailer>(bilibi_mail::SERVICE)?;
    let mailbox = Mailbox::new(dir.into(), 100);
    let files = || mailbox.entries().map_or(0, |entries| entries.len());
    let message = Message::text(
        Address::new(Some("Customer"), "customer@example.test").map_err(|_| Error::Invalid)?,
        "Reset your password",
        "Open this link to choose a new password: https://example.test/reset/walkthrough\n",
    );
    let mut run = Run { failures: 0 };
    println!("transport: {}", mailer.describe());

    // None: the role a migrated database has before anyone stores one.
    for (role, sends) in [
        (None, false),
        (Some("staging"), false),
        (Some("development"), true),
        (Some("production"), false),
    ] {
        if let Some(role) = role {
            bilibi_walkthrough_fixture::set_instance_role(&db, role).await?;
        }
        let label = role.unwrap_or("production (not stored)");
        let before = files();
        let result = mailer.deliver(&message).await;
        let stored = files() - before;
        let ready = mailer.ready().await;
        let ok = if sends {
            result.is_ok() && ready.is_ok() && stored == 1
        } else {
            refused(&result) && refused(&ready) && stored == 0
        };
        run.expect(
            &if sends {
                format!("{label}: the message is kept in a file")
            } else {
                format!("{label}: delivery fails naming MAIL_HOST, no file is written")
            },
            ok,
            format!(
                "{}, {stored} new file(s)",
                result.map_or_else(|error| error.to_string(), |_| "sent".into())
            ),
        );
    }

    let half =
        Mailer::from_lookup(|name| (name == "MAIL_HOST").then(|| "smtp.example.test".into()));
    run.expect(
        "a half-set relay still refuses to start",
        matches!(half, Err(bilibi_mail::Error::Config(_))),
        half.map_or_else(|error| error.to_string(), |mailer| mailer.describe()),
    );

    if run.failures > 0 {
        eprintln!("{} check(s) failed", run.failures);
        return Err(Error::Internal);
    }
    println!("all checks passed");
    Ok(())
}

/// The refusal of a dev mailbox off development: a configuration error
/// that names the variable to set.
fn refused<T>(result: &bilibi_mail::Result<T>) -> bool {
    matches!(result, Err(bilibi_mail::Error::Config(why)) if why.contains("MAIL_HOST"))
}
struct Run {
    failures: u32,
}
impl Run {
    fn expect(&mut self, label: &str, ok: bool, saw: impl std::fmt::Display) {
        if ok {
            println!("ok    {label}: {saw}");
        } else {
            self.failures += 1;
            println!("FAIL  {label}: {saw}");
        }
    }
}
