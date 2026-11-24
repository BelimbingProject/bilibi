use super::{
    facts::{CheckedFacts, PrintValue},
    layout::{Block, Column},
    Checked, Problem,
};
use std::fmt::{self, Write};

const MAX_HTML_BYTES: usize = 4 * 1024 * 1024;

pub(super) fn fragment(blocks: &[Block], facts: &CheckedFacts<'_>) -> Checked<String> {
    let mut html = Html(String::new());
    html.blocks(blocks, facts)
        .map_err(|_| Problem::new("layout", "The rendered print layout exceeds 4 MiB."))?;
    Ok(html.0)
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

    fn value(&mut self, value: PrintValue<'_>) -> fmt::Result {
        match value {
            PrintValue::Text(text) => self.escaped(text),
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
                    self.escaped(line)?;
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
                        self.element("dt", label)?;
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
            self.element("th", &column.heading)?;
        }
        self.write_str("</tr></thead><tbody>")?;
        for row in rows {
            self.write_str("<tr>")?;
            for column in columns {
                self.write_str("<td>")?;
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
