//! The wording of the mail this module sends, as shared templates
//! (`apps/base/templates/README.md`). Security wording is platform-controlled;
//! this module builds the links and fixes the order of the parts.
use crate::UserService;
use bilibi_database::Error;
use bilibi_mail::{Address, Message};
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_templates::{
    Audience, DefaultText, Output, Slot, Template, Text, Value, Variable, PLATFORM_CATALOGUE,
};

pub(crate) const PASSWORD_RESET: &str = "core.user.password_reset";
pub(crate) const SIGN_IN_LOCKED: &str = "core.user.sign_in_locked";
pub(crate) const INVITATION: &str = "core.user.invitation";
pub(crate) const EMAIL_VERIFICATION: &str = "core.user.email_verification";
pub(crate) const NEW_DEVICE_SIGN_IN: &str = "core.user.new_device_sign_in";
pub(crate) const TWO_FACTOR_RESET: &str = "core.user.two_factor_reset";

/// The text of a message whose link stands alone between two passages.
pub(crate) fn around_link(text: &Text, link: &str) -> String {
    format!("{}\n\n{link}\n\n{}\n", text["intro"], text["closing"])
}

impl UserService {
    /// The workspace invitation email around the one-time link.
    pub(crate) fn invitation_mail(
        &self,
        (name, email): (&str, &str),
        tenant_name: &str,
        days: Option<i32>,
        token: &str,
    ) -> Result<Message, Error> {
        let closing = if days.is_some() {
            "closing"
        } else {
            "reissue_closing"
        };
        let days = days.map(|days| days.to_string());
        let mut values = vec![("workspace_name", Value::Text(tenant_name))];
        if let Some(days) = &days {
            values.push(("days", Value::Text(days)));
        }
        let wording = self.templates.platform_wording(INVITATION, &values)?;
        Ok(Message::text(
            Address::new(Some(name), email).map_err(|_| Error::Invalid)?,
            wording["subject"].clone(),
            format!(
                "{}\n\n{}/accept/{token}\n\n{}\n",
                wording["intro"], self.origin, wording[closing]
            ),
        ))
    }
}

const LINK_SLOTS: &[Slot] = &[
    Slot::subject(),
    Slot::text("intro", "Text before the link"),
    Slot::text("closing", "Text after the link"),
];

macro_rules! template {
    ($key:expr, $template:expr) => {
        inventory::submit! { Contribution::new(PLATFORM_CATALOGUE, $key, Provenance {
            module: "core/user", source: file!(), line: line!(),
        }, &$template) }
    };
}

template!(
    PASSWORD_RESET,
    Template {
        key: PASSWORD_RESET,
        label: "Password reset email",
        audience: Audience::Anyone,
        outputs: &[Output::PlainText],
        slots: LINK_SLOTS,
        variables: &[],
        base_language: "en",
        defaults: &[DefaultText {
            language: "en",
            version: "1",
            slots: &[
                ("subject", "Reset your Bilibi password"),
                ("intro", "Reset your Bilibi password by opening this link:"),
                (
                    "closing",
                    "If you did not request this, you can ignore this email."
                ),
            ],
        }],
    }
);

template!(
    SIGN_IN_LOCKED,
    Template {
        key: SIGN_IN_LOCKED,
        label: "Sign-in locked email",
        audience: Audience::Anyone,
        outputs: &[Output::PlainText],
        slots: &[
            Slot::subject(),
            Slot::text("intro", "Message"),
            Slot::text("reset_with_link", "Text before the reset link"),
            Slot::text("reset_without_link", "Text when no reset link can be sent"),
        ],
        variables: &[
            Variable::text(
                "duration",
                "How long sign-in is locked",
                "5 minutes",
                &["subject", "intro"]
            ),
        ],
        base_language: "en",
        defaults: &[DefaultText {
            language: "en",
            version: "1",
            slots: &[
                ("subject", "Your Bilibi sign-in is locked"),
                ("intro", "Several wrong passwords were entered for your Bilibi account, so its sign-in is locked for about {{duration}}.\n\nIf this was you, wait and try again, or reset your password now to unlock it. If it was not you, your password is still safe; resetting it is a good idea."),
                ("reset_with_link", "Reset your password with this link:"),
                ("reset_without_link", "Reset your password with \"Forgot your password?\" on the sign-in page."),
            ],
        }],
    }
);

