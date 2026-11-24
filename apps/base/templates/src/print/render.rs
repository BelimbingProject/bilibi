use super::{
    facts::{CheckedFacts, PrintValue},
    layout::{Block, Column},
    Checked, Problem,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use bilibi_artifacts::{InlineDocument, PdfPaper};
use bilibi_branding::PrintBrand;
use bilibi_ui::Assets;
use std::{
    fmt::{self, Write},
    sync::OnceLock,
};

const MAX_HTML_BYTES: usize = 4 * 1024 * 1024;

pub(super) fn fragment(blocks: &[Block], facts: &CheckedFacts<'_>) -> Checked<String> {
    let mut html = Html(String::new());
    html.blocks(blocks, facts)
        .map_err(|_| Problem::new("layout", "The rendered print layout exceeds 4 MiB."))?;
    Ok(html.0)
}

pub(super) fn document(
    fragment: String,
    language: &str,
    title: &str,
    paper: PdfPaper,
    brand: &PrintBrand,
) -> Checked<InlineDocument> {
    let mut html = Html(String::new());
    html.document(&fragment, language, title, brand)
        .map_err(|_| Problem::new("layout", "The rendered print layout exceeds 4 MiB."))?;
    Ok(InlineDocument::new(html.0, paper))
}

fn packaged_data_url(path: &str, cache: &'static OnceLock<String>) -> &'static str {
    cache.get_or_init(|| {
        let asset = Assets::get(path).expect("registered print asset");
        format!(
            "data:{};base64,{}",
            asset.content_type,
            STANDARD.encode(asset.bytes.as_ref())
        )
    })
}

struct Html(String);

impl Write for Html {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.0.len() + text.len() > MAX_HTML_BYTES {
            return Err(fmt::Error);
        }
        self.0.push_str(text);
        Ok(())
    }
}

impl Html {
    fn document(
        &mut self,
        fragment: &str,
        language: &str,
        title: &str,
        brand: &PrintBrand,
    ) -> fmt::Result {
        static FONT: OnceLock<String> = OnceLock::new();
        let font = packaged_data_url("/fonts/instrument-sans-variable.woff2", &FONT);
        self.write_str("<!doctype html><html lang=\"")?;
        self.escaped(language)?;
        write!(
            self,
            "\" data-accent=\"{}\"><head><meta charset=\"utf-8\">",
            brand.accent().as_str()
        )?;
        self.write_str(concat!(
            "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; ",
            "style-src 'unsafe-inline'; font-src data:; img-src data:; ",
            "base-uri 'none'; form-action 'none'\">"
        ))?;
        self.element("title", title)?;
        write!(
            self,
            "<style>@font-face{{font-family:'Instrument Sans';src:url('{font}') format('woff2');font-weight:400 700}}{}</style></head><body><main>",
            include_str!("document.css"),
        )?;
        self.write_str(fragment)?;
        self.write_str("</main>")?;
        if brand.include_mark() {
            self.mark(brand, language)?;
        }
        self.write_str("</body></html>")
    }

    fn mark(&mut self, brand: &PrintBrand, language: &str) -> fmt::Result {
        static MARK: OnceLock<String> = OnceLock::new();
        self.write_str("<footer>")?;
        if let Some(logo) = brand.logo() {
            write!(
                self,
                "<img alt=\"\" src=\"data:{};base64,{}\">",
                logo.mime(),
                STANDARD.encode(logo.bytes())
            )?;
        } else if brand.use_product_mark() {
            let logo = packaged_data_url("/assets/logo.svg", &MARK);
            write!(self, "<img alt=\"\" src=\"{logo}\">")?;
        }
        let attribution = match language.split('-').next() {
            Some("ms") => "Dikuasakan oleh ",
            _ => "Powered by ",
        };
        self.write_str("<span>")?;
        self.escaped(attribution)?;
        self.escaped(brand.name())?;
        self.write_str("</span></footer>")
    }

    fn escaped(&mut self, text: &str) -> fmt::Result {
        let mut start = 0;
        for (index, ch) in text.char_indices() {
            let escape = match ch {
                '&' => "&amp;",
                '<' => "&lt;",
                '>' => "&gt;",
                '"' => "&quot;",
                '\'' => "&#39;",
                '\0' => "�",
                _ => continue,
            };
            self.write_str(&text[start..index])?;
            self.write_str(escape)?;
            start = index + ch.len_utf8();
        }
        self.write_str(&text[start..])
    }

    fn element(&mut self, tag: &str, text: &str) -> fmt::Result {
        write!(self, "<{tag}>")?;
        self.escaped(text)?;
        write!(self, "</{tag}>")
    }

    fn multiline(&mut self, text: &str) -> fmt::Result {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        for (index, line) in text.split('\n').enumerate() {
            if index != 0 {
                self.write_str("<br>")?;
            }
            self.escaped(line)?;
        }
        Ok(())
    }

    fn value(&mut self, value: PrintValue<'_>) -> fmt::Result {
        match value {
            PrintValue::Text(text) => self.multiline(text),
            PrintValue::Integer(number) => write!(self, "{number}"),
            PrintValue::Decimal(number) => write!(self, "{number}"),
            PrintValue::Money { amount, currency } => write!(self, "{currency} {amount}"),
            PrintValue::Date(date) => write!(self, "{date}"),
            PrintValue::Boolean(flag) => write!(self, "{flag}"),
            PrintValue::Address(lines) => {
                for (index, line) in lines.iter().enumerate() {
                    if index != 0 {
                        self.write_str("<br>")?;
                    }
                    self.multiline(line)?;
                }
                Ok(())
            }
            PrintValue::Rows(_) => unreachable!("the compiler requires scalar facts here"),
        }
    }

    fn blocks(&mut self, blocks: &[Block], facts: &CheckedFacts<'_>) -> fmt::Result {
        for block in blocks {
            match block {
                Block::Heading { text } => self.element("h1", text)?,
                Block::Paragraph { text } => self.element("p", text)?,
                Block::Field { field, label } => {
                    if let Some(value) = facts.get(field.as_str()) {
                        self.write_str("<dl>")?;
                        self.element("dt", label.as_deref().expect("compiled field label"))?;
                        self.write_str("<dd>")?;
                        self.value(*value)?;
                        self.write_str("</dd></dl>")?;
                    }
                }
                Block::Section { heading, blocks } => {
                    self.write_str("<section>")?;
                    self.element("h2", heading)?;
                    self.blocks(blocks, facts)?;
                    self.write_str("</section>")?;
                }
                Block::IfPresent { field, blocks } => {
                    if facts.contains_key(field.as_str()) {
                        self.blocks(blocks, facts)?;
                    }
                }
                Block::Table { rows, columns } => {
                    if let Some(PrintValue::Rows(rows)) = facts.get(rows.as_str()) {
                        self.table(rows, columns)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn table(&mut self, rows: &[super::facts::PrintFacts<'_>], columns: &[Column]) -> fmt::Result {
        self.write_str("<table><thead><tr>")?;
        for column in columns {
            self.write_str(if column.numeric {
                r#"<th scope="col" class="numeric">"#
            } else {
                r#"<th scope="col">"#
            })?;
            self.escaped(column.heading.as_deref().expect("compiled column heading"))?;
            self.write_str("</th>")?;
        }
        self.write_str("</tr></thead><tbody>")?;
        for row in rows {
            self.write_str("<tr>")?;
            for column in columns {
                self.write_str(if column.numeric {
                    r#"<td class="numeric">"#
                } else {
                    "<td>"
                })?;
                if let Some((_, value)) = row.iter().find(|(id, _)| *id == column.field) {
                    self.value(*value)?;
                }
                self.write_str("</td>")?;
            }
            self.write_str("</tr>")?;
        }
        self.write_str("</tbody></table>")
    }
}
