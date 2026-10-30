//! How a cost model orders valid plans ([`PlanRanking`]): Stage 3 ranks its
//! candidates with it, and the facade picks the best of its pane-width
//! rounds' selections with it (#1047).
use std::cmp::Ordering;

use crate::{ClusterObjective, CostModel, PlanCost, QueryLatency, ResourceUsage};

/// How a cost model orders valid plans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanRanking {
    /// Cost models 1 and 2: by cost per second.
    Price,
    /// Latency, then peak memory, then disk.
    Latency,
    /// Peak memory, then disk, then latency.
    Memory,
}

impl PlanRanking {
    pub fn of(model: &CostModel) -> Self {
        match model {
            CostModel::UsageOverTime(_) | CostModel::ProvisionedCapacity(_) => Self::Price,
            CostModel::ClusterCapacity(limits) => match limits.objective {
                ClusterObjective::LowestLatency => Self::Latency,
                ClusterObjective::LeastMemory => Self::Memory,
            },
        }
    }

    /// Whether `cost` ranks strictly before `best`. Costs and latencies tie
    /// within rounding ([`cheaper`]); peak memory and disk are whole bytes,
    /// compared exactly.
    pub fn better(self, cost: &PlanCost, best: &PlanCost) -> bool {
        let rounded = |a: f64, b: f64| match (cheaper(a, b), cheaper(b, a)) {
            (true, _) => Ordering::Less,
            (_, true) => Ordering::Greater,
            _ => Ordering::Equal,
        };
        let (a, b) = (&cost.usage, &best.usage);
        let latency = || rounded(worst_latency_ms(a), worst_latency_ms(b));
        let memory = || a.peak_memory_bytes.cmp(&b.peak_memory_bytes);
        let disk = || a.disk_bytes.cmp(&b.disk_bytes);
        let order = match self {
            Self::Price => rounded(
                cost.cost_per_second.unwrap_or(f64::INFINITY),
                best.cost_per_second.unwrap_or(f64::INFINITY),
            ),
            Self::Latency => latency().then_with(memory).then_with(disk),
            Self::Memory => memory().then_with(disk).then_with(latency),
        };
        order == Ordering::Less
    }
}

/// Whether `cost` is cheaper than `best`. Totals that differ only by
/// floating-point rounding (summing the same charges in another grouping)
/// tie, and a tie keeps the earlier plan.
pub fn cheaper(cost: f64, best: f64) -> bool {
    cost < best - best.abs() * 1e-9
}

/// The worst latency per evaluation among a plan's queries (0 with none).
pub fn worst_latency_ms(usage: &ResourceUsage) -> f64 {
    usage
        .latency_per_query
        .iter()
        .map(QueryLatency::total_ms)
        .fold(0.0, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Totals that differ only by rounding tie, so the earlier plan is kept;
    /// a real difference is cheaper.
    #[test]
    fn totals_differing_by_rounding_tie() {
        // One unit in the last place apart, as the same charges summed in
        // another grouping can be.
        let separate = 0.336006_f64;
        let together = f64::from_bits(separate.to_bits() - 1);
        assert!(!cheaper(separate, together));
        assert!(!cheaper(together, separate));
        assert!(cheaper(0.3, 0.4));
        assert!(!cheaper(0.4, 0.3));
    }
}
