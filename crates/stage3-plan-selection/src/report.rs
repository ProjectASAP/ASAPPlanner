//! What plan selection reports beside the plan: the accepted accuracy, the
//! candidates it pruned and the selection MILP's size and time; and each
//! rejected candidate and why.

use asap_cost::analytical::AnalyticalCostError;
use asap_cost::CostModel;
use asap_types::ir::physical_export::PhysicalASAPNodeId;
use asap_types::ir::properties::AccuracyMetric;
use asap_types::stages::{PhysicalCandidate, PrunedCandidate};
use thiserror::Error;

/// Peak memory above which a candidate's report carries a warning when the
/// deployment declares no memory capacity: 10^12 bytes (1 TB), more than
/// one large machine holds.
pub(crate) const LARGE_PEAK_MEMORY_BYTES: u64 = 1_000_000_000_000;

/// A warning when `peak_memory_bytes` is over `LARGE_PEAK_MEMORY_BYTES`
/// and `cost_model` declares no memory capacity (cost models 1 and 2). Under
/// cost model 3 the cluster's memory is declared and a candidate over it is
/// rejected ([`RejectionCause::OverClusterCapacity`]), so there is none.
pub fn large_peak_memory_warning(peak_memory_bytes: u64, cost_model: &CostModel) -> Option<String> {
    match cost_model {
        CostModel::ClusterCapacity(_) => None,
        CostModel::UsageOverTime(_) | CostModel::ProvisionedCapacity(_) => {
            (peak_memory_bytes > LARGE_PEAK_MEMORY_BYTES).then(|| {
                format!(
                    "peak memory {} bytes is over {LARGE_PEAK_MEMORY_BYTES} bytes and the \
                     deployment declares no memory capacity to check it against",
                    peak_memory_bytes
                )
            })
        }
    }
}

/// A candidate that was not selected: its id, why it lost
/// ([`RejectionCause`]) and a one-line reason with the numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct Rejection {
    pub id: String,
    pub cause: RejectionCause,
    pub reason: String,
}

/// Why Stage 3 did not select a candidate. The first five mean the
/// candidate failed a check and is invalid ([`Self::failed_a_check`]); the
/// rest mean it was valid and lost under the cost model. JSON: the
/// snake_case variant name, e.g. `"costlier"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionCause {
    /// A query's accuracy target is not met: its output's guarantee misses
    /// it and no deployment evidence meets it.
    AccuracyTargetMissed,
    /// A query's output has no accuracy guarantee: no analytical rule and
    /// no deployment evidence supplies one.
    NoAccuracyGuarantee,
    /// A query's latency is over its `max_latency_ms` bound.
    LatencyBoundExceeded,
    /// A repeating query's latency per evaluation is over its repeat
    /// interval, so its evaluations cannot keep up; checked whether or not
    /// it has a latency bound.
    LatencyOverRepeatInterval,
    /// Under cost model 3, its usage is over one of the cluster's limits.
    OverClusterCapacity,
    /// Its cost overflows the cost model's arithmetic, so it cannot be
    /// costed.
    CostNotRepresentable,
    /// Under cost models 1 and 2, its cost is higher than the selected
    /// candidate's.
    Costlier,
    /// Under cost model 3's `lowest-latency`, it is slower than the
    /// selected candidate.
    Slower,
    /// Under cost model 3's `least-memory`, it uses more memory than the
    /// selected candidate.
    MoreMemory,
    /// It ties with the selected candidate and comes later in enumeration
    /// order.
    TiedLostOnEnumerationOrder,
    /// No plan meets every query's accuracy target, latency bound and the
    /// catalog's consumer constraints together: the selection MILP is
    /// infeasible. The reason names a query when it has no plan even alone.
    Infeasible,
}

impl RejectionCause {
    /// Whether the candidate failed a check (accuracy, latency bound,
    /// repeat interval, cluster capacity or a representable cost) rather
    /// than losing under the cost model.
    pub fn failed_a_check(self) -> bool {
        matches!(
            self,
            Self::AccuracyTargetMissed
                | Self::NoAccuracyGuarantee
                | Self::LatencyBoundExceeded
                | Self::LatencyOverRepeatInterval
                | Self::OverClusterCapacity
                | Self::Infeasible
                | Self::CostNotRepresentable
        )
    }
}

