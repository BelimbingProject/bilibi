use std::{collections::BTreeMap, future::Future};

tokio::task_local! {
    static WORDS: FieldLabels;
}

#[derive(Clone, Debug, Default)]
pub struct FieldLabels(pub BTreeMap<String, String>);

impl FieldLabels {
    /// Runs `render` with these labels. A plain function, not an `async fn`,
    /// so `render` is held once rather than twice (#965).
    pub fn during<F: Future>(&self, render: F) -> impl Future<Output = F::Output> {
        WORDS.scope(self.clone(), render)
    }
}

pub(crate) fn field_label(identity: &str, default: &str) -> String {
    WORDS
        .try_with(|words| words.0.get(identity).cloned())
        .ok()
        .flatten()
        .unwrap_or_else(|| default.to_owned())
}
