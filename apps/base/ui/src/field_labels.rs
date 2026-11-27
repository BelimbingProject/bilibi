use std::{collections::BTreeMap, future::Future};

tokio::task_local! {
    static WORDS: FieldLabels;
}

#[derive(Clone, Debug, Default)]
pub struct FieldLabels(pub BTreeMap<String, String>);

impl FieldLabels {
    pub async fn during<T>(&self, render: impl Future<Output = T>) -> T {
        WORDS.scope(self.clone(), render).await
    }
}

pub(crate) fn field_label(identity: &str, default: &str) -> String {
    WORDS
        .try_with(|words| words.0.get(identity).cloned())
        .ok()
        .flatten()
        .unwrap_or_else(|| default.to_owned())
}
