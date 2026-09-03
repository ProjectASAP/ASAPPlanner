//! Planner-level summary-window primitives.
//!
//! These values identify the abstract window framework selected during
//! candidate search. They do not identify a runtime library, process,
//! placement, shard layout, storage backend, or deployment instance; those
//! choices belong to downstream physical compilation.

use serde::{Deserialize, Serialize};

/// Abstract framework used to organize incrementally maintained summary
/// state over time.
///
/// The built-in variants name semantics that the planner can compare across
/// implementations. [`Self::Extension`] lets a provider introduce a new
/// primitive without treating an opaque physical deployment ID as planner IR.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryWindowFramework {
    /// Disjoint, fixed-width windows.
    Tumbling,
    /// Overlapping logical windows, commonly realized from reusable panes.
    Sliding,
    /// Hierarchical buckets with exponentially increasing coverage.
    ExponentialHistogram,
    /// A named planner primitive whose semantics are registered by a provider.
    Extension(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_and_extension_frameworks_round_trip() {
        for framework in [
            SummaryWindowFramework::Tumbling,
            SummaryWindowFramework::Sliding,
            SummaryWindowFramework::ExponentialHistogram,
            SummaryWindowFramework::Extension("learned_window".into()),
        ] {
            let encoded = serde_json::to_string(&framework).unwrap();
            assert_eq!(
                serde_json::from_str::<SummaryWindowFramework>(&encoded).unwrap(),
                framework
            );
        }
    }
}
