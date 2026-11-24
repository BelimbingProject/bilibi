use super::{render, Checked, PrintContract, PrintField, PrintKind, Problem, MAX_LAYOUT_BYTES};
use crate::print::facts::PrintFacts;
use serde::Deserialize;
use std::collections::BTreeSet;

const MAX_DEPTH: usize = 8;
const MAX_NODES: usize = 160;
const MAX_COLUMNS: usize = 16;
const MAX_LITERAL_BYTES: usize = 2_000;

pub struct CompiledPrint<'contract> {
    contract: &'contract PrintContract,
    blocks: Vec<Block>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Layout {
    blocks: Vec<Block>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Block {
    Heading { text: String },
    Paragraph { text: String },
    Field { field: String, label: String },
    Section { heading: String, blocks: Vec<Block> },
    IfPresent { field: String, blocks: Vec<Block> },
    Table { rows: String, columns: Vec<Column> },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Column {
    pub field: String,
    pub heading: String,
}

pub(super) fn compile<'a>(contract: &'a PrintContract, source: &str) -> Checked<CompiledPrint<'a>> {
    if source.len() > MAX_LAYOUT_BYTES {
        return Err(Problem::new("layout", "The print layout exceeds 48 KiB."));
    }
    let layout: Layout = serde_json::from_str(source)
        .map_err(|error| Problem::new("layout", format!("Invalid print layout: {error}")))?;
    let mut remaining = MAX_NODES;
    validate_blocks(contract.fields, &layout.blocks, 1, &mut remaining)?;
    Ok(CompiledPrint {
        contract,
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

    fn render_facts(&self, facts: PrintFacts<'_>) -> Checked<String> {
        let facts = super::facts::check_facts(self.contract.fields, facts)?;
        render::fragment(&self.blocks, &facts)
    }
}

fn validate_blocks(
    fields: &[PrintField],
    blocks: &[Block],
    depth: usize,
    remaining: &mut usize,
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
                scalar(fields, field)?;
                literal("label", label)?;
            }
            Block::Section { heading, blocks } => {
                literal("heading", heading)?;
                validate_blocks(fields, blocks, depth + 1, remaining)?;
            }
            Block::IfPresent { field, blocks } => {
                if !lookup(fields, field)?.optional {
                    return Err(Problem::new(
                        field,
                        "Presence conditions require an optional fact.",
                    ));
                }
                validate_blocks(fields, blocks, depth + 1, remaining)?;
            }
            Block::Table { rows, columns } => validate_table(fields, rows, columns)?,
        }
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

fn scalar(fields: &[PrintField], id: &str) -> Checked<()> {
    if matches!(lookup(fields, id)?.kind, PrintKind::Rows(_)) {
        return Err(Problem::new(id, "Repeated facts require a table."));
    }
    Ok(())
}

fn validate_table(fields: &[PrintField], rows: &str, columns: &[Column]) -> Checked<()> {
    let PrintKind::Rows(fields) = lookup(fields, rows)?.kind else {
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
        scalar(fields, &column.field)?;
        literal("heading", &column.heading)?;
        if !seen.insert(&column.field) {
            return Err(Problem::new(
                &column.field,
                "A column can appear only once in a table.",
            ));
        }
    }
    Ok(())
}
