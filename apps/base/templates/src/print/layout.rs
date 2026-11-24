use super::{render, Checked, PrintContract, PrintField, PrintKind, Problem, MAX_LAYOUT_BYTES};
use crate::print::facts::PrintFacts;
use bilibi_artifacts::{InlineDocument, PdfPaper};
use bilibi_ui::Accent;
use serde::Deserialize;
use std::collections::BTreeSet;

const MAX_DEPTH: usize = 8;
const MAX_NODES: usize = 160;
const MAX_COLUMNS: usize = 16;
const MAX_LITERAL_BYTES: usize = 2_000;

pub struct CompiledPrint<'contract> {
    contract: &'contract PrintContract,
    language: String,
    paper: PdfPaper,
    blocks: Vec<Block>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Layout {
    #[serde(default)]
    paper: PdfPaper,
    blocks: Vec<Block>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Block {
    Heading {
        text: String,
    },
    Paragraph {
        text: String,
    },
    Field {
        field: String,
        label: Option<String>,
    },
    Section {
        heading: String,
        blocks: Vec<Block>,
    },
    IfPresent {
        field: String,
        blocks: Vec<Block>,
    },
    Table {
        rows: String,
        columns: Vec<Column>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Column {
    pub field: String,
    pub heading: Option<String>,
    #[serde(skip)]
    pub numeric: bool,
}

pub(super) fn compile<'a>(
    contract: &'a PrintContract,
    language: &str,
    source: &str,
) -> Checked<CompiledPrint<'a>> {
    if source.len() > MAX_LAYOUT_BYTES {
        return Err(Problem::new("layout", "The print layout exceeds 48 KiB."));
    }
    let language = crate::catalogue::language(language)
        .ok_or_else(|| Problem::new("language", "Enter a valid print language tag."))?;
    let mut layout: Layout = serde_json::from_str(source)
        .map_err(|error| Problem::new("layout", format!("Invalid print layout: {error}")))?;
    let mut remaining = MAX_NODES;
    let mut shown = BTreeSet::new();
    validate_blocks(
        contract.fields,
        &mut layout.blocks,
        &language,
        1,
        &mut remaining,
        Some(&mut shown),
    )?;
    for field in contract.fields {
        if field.requires_output() && !shown.contains(field.id) {
            return Err(Problem::new(
                field.id,
                "Required print output must appear outside presence conditions.",
            ));
        }
    }
    Ok(CompiledPrint {
        contract,
        language,
        paper: layout.paper,
        blocks: layout.blocks,
    })
}

impl CompiledPrint<'_> {
    /// Render only the declaration's synthetic facts.
    pub fn preview(&self) -> Checked<String> {
        let samples: Vec<_> = self
            .contract
            .fields
            .iter()
            .map(|field| (field.id, field.sample))
            .collect();
        self.render_facts(&samples)
    }

    /// Render escaped HTML from a business snapshot. No queries or calculations are performed.
    pub fn render(&self, state: &str, facts: PrintFacts<'_>) -> Checked<String> {
        if !self.contract.states.contains(&state) {
            return Err(Problem::new(
                "state",
                "This document cannot print in the given state.",
            ));
        }
        self.render_facts(facts)
    }

    /// Complete offline HTML and its approved physical size for Base Artifacts.
    pub fn document(
        &self,
        state: &str,
        facts: PrintFacts<'_>,
        accent: Accent,
    ) -> Checked<InlineDocument> {
        render::document(
            self.render(state, facts)?,
            &self.language,
            self.contract.label,
            self.paper,
            accent,
        )
    }

    fn render_facts(&self, facts: PrintFacts<'_>) -> Checked<String> {
        let facts = super::facts::check_facts(self.contract.fields, facts)?;
        render::fragment(&self.blocks, &facts)
    }
}

