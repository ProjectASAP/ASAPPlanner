//! What plan selection reports beside the plan: the accepted accuracy, the
//! candidates it pruned and the selection MILP's size and time; and each
//! rejected candidate and why.

use std::collections::{BTreeSet, HashMap};

use asap_cost::node::analytical::AnalyticalCostError;
use asap_cost::{summary_type_name, CostModel};
use asap_types::accuracy::AccuracyMetric;
use asap_types::deployment_capabilities::ExclusionSource;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPDAG, PhysicalASAPDAGNode, PhysicalASAPNodeId,
};
use asap_types::stages::{
    PhysicalASAPMemoCandidates, PhysicalCandidate, PruneReason, PrunedCandidate,
};
use thiserror::Error;

use crate::cost::ExecutionCosts;

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
    /// No plan is left without the candidates the deployment excludes
    /// (`DeploymentCapabilities::exclusions`, which Stage 2 pruned as
    /// `ExcludedByDeployment`) beyond the default deployment's own. The
    /// reason names the query whose every candidate was excluded, else says
    /// the selection MILP is infeasible; then each such exclusion source
    /// that pruned a candidate, by its JSON
    /// name (`excluded_candidates[i]` for the caller's own), with how many
    /// it pruned, e.g. `counter_not_extrapolated (7 candidates)`; and that
    /// the workload may be infeasible without these exclusions too (telling
    /// would take a second solve); then, as for `Infeasible`, why executions
    /// could not be costed. Not a failed check: the excluded
    /// candidates were valid.
    ExcludedByDeployment,
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
    /// The metric of the accepted error bound, that of the first sketch
    /// readout the output reads (in plan order; with a target naming no
    /// metric, readouts in several metrics may each be within it, as (K)
    /// bounds each metric apart, and this is the first's); `None` for an
    /// exact output, which meets every metric, and for a query without an
    /// accuracy target.
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
/// ([`select_plan`](crate::select_plan)).
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    /// Per query, the accuracy Stage 3 accepted for the selected plan.
    pub accepted_accuracy_per_query: Vec<AcceptedAccuracy>,
    /// The candidates Stage 3 ruled out before the selection MILP:
    /// dominated ones and those whose usage could not be costed.
    pub pruned_candidates: Vec<PrunedCandidate<PhysicalCandidate>>,
    /// The selection MILP's optimum: the selected plan's cost per second
    /// under cost model 1, which `asap_cost::plan_cost` of the plan
    /// reports too.
    pub selected_plan_cost_per_second: f64,
    /// The selection MILP's variables (columns).
    pub milp_variable_count: usize,
    /// The selection MILP's constraint rows.
    pub milp_constraint_count: usize,
    /// How long building the selection MILP took.
    pub milp_construction_ms: u64,
    /// How long HiGHS took to solve the selection MILP, summed over
    /// `milp_solve_count` solves.
    pub milp_solve_ms: u64,
    /// How many times HiGHS solved the MILP: 1, or 2 when the selection at
    /// HiGHS's default feasibility tolerances (1e-7) missed an accuracy
    /// target when checked exactly and was solved again at 1e-10.
    pub milp_solve_count: usize,
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
    /// Query `query`'s accuracy target is below the solver's precision:
    /// the MILP's selection misses it when checked exactly, so no plan is
    /// returned rather than one that may not meet the target. Example: a
    /// failure probability of 1e-12 per output value (δ = 0.01 over 1e10
    /// values), below the solver's feasibility tolerance 1e-10.
    #[error(
        "query {query}: an accuracy target is below the solver's precision (failure probability per output value {failure_probability_per_output_value:?}); the solver's plan misses it, so no plan is returned"
    )]
    AccuracyTargetBelowSolverPrecision {
        query: usize,
        failure_probability_per_output_value: Option<f64>,
    },
    /// The selection MILP implements cost model 1 only (#1095 §3).
    #[error("the selection MILP does not support {0}")]
    UnsupportedCostModel(String),
}

/// The selection MILP is infeasible, with the reasons executions could not
/// be costed.
pub(crate) fn infeasible(costs: &ExecutionCosts) -> ErrorSelectingPlan {
    let reason = format!(
        "no plan gives every query output a candidate whose every input has an admissible \
         producer that delivers on its context (the selection MILP is infeasible){}",
        costing_failures(costs)
    );
    ErrorSelectingPlan::NoValidCandidate(vec![Rejection {
        id: "workload".into(),
        cause: RejectionCause::Infeasible,
        reason,
    }])
}

/// Why executions could not be costed, each distinct reason once, as a
/// suffix of a rejection's reason: "; executions left out because they
/// could not be costed: no cardinality for m"; empty when every execution
/// was costed.
fn costing_failures(costs: &ExecutionCosts) -> String {
    let mut reasons: Vec<&str> = Vec::new();
    for reason in costs
        .per_execution
        .iter()
        .filter_map(|cost| cost.as_ref().err())
    {
        if !reasons.contains(&reason.as_str()) {
            reasons.push(reason);
        }
    }
    if reasons.is_empty() {
        return String::new();
    }
    format!(
        "; executions left out because they could not be costed: {}",
        reasons.join("; ")
    )
}

