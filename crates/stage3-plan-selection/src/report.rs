//! What plan selection reports: the selection, each rejected candidate and why,
//! and the search report.

use std::collections::BTreeMap;

use asap_cost::analytical::AnalyticalCostError;
use asap_cost::{CostModel, PlanCost};
use asap_types::ir::physical_export::PhysicalASAPNodeId;
use asap_types::ir::properties::AccuracyMetric;
use asap_types::stages::{DagOfSelectedCandidates, PhysicalCandidateId, PrunedCandidate};
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

/// Why Stage 3 did not select a candidate. The first six mean the
/// candidate failed a check and is invalid ([`Self::failed_a_check`]); the
/// rest mean it was valid and lost under the cost model. JSON: the
/// snake_case variant name, e.g. `"costlier"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionCause {
    /// A query's accuracy target is not met: its output's guarantee misses
    /// it and no deployment evidence meets it.
    AccuracyTargetMissed,
    /// A query's plan reads a Count-Min sketch whose update weights are not
    /// proven non-negative (e.g. samples of a metric declared neither a
    /// counter nor non-negative). The executor cannot run it, so the deployment's accuracy
    /// model cannot accept it either.
    CountMinWeightsNotProvenNonNegative,
    /// A query's output has no accuracy guarantee (Stage 1 has no
    /// analytical rule for it and the deployment's evidence supplies none),
    /// and the deployment's accuracy model did not accept it directly
    /// ([`AccuracyModel::accepts_output_regardless_of_guarantee`](asap_accuracy::AccuracyModel::accepts_output_regardless_of_guarantee)).
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
    /// priced.
    CostNotRepresentable,
    /// Under cost models 1 and 2, its price is higher than the selected
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
                | Self::CountMinWeightsNotProvenNonNegative
                | Self::NoAccuracyGuarantee
                | Self::LatencyBoundExceeded
                | Self::LatencyOverRepeatInterval
                | Self::OverClusterCapacity
                | Self::Infeasible
                | Self::CostNotRepresentable
        )
    }
}

/// The accuracy Stage 3 accepted for one query of a valid candidate: the
/// metric the query's target set (if any) and the metric of the guarantee
/// that met it. With no metric set, any metric meeting the target's ε, δ
/// and scope is accepted, so ε means different things across candidates
/// (e.g. 0.01 of the true value vs 0.01 of the rank); this record makes
/// the accepted metric visible.
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptedAccuracy {
    /// The target's metric; `None` when the user did not set one.
    pub target_metric: Option<AccuracyMetric>,
    /// The metric the accepted guarantee is stated in; `None` for an exact
    /// output, which meets every metric, and for one the deployment's model
    /// accepted regardless of its guarantee (no guarantee was checked).
    pub accepted_metric: Option<AccuracyMetric>,
    /// Whether the deployment's accuracy model accepted the output directly,
    /// whatever its guarantee says
    /// ([`AccuracyModel::accepts_output_regardless_of_guarantee`](asap_accuracy::AccuracyModel::accepts_output_regardless_of_guarantee)), as
    /// every output is with [`IgnoreAccuracyGuarantees`](asap_accuracy::IgnoreAccuracyGuarantees); `false` when its
    /// guarantee was checked and met the target.
    pub accepted_regardless_of_guarantee: bool,
    /// When accepted regardless of its guarantee, the cause the guarantee
    /// check would have rejected it with
    /// ([`RejectionCause::NoAccuracyGuarantee`] or
    /// [`RejectionCause::AccuracyTargetMissed`]): the output was accepted
    /// only because of the accuracy model (e.g.
    /// [`IgnoreAccuracyGuarantees`](asap_accuracy::IgnoreAccuracyGuarantees)). `None` when
    /// its guarantee meets the target.
    pub guarantee_check_rejection: Option<RejectionCause>,
    /// The sketch algorithms the query's output reads, in plan order
    /// (e.g. `["Kll"]`); empty for an exact plan.
    pub sketches: Vec<String>,
}

impl std::fmt::Display for AcceptedAccuracy {
    /// E.g. `metric not set; accepted rank (Kll)`, or
    /// `metric relative; accepted relative (DDSketch)`, or
    /// `metric rank; accepted by the deployment's accuracy model regardless of guarantee (Kll)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = |metric: AccuracyMetric| {
            serde_json::to_value(metric)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| format!("{metric:?}"))
        };
        match self.target_metric {
            None => write!(f, "metric not set; ")?,
            Some(metric) => write!(f, "metric {}; ", name(metric))?,
        }
        match self.accepted_metric {
            _ if self.accepted_regardless_of_guarantee => {
                write!(
                    f,
                    "accepted by the deployment's accuracy model regardless of guarantee"
                )?;
                if let Some(cause) = self.guarantee_check_rejection {
                    write!(f, ", ignoring a failed guarantee check ({cause:?})")?;
                }
            }
            None => write!(f, "accepted exact")?,
            Some(metric) => write!(f, "accepted {}", name(metric))?,
        }
        if !self.sketches.is_empty() {
            write!(f, " ({})", self.sketches.join(", "))?;
        }
        Ok(())
    }
}