/// The accuracy Stage 3 accepted for one query: the metric of the error
/// bound that met its target, and the sketches its output reads.
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptedAccuracy {
    /// Whether the query has an accuracy target.
    pub has_accuracy_target: bool,
    /// The metric of the accepted error bound, that of the sketch readout
    /// the output reads; `None` for an exact output, which meets every
    /// metric, and for a query without an accuracy target.
    pub accepted_metric: Option<AccuracyMetric>,
    /// The sketch algorithms the query's output reads, in plan order
    /// (e.g. `["Kll"]`); empty for an exact plan.
    pub sketches: Vec<String>,
}

impl std::fmt::Display for AcceptedAccuracy {
    /// E.g. `accepted rank (Kll)`, `accepted exact`, or `no accuracy
    /// target (DDSketch)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.accepted_metric {
            None if !self.has_accuracy_target => write!(f, "no accuracy target")?,
            None => write!(f, "accepted exact")?,
            Some(metric) => {
                let name = serde_json::to_value(metric)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{metric:?}"));
                write!(f, "accepted {name}")?
            }
        }
        if !self.sketches.is_empty() {
            write!(f, " ({})", self.sketches.join(", "))?;
        }
        Ok(())
    }
}

/// What Stage 3 reports about its own run, beside the plan
/// ([`select_plan`](crate::select_plan)). The plan's cost is
/// `asap_cost::plan_cost` of the plan; this does not repeat it.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    /// Per query, the accuracy Stage 3 accepted for the selected plan.
    pub accepted_accuracy_per_query: Vec<AcceptedAccuracy>,
    /// The candidates Stage 3 ruled out before the selection MILP:
    /// dominated ones and those whose usage could not be costed.
    pub pruned_candidates: Vec<PrunedCandidate<PhysicalCandidate>>,
    /// The selection MILP's size.
    pub milp_variables: usize,
    pub milp_constraints: usize,
    /// How long building and solving the selection MILP took.
    pub milp_construction_ms: u64,
    pub milp_solve_ms: u64,
}

/// Why Stage 3 selected no plan.
#[derive(Debug, Error)]
pub enum ErrorSelectingPlan {
    /// Bad input: the deployment's cost inputs
    /// ([`CostInputs::validate`](asap_cost::CostInputs::validate)).
    #[error("cost inputs: {0}")]
    InvalidSettings(AnalyticalCostError),
    /// Bad input: the caller gave a different number of workload entries
    /// than the candidate has query roots.
    #[error("candidate {candidate} has {roots} roots but {entries} workload entries were given")]
    EntryCount {
        candidate: String,
        roots: usize,
        entries: usize,
    },
    #[error("candidate {candidate}, node {node:?}: {error}")]
    Cost {
        candidate: String,
        node: PhysicalASAPNodeId,
        error: AnalyticalCostError,
    },
    #[error("no valid candidate: {0:?}")]
    NoValidCandidate(Vec<Rejection>),
    /// The memo search's MILP solver failed.
    #[error("MILP solver: {0}")]
    Solver(String),
    /// The selection MILP implements cost model 1 only (#1095 §3).
    #[error("the selection MILP does not support {0}")]
    UnsupportedCostModel(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_cost::{ClusterLimits, ClusterObjective, ResourceUnitCost};

    /// Without a declared memory capacity nothing rejects a candidate for
    /// its memory, so a peak over `LARGE_PEAK_MEMORY_BYTES` is reported
    /// as a warning (the eval's 24 h quantiles plan peaked at 1.43 TB).
    /// Under cost model 3 the cluster's memory is declared and its capacity
    /// check applies instead, so there is no warning.
    #[test]
    fn a_very_large_peak_memory_is_warned_about_only_without_a_declared_capacity() {
        let usage_over_time = CostModel::UsageOverTime(ResourceUnitCost::illustrative());
        assert_eq!(
            large_peak_memory_warning(1_430_000_000_000, &usage_over_time).as_deref(),
            Some(
                "peak memory 1430000000000 bytes is over 1000000000000 bytes and the deployment \
                 declares no memory capacity to check it against"
            )
        );
        assert_eq!(
            large_peak_memory_warning(LARGE_PEAK_MEMORY_BYTES, &usage_over_time),
            None
        );
        let cluster = CostModel::ClusterCapacity(ClusterLimits {
            memory_bytes: u64::MAX,
            disk_bytes: u64::MAX,
            cpu_cores: None,
            objective: ClusterObjective::LowestLatency,
        });
        assert_eq!(large_peak_memory_warning(1_430_000_000_000, &cluster), None);
    }
}
