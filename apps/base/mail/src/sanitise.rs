//! The one allow-list for HTML written by a stranger (a received email body).
//! Apply it before storing; show the stored result with `bilibi_ui::MailFrame`.
use std::{borrow::Cow, collections::HashSet, sync::LazyLock};

/// Inline style properties kept: layout and text only. A value holding
/// `url(...)` or any escape is dropped with its declaration.
const STYLE_PROPERTIES: &[&str] = &[
    "color",
    "background-color",
    "font",
    "font-family",
    "font-size",
    "font-style",
    "font-weight",
    "text-align",
    "text-decoration",
    "text-transform",
    "text-indent",
    "line-height",
    "letter-spacing",
    "white-space",
    "vertical-align",
    "width",
    "max-width",
    "min-width",
    "height",
    "margin",
    "margin-top",
    "margin-right",
    "margin-bottom",
    "margin-left",
    "padding",
    "padding-top",
    "padding-right",
    "padding-bottom",
    "padding-left",
    "border",
    "border-top",
    "border-right",
    "border-bottom",
    "border-left",
    "border-color",
    "border-style",
    "border-width",
    "border-collapse",
    "border-spacing",
    "border-radius",
    "display",
    "list-style-type",
];

static CLEANER: LazyLock<ammonia::Builder<'static>> = LazyLock::new(|| {
    let mut builder = ammonia::Builder::default();
    builder
        // Default tags: text structure, lists, tables, links and images; no
        // script, style, iframe, object, embed or form.
        .rm_tags(["map", "area"])
        .add_clean_content_tags(["title", "head", "noscript", "template"])
        .generic_attributes(HashSet::from([
            "title", "lang", "dir", "style", "align", "valign", "width", "height",
        ]))
        .tag_attributes(
            [
                ("a", &["href"][..]),
                ("img", &["src", "alt", "width", "height", "border"]),
                (
                    "table",
                    &["border", "cellpadding", "cellspacing", "bgcolor"],
                ),
                ("td", &["colspan", "rowspan", "bgcolor", "nowrap"]),
                ("th", &["colspan", "rowspan", "bgcolor", "scope"]),
                ("tr", &["bgcolor"]),
                ("ol", &["start", "type"]),
                ("font", &["color", "face", "size"]),
            ]
            .into_iter()
            .map(|(tag, attributes)| (tag, attributes.iter().copied().collect()))
            .collect(),
        )
        .add_tags(["font", "center"])
        .url_schemes(HashSet::from(["http", "https", "mailto", "cid", "data"]))
        .url_relative(ammonia::UrlRelative::Deny)
        .link_rel(Some("noopener noreferrer"))
        .strip_comments(true)
        .filter_style_properties(STYLE_PROPERTIES.iter().copied().collect())
        .attribute_filter(filter);
    builder
});

/// Reduces untrusted HTML to the allow-list:
/// - kept: text structure, lists, tables, links and images;
/// - removed: `script`, `style`, `iframe`, `object`, `embed`, `form`, every
///   `on*` attribute, comments;
/// - links: only `http`, `https` and `mailto`, with `rel="noopener noreferrer"`;
/// - images: only `cid:` (an inline part) and `data:image/*`, so showing a
///   message never fetches anything;
/// - inline `style`: only layout and text properties, never `url(...)`.
pub fn sanitise_html(html: &str) -> String {
    CLEANER.clean(html).to_string()
}

fn filter<'u>(element: &str, attribute: &str, value: &'u str) -> Option<Cow<'u, str>> {
    let lower = value.trim().to_ascii_lowercase();
    match (element, attribute) {
        ("a", "href") => ["http://", "https://", "mailto:"]
            .iter()
            .any(|scheme| lower.starts_with(scheme))
            .then_some(Cow::Borrowed(value)),
        ("img", "src") => (lower.starts_with("cid:") || lower.starts_with("data:image/"))
            .then_some(Cow::Borrowed(value)),
        (_, "style") => {
            let kept: Vec<&str> = value
                .split(';')
                .map(str::trim)
                .filter(|declaration| {
                    let lower = declaration.to_ascii_lowercase();
                    !lower.is_empty()
                        && ![
                            "url(",
                            "expression(",
                            "\\",
                            "@",
                            "image-set(",
                            "javascript:",
                        ]
                        .iter()
                        .any(|banned| lower.contains(banned))
                })
                .collect();
            (!kept.is_empty()).then(|| Cow::Owned(kept.join("; ")))
        }
        _ => Some(Cow::Borrowed(value)),
    }
}

/// An inline image to show in place of its `cid:` reference. `content_type`
/// is the type the caller detected when it stored the bytes.
pub struct InlineImage<'a> {
    pub content_id: &'a str,
    pub content_type: &'a str,
    pub bytes: &'a [u8],
}

/// Rewrites `src="cid:<id>"` in `sanitise_html` output to a `data:` URI for
/// each matching image, so `MailFrame` can show inline images without a
/// fetch. Parts that are not `image/*` are skipped; an unmatched `cid:` stays
/// as it is, and the browser shows nothing for it.
pub fn inline_images<'a>(html: &str, images: impl IntoIterator<Item = InlineImage<'a>>) -> String {
    let mut html = html.to_owned();
    for image in images {
        let content_type = image.content_type.trim().to_ascii_lowercase();
        let token = |b: u8| b.is_ascii_alphanumeric() || b"+-.".contains(&b);
        let valid = content_type
            .strip_prefix("image/")
            .is_some_and(|subtype| !subtype.is_empty() && subtype.bytes().all(token));
        if !valid || image.content_id.is_empty() {
            continue;
        }
        // The serializer writes attribute values double-quoted, `&` as `&amp;`.
        let reference = format!(
            "src=\"cid:{}\"",
            image
                .content_id
                .replace('&', "&amp;")
                .replace('"', "&quot;")
                .replace('\u{a0}', "&nbsp;")
        );
        let data = format!(
            "src=\"data:{content_type};base64,{}\"",
            data_encoding::BASE64.encode(image.bytes)
        );
        html = html.replace(&reference, &data);
    }
    html
}
