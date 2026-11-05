//! The reading view for received mail. A Domain never passes received HTML to
//! `Html::raw`: it shows the stored output of `bilibi_mail::sanitise_html` here.
use crate::Assets;
use askama::Template;

/// Extra limits inside the frame, on top of the page policy the frame takes on
/// (`srcdoc` keeps the page's CSP): no script, no remote fetch, images only
/// from `data:`. Style is left to the page policy, which allows only the
/// frame stylesheet, so inline `style` attributes are not applied.
const FRAME_POLICY: &str = "script-src 'none'; img-src data:; media-src 'none'; font-src 'none'; connect-src 'none'; object-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'none'";

/// Sanitised mail HTML in a sandboxed `<iframe srcdoc>`. The sandbox grants
/// nothing but opening a clicked link in a new tab: script, forms, plugins
/// and same-origin access stay off. Remote images were removed by the
/// sanitiser and are also refused by the frame's policy, so opening a message
/// fires no tracking pixel.
///
/// `html` must be `sanitise_html` output; for inline images, first rewrite
/// its `cid:` sources with `bilibi_mail::inline_images`. The whole document is
/// escaped into the `srcdoc` attribute, never inserted as markup.
#[derive(Template, Debug)]
#[template(
    source = r#"{% import "components.html" as ui %}{% call ui::mail_frame(id=id, title=title, document=document) %}{% endcall %}"#,
    ext = "html"
)]
pub struct MailFrame {
    id: String,
    title: String,
    document: String,
}

impl MailFrame {
    /// `id` must be stable (the live patcher keys an iframe by it); `title`
    /// names the frame for assistive technology, for example the subject.
    pub fn new(id: &str, title: &str, html: &str) -> Self {
        let stylesheet = Assets::url("/assets/mail-frame.css");
        Self {
            id: id.into(),
            title: title.into(),
            document: format!(
                "<!doctype html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"{FRAME_POLICY}\"><meta name=\"referrer\" content=\"no-referrer\"><base target=\"_blank\"><link rel=\"stylesheet\" href=\"{stylesheet}\"></head><body>{html}</body></html>"
            ),
        }
    }
}
