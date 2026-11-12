//! The statistics read from a summary state.

use serde::{Deserialize, Serialize};

use crate::ir::operator::AggIntent;
use crate::ir::scalar::ColumnRef;

/// The statistic computed from summary state by `SummaryEstimate`, for example
/// `Quantile { q: 0.99 }`. This is a result operation, not a workload query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SketchStatistic {
    /// sqrt(sum_v frequency(v)^2), not the norm of numeric input values.
    FrequencyL2,
    /// Shannon entropy of the value-frequency distribution, in bits.
    FrequencyEntropy,
    /// Extract the value at quantile rank `q` ∈ (0, 1].
    Quantile { q: f64 },
    /// Estimated count / frequency. `key` names which column is being
    /// queried (`ColumnRef::SampleValue` for the bare bucket total, with
    /// `value: None`); a `Named`/`Qualified` `key` paired with
    /// `value: Some(v)` is a per-item point lookup (e.g.
    /// `count(cms_metric{item="checkout"})` — `key` is `item`, `value` is
    /// `"checkout"`). `value` is carried here rather than resolved by the
    /// `SummaryExecutor` from a `Filter` predicate because a summary
    /// evaluation is read without access to the rest of the DAG.
    PointCount {
        key: ColumnRef,
        value: Option<String>,
    },
    /// Estimated number of distinct elements.
    Cardinality,
    /// Read the top-k entries from a heap-backed summary.
    TopK { k: usize },
}

impl SketchStatistic {
    /// The statistic a summary is read for to answer `intent`, or `None`
    /// when no sketch statistic serves the intent.
    pub fn for_intent(intent: &AggIntent) -> Option<Self> {
        Some(match intent {
            AggIntent::Quantile { q, .. } => Self::Quantile { q: *q },
            AggIntent::Cardinality { .. } => Self::Cardinality,
            AggIntent::FrequencyL2 { .. } => Self::FrequencyL2,
            AggIntent::FrequencyEntropy { .. } => Self::FrequencyEntropy,
            AggIntent::TopK { k, .. } => Self::TopK { k: *k },
            AggIntent::Count => Self::PointCount {
                key: ColumnRef::SampleValue,
                value: None,
            },
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests;
