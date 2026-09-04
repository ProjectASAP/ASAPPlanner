use serde::{Deserialize, Serialize};

/// Accuracy requirement that a query result must satisfy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum AccuracyTarget {
    /// Additive error bound ε: |estimate − true| ≤ ε · (domain size).
    Epsilon(f64),
    /// Probabilistic (ε, δ) guarantee: error ≤ ε with probability ≥ 1 − δ.
    EpsilonDelta { epsilon: f64, delta: f64 },
    /// No approximation permitted; result must be exact.
    Exact,
}