template!(
    INVITATION,
    Template {
        key: INVITATION,
        label: "Workspace invitation email",
        audience: Audience::Staff,
        outputs: &[Output::PlainText],
        slots: &[
            Slot::subject(),
            Slot::text("intro", "Text before the link"),
            Slot::text("closing", "Text after the initial link"),
            Slot::text("reissue_closing", "Text after a reissued link"),
        ],
        variables: &[
            Variable::text("workspace_name", "Workspace name", "Example Trading", &["subject", "intro"]),
            Variable::optional_text("days", "Days the link works", "7", &["closing"]),
        ],
        base_language: "en",
        defaults: &[DefaultText {
            language: "en",
            version: "1",
            slots: &[
                ("subject", "You're invited to {{workspace_name}}"),
                ("intro", "You are invited to join {{workspace_name}}.\n\nOpen this link to accept:"),
                ("closing", "The link works once and expires in {{days}} day(s). If you did not expect this, you can ignore this email."),
                ("reissue_closing", "The link works once. If you did not expect this, you can ignore this email."),
            ],
        }],
    }
);

template!(
    EMAIL_VERIFICATION,
    Template {
        key: EMAIL_VERIFICATION,
        label: "Email verification email",
        audience: Audience::Anyone,
        outputs: &[Output::PlainText],
        slots: LINK_SLOTS,
        variables: &[],
        base_language: "en",
        defaults: &[DefaultText {
            language: "en",
            version: "1",
            slots: &[
                ("subject", "Verify your Bilibi email address"),
                ("intro", "Verify your email address by opening this link:"),
                ("closing", "The link expires in one hour. If you did not request this, you can ignore this email."),
            ],
        }],
    }
);

const DEVICE_MAIL: &[&str] = &["subject", "intro", "details", "closing"];
template!(
    NEW_DEVICE_SIGN_IN,
    Template {
        key: NEW_DEVICE_SIGN_IN,
        label: "New-device sign-in email",
        audience: Audience::Staff,
        outputs: &[Output::PlainText],
        slots: &[
            Slot::subject(),
            Slot::text("intro", "Opening text"),
            Slot::text("details", "Sign-in details"),
            Slot::text("closing", "Text before the link to Your sessions"),
        ],
        variables: &[
            Variable::text("time", "When the sign-in happened", "2026-11-20 09:15", DEVICE_MAIL),
            Variable::text("device", "Device", "Chrome on Windows", DEVICE_MAIL),
            Variable::text("ip_address", "IP address", "203.0.113.7", DEVICE_MAIL),
            Variable::text("place", "Place", "Kuala Lumpur, Malaysia", DEVICE_MAIL),
        ],
        base_language: "en",
        defaults: &[DefaultText {
            language: "en",
            version: "1",
            slots: &[
                ("subject", "New sign-in to your Bilibi account"),
                ("intro", "Your Bilibi account was signed in from a device it has not used before."),
                ("details", "Time: {{time}}\nDevice: {{device}}\nIP address: {{ip_address}}\nPlace: {{place}}"),
                ("closing", "If this was you, you can ignore this email.\n\nIf this wasn't you, change your password now. Then sign that session out in Your sessions:"),
            ],
        }],
    }
);

pub(crate) const TWO_FACTOR_SUBJECT: &str = "Your 2FA was reset";
pub(crate) const TWO_FACTOR_ADMIN_BODY: &str = "An admin turned off your 2FA methods and disconnected your AI apps. Sign in and set up 2FA again in Settings.";
pub(crate) const TWO_FACTOR_SYSTEM_BODY: &str = "A server administrator turned off your 2FA methods and disconnected your AI apps. Sign in and set up 2FA again in Settings.";
template!(
    TWO_FACTOR_RESET,
    Template {
        key: TWO_FACTOR_RESET,
        label: "2FA reset email",
        audience: Audience::Staff,
        outputs: &[Output::PlainText],
        slots: &[
            Slot::subject(),
            Slot::text("by_admin", "Message when a workspace admin reset it"),
            Slot::text(
                "by_server_admin",
                "Message when a server administrator reset it"
            ),
        ],
        variables: &[],
        base_language: "en",
        defaults: &[DefaultText {
            language: "en",
            version: "1",
            slots: &[
                ("subject", TWO_FACTOR_SUBJECT),
                ("by_admin", TWO_FACTOR_ADMIN_BODY),
                ("by_server_admin", TWO_FACTOR_SYSTEM_BODY),
            ],
        }],
    }
);
