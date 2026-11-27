//! Untrusted Markdown enters here; only parser-generated, filtered HTML leaves.
use askama::Template;
use pulldown_cmark::{html, Event, Parser, Tag, TagEnd};

use crate::Html;

/// A safe reading view of plain-text CommonMark. Keep the original source in
/// the owner's textarea and storage; this component never rewrites it.
#[derive(Clone, Debug, Template)]
#[template(path = "markdown.html")]
pub struct Markdown {
    body: Html,
}

impl Markdown {
    pub fn new(source: &str) -> Self {
        let mut links = Vec::new();
        let mut image_depth = 0;
        // No optional syntax extensions: the supported subset stays small.
        let events = Parser::new(source)
            .into_offset_iter()
            .filter_map(|(event, range)| {
                // Images stay literal, including their destination, without ever
                // creating a media element or making a network request.
                if image_depth > 0 {
                    match event {
                        Event::Start(Tag::Image { .. }) => image_depth += 1,
                        Event::End(TagEnd::Image) => image_depth -= 1,
                        _ => {}
                    }
                    return None;
                }
                match event {
                    Event::Start(Tag::Image { .. }) => {
                        image_depth = 1;
                        Some(Event::Text(source[range].into()))
                    }
                    // Render raw HTML as escaped text, never as an HTML event.
                    Event::Html(text) | Event::InlineHtml(text) => Some(Event::Text(text)),
                    Event::Start(Tag::HtmlBlock) | Event::End(TagEnd::HtmlBlock) => None,
                    Event::Start(Tag::Link { ref dest_url, .. }) => {
                        let allowed = safe_link(dest_url);
                        links.push(allowed);
                        allowed.then_some(event)
                    }
                    Event::End(TagEnd::Link) => links.pop().unwrap_or(false).then_some(event),
                    // A thematic break is outside the documented subset.
                    Event::Rule => Some(Event::Text(source[range].into())),
                    other => Some(other),
                }
            });
        let mut rendered = String::new();
        // The maintained renderer escapes text, code, titles and attributes.
        html::push_html(&mut rendered, events);
        Self {
            body: Html::raw(rendered),
        }
    }
}

fn safe_link(destination: &str) -> bool {
    // Browsers discard some whitespace/control characters in schemes and
    // treat backslashes as slashes. Refuse these ambiguous spellings.
    if destination
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
        || destination.starts_with("//")
    {
        return false;
    }
    // A colon before the first path/query/fragment separator denotes a scheme.
    let first = destination
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    match first.split_once(':') {
        Some((scheme, _)) => ["http", "https", "mailto"]
            .iter()
            .any(|allowed| scheme.eq_ignore_ascii_case(allowed)),
        None => true,
    }
}
