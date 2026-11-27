//! The preview frame: a file the owner serves from its own authorized route,
//! shown in place. Pair it with `FileBody::preview` (`src/download.rs`), which
//! decides what may be shown.
use askama::Template;

/// A file shown in place from the owner's own same-origin route (an image,
/// a PDF or plain text), for a preview beside a list or a record. The route
/// answers `FileBody::inline` for a type it chooses to show, behind the same
/// authorization as the download, and its response policy forbids scripts;
/// this only frames the address. Pass a route the owner built, never request
/// data. Give it a stable `id` (the live patcher keys an iframe by it) and a
/// `title` naming the file for assistive technology.
#[derive(Template, Debug)]
#[template(
    source = r#"{% import "components.html" as ui %}{% call ui::file_preview(id=id, src=src, title=title) %}{% endcall %}"#,
    ext = "html"
)]
pub struct FilePreview<'a> {
    pub id: &'a str,
    pub src: &'a str,
    pub title: &'a str,
}