/// Stage 3's result ([`select_plan`](crate::select_plan)): the selected DAG
/// over the [`PhysicalASAPMemoCandidates`](asap_types::stages::PhysicalASAPMemoCandidates)
/// it was given, its cost and accepted accuracy, and the search report.
/// The facade builds the plan from `selected_dag` with the same
/// [`AssignmentRealizer`](asap_types::stages::AssignmentRealizer).
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    /// The selected plan: one node per run of a candidate.
    pub selected_dag: DagOfSelectedCandidates,
    pub cost: PlanCost,
    /// Per query, the accuracy Stage 3 accepted for the selected plan.
    pub accepted_accuracy_per_query: Vec<AcceptedAccuracy>,
    /// The candidates Stage 3 ruled out before the search: those whose usage
    /// could not be costed.
    pub pruned_candidates: Vec<PrunedCandidate<PhysicalCandidateId>>,
    pub search_report: SearchReport,
}

/// How the search behind a selection ended, and what it cost.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SearchReport {
    pub status: SearchStatus,
    /// The solver's lower bound on the selected plan's cost per second;
    /// `None` when the solver's dual bound is not finite.
    pub cost_lower_bound_per_second: Option<f64>,
    /// The largest MILP the search built.
    pub milp_variables: usize,
    pub milp_constraints: usize,
    pub model_construction_ms: u64,
    pub solver_ms: u64,
    pub validation_ms: u64,
    /// Set when `asap_cost::plan_cost` of the selected plan differs from
    /// the selection MILP's objective (by more than a relative 10⁻⁶), with
    /// the terms of `plan_cost` the formulation's objective does not encode
    /// that the selection may meet (e.g. the memory a query-time result
    /// holds while its evaluation runs, which needs query latency); the
    /// plan is returned with its cost equality unproven. `None` when the
    /// two are equal. A difference with no such term is an error
    /// ([`ErrorSelectingPlan::ObjectiveDiffersFromPlanCost`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inexact_objective: Option<InexactObjective>,
}

/// The selected plan's cost per second as the selection MILP's objective
/// and as `asap_cost::plan_cost` computes it (both with the runtime's
/// request fees), and the terms of `plan_cost` the objective does not
/// encode that the selection may meet, each with how many selected
/// executions it may concern.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InexactObjective {
    pub objective_per_second: f64,
    pub plan_cost_per_second: f64,
    pub inexact_terms: BTreeMap<String, usize>,
}

impl SearchReport {
    /// How far the lower bound is below `cost_per_second`, as a fraction
    /// of it in [0, 1]; `None` without a bound.
    pub fn relative_gap(&self, cost_per_second: f64) -> Option<f64> {
        self.cost_lower_bound_per_second.map(|bound| {
            if cost_per_second == 0.0 {
                0.0
            } else {
                ((cost_per_second - bound).max(0.0) / cost_per_second.abs()).min(1.0)
            }
        })
    }
}

/// How the search ended with a validated plan. JSON: snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchStatus {
    /// The search proved the plan optimal over the catalog it was given
    /// (the only outcome: the solve has no time limit).
    Optimal,
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
    /// Stage 2 rejected a plan Stage 3 decoded from its catalog: a Stage 2
    /// catalog bug.
    #[error("the realizer rejected the selected plan: {0}")]
    RealizerRejected(String),
    /// The memo search's MILP solver failed.
    #[error("MILP solver: {0}")]
    Solver(String),
    /// The selection MILP implements cost model 1 only (#1095 §3).
    #[error("the selection MILP does not support {0}")]
    UnsupportedCostModel(String),
    /// `asap_cost::plan_cost` of the selected plan differs from the
    /// selection MILP's objective and the selection meets none of the terms
    /// the formulation's objective leaves out
    /// ([`InexactObjective::inexact_terms`]): a bug in a cost coefficient.
    #[error(
        "the selection MILP's objective {objective_per_second:e} differs from plan_cost \
         {plan_cost_per_second:e} (per second) with no term the objective leaves out"
    )]
    ObjectiveDiffersFromPlanCost {
        objective_per_second: f64,
        plan_cost_per_second: f64,
    },
    /// The selected plan fails a check the selection MILP encodes
    /// (accuracy through the budget shares, latency): a bug in the
    /// accuracy weights, the sizing rule or a latency term, never a reason
    /// to re-solve.
    #[error("the selected plan fails a check the MILP encodes ({cause:?}): {reason}")]
    SelectedPlanFailedCheck {
        cause: RejectionCause,
        reason: String,
    },
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
