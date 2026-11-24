//! One outgoing HTML layout over rendered prose. Received HTML uses the
//! separate sanitiser and never reaches this renderer.
use crate::{message::render_parts, Address, Attachment, Limits, Message, Rendered, Result};
use askama::Template;
use bilibi_ui::{Accent, PRODUCT};
use std::{future::Future, pin::Pin};

/// Installation branding for mail. Owners supply only authorized raster bytes
/// for a logo, which travels inline with the message.
#[derive(Default)]
pub struct MailBrand {
    pub name: String,
    pub accent: Accent,
    pub logo: Option<Attachment>,
}

pub type BrandFuture<'a> = Pin<Box<dyn Future<Output = MailBrand> + Send + 'a>>;
pub trait BrandSource: Send + Sync {
    fn current(&self) -> BrandFuture<'_>;
}

#[derive(Template)]
#[template(path = "branded.html")]
struct Branded<'a> {
    subject: &'a str,
    name: &'a str,
    accent: &'static str,
    logo_id: Option<&'a str>,
    body: &'a str,
    action: Option<&'a str>,
}

impl Branded<'_> {
    fn html(&self, max_bytes: usize) -> Result<String> {
        struct LimitedString {
            text: String,
            max_bytes: usize,
        }
        impl std::fmt::Write for LimitedString {
            fn write_str(&mut self, part: &str) -> std::fmt::Result {
                if part.len() > self.max_bytes - self.text.len() {
                    return Err(std::fmt::Error);
                }
                self.text.push_str(part);
                Ok(())
            }
        }
        let mut output = LimitedString {
            text: String::new(),
            max_bytes,
        };
        self.render_into(&mut output)
            .map_err(|_| crate::Error::Invalid)?;
        Ok(output.text)
    }

    fn is_action(&self, line: &str) -> bool {
        self.action == Some(line)
    }
}

pub(crate) fn render(
    message: &Message,
    from: &Address,
    limits: Limits,
    brand: &MailBrand,
    action: Option<&str>,
) -> Result<Rendered> {
    let html_budget = limits
        .max_message_bytes
        .checked_sub(message.text.len())
        .ok_or(crate::Error::Invalid)?;
    let action = action.filter(|link| {
        url::Url::parse(link).is_ok_and(|url| {
            matches!(url.scheme(), "https" | "http")
                && url.has_host()
                && url.username().is_empty()
                && url.password().is_none()
        })
    });
    let name = if brand.name.is_empty() {
        PRODUCT
    } else {
        &brand.name
    };
    let logo_id = brand
        .logo
        .as_ref()
        .map(|_| format!("{}@bilibi", uuid::Uuid::new_v4()));
    let mut layout = Branded {
        subject: &message.subject,
        name,
        accent: match brand.accent {
            Accent::Lime => "#84a617",
            Accent::Teal => "#0d9488",
            Accent::Cyan => "#0891b2",
            Accent::Indigo => "#6366f1",
            Accent::Violet => "#8b5cf6",
            Accent::Fuchsia => "#c026d3",
        },
        logo_id: logo_id.as_deref(),
        body: &message.text,
        action,
    };
    let render = |layout: &Branded<'_>, logo| {
        let html = layout.html(html_budget)?;
        render_parts(message, from, limits, Some(&html), logo)
    };
    let branded = match render(&layout, logo_id.as_deref().zip(brand.logo.as_ref())) {
        Err(crate::Error::Invalid) if brand.logo.is_some() => {
            layout.logo_id = None;
            render(&layout, None)
        }
        result => result,
    };
    match branded {
        Err(crate::Error::Invalid) => render_parts(message, from, limits, None, None),
        result => result,
    }
}
