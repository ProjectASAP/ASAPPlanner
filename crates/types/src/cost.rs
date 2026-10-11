//! The unit a cost is expressed in. Values with different [`CostUnit`]s are
//! never combined.

use serde::{Deserialize, Serialize};

/// The unit a cost value is expressed in.
/// Producers and consumers must not compare or aggregate different units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CostUnit {
    /// A recurring cost expressed per second (`cost_units / s`) — the
    /// recurring costs. It requires an explicit recurrence interval/rate.
    CostUnitsPerSecond,
    /// A finite-run or one-shot total at some horizon `H` — `total_cost(H)`
    /// in the issue's formulas, or a standalone one-shot addend. Never
    /// aggregated with [`CostUnitsPerSecond`](CostUnit::CostUnitsPerSecond)
    /// except through `total_cost`, which keeps the two terms explicit
    /// rather than silently adding a rate to a total.
    CostUnits,
}

impl CostUnit {
    /// Stable machine-readable name for cost exports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CostUnitsPerSecond => "cost_units_per_second",
            Self::CostUnits => "cost_units",
        }
    }
}

impl std::fmt::Display for CostUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CostUnit::CostUnitsPerSecond => write!(f, "cost units/s"),
            CostUnit::CostUnits => write!(f, "cost units"),
        }
    }
}
