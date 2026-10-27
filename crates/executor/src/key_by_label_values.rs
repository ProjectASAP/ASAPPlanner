use serde::{Deserialize, Serialize};

/// The label values naming one population of a keyed exact state. Public
/// entry points take these values as `&[String]`; this wrapper only keeps the
/// persisted shape of keyed exact states (`{ labels: [...] }`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct KeyByLabelValues {
    pub(crate) labels: Vec<String>,
}

impl KeyByLabelValues {
    pub(crate) fn new_with_labels(labels: Vec<String>) -> Self {
        Self { labels }
    }
}
