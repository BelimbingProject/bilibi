//! The new-device sign-in email. Base Session judges the device when it records
//! the sign-in (`apps/base/session/README.md`, "New-device sign-in email"); this
//! owner holds the account's address and sends the email.
//!
//! It runs in the background after the sign-in committed, so it never delays
//! or fails a sign-in: every failure is logged and dropped. The email carries
//! no secret and no sign-out link that acts by itself; its link opens Your
//! sessions, which needs a normal sign-in.
use crate::UserService;
use bilibi_database::{Error, Result};
use bilibi_datetime::{Format, Precision};
use bilibi_mail::{Address, Message};
use bilibi_session::NewDevice;
use std::sync::Arc;

const ACTION: &str = "user.new_device_sign_in_email";

impl UserService {
    /// Emails the account holder of the session `token` about a sign-in from
    /// `device`, at most once a day per device. Returns at once.
    pub fn alert_new_device(self: &Arc<Self>, token: &str, device: NewDevice) {
        let (users, token) = (self.clone(), token.to_owned());
        tokio::spawn(async move {
            if let Err(error) = users.send_new_device_alert(&token, &device).await {
                eprintln!("mail: new-device sign-in email was not sent: {error}");
            }
        });
    }

    async fn send_new_device_alert(&self, token: &str, device: &NewDevice) -> Result<()> {
        let mut client = self.db.get().await?;
        // The session just admitted proves the account; an ended one sends nothing.
        let scope = self.sessions.check(&**client, token).await?;
        let Some(row) = client
            .query_opt(
                "SELECT u.name,i.email FROM users u JOIN user_identities i ON i.id=u.identity_id AND i.deleted_at IS NULL AND i.status='active' WHERE u.tenant_id=$1 AND u.id=$2 AND u.kind='staff' AND u.deleted_at IS NULL AND u.status='active'",
                &[&scope.tenant(), &scope.user()],
            )
            .await?
        else {
            return Ok(());
        };
        let (name, address): (String, String) = (row.get(0), row.get(1));
        let locale = self.locales.resolve(&**client, &scope, None).await?.locale;
        let display = self
            .display
            .display(&**client, &scope, None, locale)
            .await?;
        let at = jiff::Timestamp::try_from(device.signed_in_at).map_err(|_| Error::Internal)?;
        let at = display.render(at, Format::DateTime, Precision::Minute).text;
        let ip = device.ip_address.as_deref().unwrap_or("not recorded");
        let place = device.place.as_deref().unwrap_or("not recorded");
        let message = Message::text(
            Address::new(Some(&name), &address).map_err(|_| Error::Invalid)?,
            "New sign-in to your Bilibi account",
            format!(
                "Your Bilibi account was signed in from a device it has not used before.\n\n\
                 Time: {at}\nDevice: {device}\nIP address: {ip}\nPlace: {place}\n\n\
                 If this was you, you can ignore this email.\n\n\
                 If this wasn't you, change your password now. Then sign that session out in Your sessions:\n\n\
                 {origin}/settings/sessions\n",
                device = device.device,
                place = place,
                origin = self.origin,
            ),
        );
        let mut tx = self
            .mutations
            .begin(&mut client, scope.mutation_context(ACTION))
            .await?;
        let claimed = match self
            .sessions
            .claim_new_device_alert(&mut tx, &scope, device)
            .await
        {
            Ok(claimed) => claimed,
            // A concurrent sign-in from the same device claimed today's email.
            Err(Error::Conflict) => return Ok(()),
            Err(error) => return Err(error),
        };
        if !claimed {
            return Ok(());
        }
        tx.commit().await?;
        drop(client);
        if let Err(error) = self.mailer.deliver(&message).await {
            eprintln!(
                "mail: new-device sign-in email for user {} was not delivered: {error}",
                scope.user()
            );
        }
        Ok(())
    }
}