fn validate_blocks(
    fields: &[PrintField],
    blocks: &mut [Block],
    language: &str,
    depth: usize,
    remaining: &mut usize,
    mut shown: Option<&mut BTreeSet<&'static str>>,
) -> Checked<()> {
    if blocks.is_empty() || depth > MAX_DEPTH {
        return Err(Problem::new(
            "blocks",
            "Supply blocks with at most eight levels of nesting.",
        ));
    }
    *remaining = remaining
        .checked_sub(blocks.len())
        .ok_or_else(|| Problem::new("blocks", "The print layout exceeds 160 blocks."))?;
    for block in blocks {
        match block {
            Block::Heading { text } | Block::Paragraph { text } => literal("text", text)?,
            Block::Field { field, label } => {
                let field = scalar(fields, field)?;
                resolve_label(field, language, label)?;
                place_required(field, shown.as_deref_mut())?;
            }
            Block::Section { heading, blocks } => {
                literal("heading", heading)?;
                validate_blocks(
                    fields,
                    blocks,
                    language,
                    depth + 1,
                    remaining,
                    shown.as_deref_mut(),
                )?;
            }
            Block::IfPresent { field, blocks } => {
                if !lookup(fields, field)?.optional {
                    return Err(Problem::new(
                        field,
                        "Presence conditions require an optional fact.",
                    ));
                }
                validate_blocks(fields, blocks, language, depth + 1, remaining, None)?;
            }
            Block::Table { rows, columns } => {
                let rows = lookup(fields, rows)?;
                validate_table(rows, language, columns)?;
                place_required(rows, shown.as_deref_mut())?;
            }
        }
    }
    Ok(())
}

fn place_required(field: &PrintField, shown: Option<&mut BTreeSet<&'static str>>) -> Checked<()> {
    if !field.requires_output() {
        return Ok(());
    }
    let shown = shown.ok_or_else(|| {
        Problem::new(
            field.id,
            "Required print output cannot be placed in a presence condition.",
        )
    })?;
    if !shown.insert(field.id) {
        return Err(Problem::new(
            field.id,
            "Required print output must appear exactly once.",
        ));
    }
    Ok(())
}

fn literal(id: &str, text: &str) -> Checked<()> {
    if text.trim().is_empty() || text.len() > MAX_LITERAL_BYTES {
        return Err(Problem::new(id, "Enter 1 to 2,000 bytes of print wording."));
    }
    Ok(())
}

fn lookup<'a>(fields: &'a [PrintField], id: &str) -> Checked<&'a PrintField> {
    fields
        .iter()
        .find(|field| field.id == id)
        .ok_or_else(|| Problem::new(id, "The print contract does not declare this field."))
}

fn scalar<'a>(fields: &'a [PrintField], id: &str) -> Checked<&'a PrintField> {
    let field = lookup(fields, id)?;
    if matches!(field.kind, PrintKind::Rows(_)) {
        return Err(Problem::new(id, "Repeated facts require a table."));
    }
    Ok(field)
}

fn resolve_label(field: &PrintField, language: &str, label: &mut Option<String>) -> Checked<()> {
    if field.required_output {
        if label.is_some() {
            return Err(Problem::new(
                field.id,
                "Required print wording comes from the owner.",
            ));
        }
        let mut matches = field
            .labels
            .iter()
            .filter(|(locale, _)| *locale == language);
        let Some((_, wording)) = matches.next().filter(|_| matches.next().is_none()) else {
            return Err(Problem::new(
                field.id,
                "Declare one owner label for the print language.",
            ));
        };
        *label = Some((*wording).to_owned());
    }
    literal(field.id, label.as_deref().unwrap_or_default())
}

fn validate_table(table: &PrintField, language: &str, columns: &mut [Column]) -> Checked<()> {
    let rows = table.id;
    let PrintKind::Rows(fields) = table.kind else {
        return Err(Problem::new(rows, "A table needs a repeated field."));
    };
    if columns.is_empty() || columns.len() > MAX_COLUMNS {
        return Err(Problem::new(
            rows,
            "Declare between 1 and 16 table columns.",
        ));
    }
    let mut seen = BTreeSet::new();
    for column in columns {
        let field = scalar(fields, &column.field)?;
        resolve_label(field, language, &mut column.heading)?;
        column.numeric = matches!(
            field.kind,
            PrintKind::Integer | PrintKind::Decimal | PrintKind::Money
        );
        if !seen.insert(field.id) {
            return Err(Problem::new(
                &column.field,
                "A column can appear only once in a table.",
            ));
        }
    }
    for field in fields {
        if field.required_output && !seen.contains(field.id) {
            return Err(Problem::new(
                &format!("{rows}.{}", field.id),
                "Required print column is missing.",
            ));
        }
    }
    Ok(())
}
