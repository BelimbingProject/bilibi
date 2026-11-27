//! PostgreSQL source shape validation and process-wide caching.

use super::CatalogError;
use crate::Source;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};
use tokio_postgres::{types::Type, GenericClient};

/// Static source descriptions contain selected keys and arity, never scope data.
#[derive(Debug)]
pub(super) struct SourceDescription {
    pub(super) keys: Vec<String>,
    pub(super) parameters: usize,
}
type Description = Arc<SourceDescription>;
type Descriptions = HashMap<(&'static str, bool, bool), Description>;
fn descriptions() -> &'static Mutex<Descriptions> {
    static CACHE: OnceLock<Mutex<Descriptions>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// A static statement's shape, read from the server's description of it
/// (nothing runs) and kept for the process: static SQL has a fixed shape. Every placeholder must be a bigint the grid can bind.
pub(super) async fn describe_source(
    client: &impl GenericClient,
    what: &str,
    source: &Source,
) -> Result<Description, CatalogError> {
    let cache_key = (source.sql, source.tenant_scoped, source.company_scoped);
    if let Some(keys) = descriptions()
        .lock()
        .ok()
        .and_then(|c| c.get(&cache_key).cloned())
    {
        return Ok(keys);
    }
    let statement = client.prepare(source.sql).await.map_err(|error| {
        CatalogError::Misdeclared(format!("{what}: the server refused the source: {error}"))
    })?;
    let expected = if source.company_scoped {
        3..=3
    } else if source.tenant_scoped {
        1..=2
    } else {
        0..=0
    };
    if !expected.contains(&statement.params().len())
        || statement.params().iter().any(|ty| *ty != Type::INT8)
    {
        return Err(CatalogError::Misdeclared(format!(
            "{what}: a tenant source takes bigint $1 (tenant) and optionally $2 (user), a company source takes $1 (tenant), $2 (user) and $3 (company), and a global source takes none"
        )));
    }
    let keys = Arc::new(SourceDescription {
        keys: statement
            .columns()
            .iter()
            .map(|column| column.name().to_string())
            .collect(),
        parameters: statement.params().len(),
    });
    if let Ok(mut cache) = descriptions().lock() {
        cache.insert(cache_key, keys.clone());
    }
    Ok(keys)
}