/// When Stage 2 pruned some candidate as excluded by the deployment
/// (`PruneReason::ExcludedByDeployment`) for a source the default
/// deployment does not have, why no plan is left: one
/// [`RejectionCause::ExcludedByDeployment`] whose reason names `query`, the
/// first query with no candidate left to answer it (else the infeasible
/// selection MILP), then each exclusion source with how many candidates it
/// pruned, in the order Stage 2 first recorded it. E.g. "query 0 has no
/// candidate left to answer it; excluded by the deployment:
/// counter_not_extrapolated (7 candidates); the workload may be infeasible
/// without these exclusions too", followed by why executions could not be
/// costed, as [`infeasible`] reports it ([`costing_failures`]). `None` when no candidate was excluded
/// beyond the default deployment's exclusions: the selection is infeasible
/// on its own ([`infeasible`]). Telling
/// whether the workload has a plan without the exclusions would take a
/// second solve, so the reason says it may not.
pub(crate) fn excluded_by_deployment(
    stage2_pruned_candidates: &[PrunedCandidate<PhysicalCandidate>],
    query: Option<usize>,
    costs: &ExecutionCosts,
) -> Option<ErrorSelectingPlan> {
    let mut pruned_per_source: Vec<(ExclusionSource, usize)> = Vec::new();
    for pruned in stage2_pruned_candidates {
        let PruneReason::ExcludedByDeployment(source) = pruned.reason else {
            continue;
        };
        // The default deployment's own exclusions (query-time results not
        // kept across evaluations) are planning as it always was, so they
        // neither cause this rejection nor appear in it.
        if source.in_default_deployment() {
            continue;
        }
        match pruned_per_source
            .iter_mut()
            .find(|(seen, _)| *seen == source)
        {
            Some((_, count)) => *count += 1,
            None => pruned_per_source.push((source, 1)),
        }
    }
    if pruned_per_source.is_empty() {
        return None;
    }
    let what = match query {
        Some(query) => format!("query {query} has no candidate left to answer it"),
        None => "no plan is left (the selection MILP is infeasible)".to_string(),
    };
    let sources: Vec<String> = pruned_per_source
        .iter()
        .map(|(source, count)| {
            let noun = if *count == 1 {
                "candidate"
            } else {
                "candidates"
            };
            format!("{source} ({count} {noun})")
        })
        .collect();
    Some(ErrorSelectingPlan::NoValidCandidate(vec![Rejection {
        id: "workload".into(),
        cause: RejectionCause::ExcludedByDeployment,
        reason: format!(
            "{what}; excluded by the deployment: {}; the workload may be infeasible without \
             these exclusions too{}",
            sources.join(", "),
            costing_failures(costs)
        ),
    }]))
}

/// Per query, the sketches its output reads in `plan` (each sketch build
/// below its root, in node order) and, when it has an accuracy target and
/// reads a sketch, the metric of the first sketch readout below its root
/// (`CandidateForMemoGroup::required_accuracy`, the requirement (K)
/// checked), whatever metric the target names: a target naming none
/// reports the one it accepted.
pub(crate) fn accepted_accuracy(
    physical: &PhysicalASAPMemoCandidates,
    plan: &PhysicalASAPDAG,
) -> Vec<AcceptedAccuracy> {
    let memo = &physical.logical_candidates.memo;
    let mut operator_roots = plan.roots.iter();
    let mut producers_of: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
    for edge in &plan.edges {
        producers_of
            .entry(edge.consumer)
            .or_default()
            .push(edge.producer);
    }
    memo.queries()
        .iter()
        .enumerate()
        .map(|(query, memo_query)| {
            let starts: Vec<PhysicalASAPNodeId> = match plan
                .scalar_roots
                .iter()
                .find(|(position, _)| *position == query)
            {
                Some((_, expression)) => expression.operator_refs().into_iter().copied().collect(),
                None => vec![*operator_roots.next().expect("one operator root per query")],
            };
            let mut sketches: Vec<String> = Vec::new();
            let mut readout_metrics: Vec<AccuracyMetric> = Vec::new();
            for node in nodes_below(plan, &producers_of, starts) {
                match &node.payload {
                    Operator::ASAP(
                        ASAPOp::SummaryAgg { summary_type, .. }
                        | ASAPOp::SummaryTumblingWindowChain { summary_type, .. },
                    ) if matches!(summary_type, FieldDataType::Sketch(_)) => {
                        let name = summary_type_name(summary_type);
                        if !sketches.contains(&name) {
                            sketches.push(name);
                        }
                    }
                    Operator::ASAP(ASAPOp::SummaryEstimate { .. }) => {
                        // The metric of the error the readout's candidate
                        // was required to meet, the one (K) checked.
                        readout_metrics.extend(node.selected_physical_candidate.and_then(|v| {
                            let id = physical.physical_candidates[v.0].logical_candidate_id;
                            Some(memo.candidate(id).required_accuracy()?.metric)
                        }));
                    }
                    _ => {}
                }
            }
            let has_accuracy_target = memo_query.accuracy_target.is_some();
            AcceptedAccuracy {
                has_accuracy_target,
                accepted_metric: (has_accuracy_target && !sketches.is_empty())
                    .then(|| readout_metrics.first().copied())
                    .flatten(),
                sketches,
            }
        })
        .collect()
}

/// The nodes at or below `starts` in `plan`, in node order.
fn nodes_below<'a>(
    plan: &'a PhysicalASAPDAG,
    producers_of: &HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
    starts: Vec<PhysicalASAPNodeId>,
) -> impl Iterator<Item = &'a PhysicalASAPDAGNode> {
    let mut below = BTreeSet::new();
    let mut pending = starts;
    while let Some(id) = pending.pop() {
        if below.insert(id) {
            pending.extend(producers_of.get(&id).into_iter().flatten().copied());
        }
    }
    plan.nodes
        .iter()
        .filter(move |node| below.contains(&node.id))
}

#[cfg(test)]
mod tests;
