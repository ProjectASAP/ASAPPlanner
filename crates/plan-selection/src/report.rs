//! What plan selection reports: the selection, each rejected candidate and why,
//! and the search report.

use std::collections::BTreeMap;

use asap_cost::analytical::AnalyticalCostError;
use asap_cost::{CandidateCost, CostModel};
use asap_logical_asap_optimization::deployment_capabilities::DeploymentCapabilitiesError;
use asap_types::ir::physical_export::PhysicalASAPNodeId;
use asap_types::ir::properties::AccuracyMetric;
use asap_types::ir::QueryRoot;
use thiserror::Error;

/// Peak memory above which a candidate's report carries a warning when the
/// deployment declares no memory capacity: 10^12 bytes (1 TB), more than
/// one large machine holds.
pub const LARGE_PEAK_MEMORY_BYTES: u64 = 1_000_000_000_000;

/// A warning when `peak_memory_bytes` is over [`LARGE_PEAK_MEMORY_BYTES`]
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

/// Why Stage 3 did not select a candidate. The first ten mean the
/// candidate failed a check and is invalid ([`Self::failed_a_check`]); the
/// rest mean it was valid and lost under the cost model. JSON: the
/// snake_case variant name, e.g. `"costlier"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionCause {
    /// Its summaries, resized for the queries' failure probabilities, do
    /// not rebuild into a plan.
    FailureProbabilityResizeFailed,
    /// A query's accuracy target is not met: its output's guarantee misses
    /// it and no deployment evidence meets it.
    AccuracyTargetMissed,
    /// A query's plan reads a Count-Min sketch whose update weights are not
    /// proven non-negative (e.g. samples of a metric declared neither a
    /// counter nor non-negative). The executor cannot run it, so the deployment's accuracy
    /// model cannot accept it either.
    CountMinWeightsNotProvenNonNegative,
    /// It reduces each series' raw data samples with an exact aggregate the
    /// executor has no kernel for (e.g. `stddev_over_time`), so the
    /// executor cannot compile it
    /// ([`asap_types::ir::operator::agg_has_exact_per_entity_kernel`]).
    PerEntityAggregateNotExecutable,
    /// A query's output has no accuracy guarantee (Stage 1 has no
    /// analytical rule for it and the deployment's evidence supplies none),
    /// and the deployment's accuracy model did not accept it directly
    /// ([`AccuracyModel::accepts_output_regardless_of_guarantee`]).
    NoAccuracyGuarantee,
    /// It scans raw data at query time in a deployment that does not
    /// retain raw data.
    RawDataNotRetained,
    /// A query's latency is over its `max_latency_ms` bound.
    LatencyBoundExceeded,
    /// A repeating query's latency per evaluation is over its repeat
    /// interval, so its evaluations cannot keep up; checked whether or not
    /// it has a latency bound.
    LatencyOverRepeatInterval,
    /// Under cost model 3, its usage is over one of the cluster's limits.
    OverClusterCapacity,
    /// Under cost models 1 and 2, its price is higher than the selected
    /// candidate's.
    Costlier,
    /// Under cost model 3's `lowest-latency`, it is slower than the
    /// selected candidate.
    Slower,
    /// Under cost model 3's `least-memory`, it uses more memory than the
    /// selected candidate.
    MoreMemory,
    /// Under cost model 3's `pareto-front`, it is on the Pareto front but
    /// not its lowest-latency member, which is selected.
    NotLowestLatencyOnParetoFront,
    /// Under cost model 3's `pareto-front`, another candidate dominates it.
    Dominated,
    /// It ties with the selected candidate and comes later in enumeration
    /// order.
    TiedLostOnEnumerationOrder,
}

impl RejectionCause {
    /// Whether the candidate failed a check (executability, accuracy, raw
    /// data, latency bound, repeat interval or cluster capacity) rather than losing under the
    /// cost model.
    pub fn failed_a_check(self) -> bool {
        matches!(
            self,
            Self::FailureProbabilityResizeFailed
                | Self::AccuracyTargetMissed
                | Self::CountMinWeightsNotProvenNonNegative
                | Self::PerEntityAggregateNotExecutable
                | Self::NoAccuracyGuarantee
                | Self::RawDataNotRetained
                | Self::LatencyBoundExceeded
                | Self::LatencyOverRepeatInterval
                | Self::OverClusterCapacity
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
    /// ([`AccuracyModel::accepts_output_regardless_of_guarantee`]), as
    /// every output is with `ignore_accuracy_guarantees`; `false` when its
    /// guarantee was checked and met the target.
    pub accepted_regardless_of_guarantee: bool,
    /// When accepted regardless of its guarantee, the cause the guarantee
    /// check would have rejected it with
    /// ([`RejectionCause::NoAccuracyGuarantee`] or
    /// [`RejectionCause::AccuracyTargetMissed`]): the output was accepted
    /// only because of the accuracy model (e.g.
    /// [`CostInputs::ignore_accuracy_guarantees`]). `None` when
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

/// Stage 3's result: the selected candidate and the plan to run, with
/// what Stage 3 kept of the other candidates. Keeping every candidate
/// ([`PlanSelector::new`]'s `keep_every_candidate`, [`select_plan`]),
/// `costs` and `accepted` list every valid candidate and `rejected` every
/// other one; otherwise only the selected candidate and, under
/// [`ClusterObjective::ParetoFront`], the front are kept, and `rejected`
/// holds the first rejection of each cause.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub selected: String,
    /// The selected candidate's query roots with every weight requirement
    /// Stage 3 confirmed made a proof ([`with_confirmed_weights`]): the plan
    /// to run. The Stage 2 candidate keeps the open requirement.
    pub plan: Vec<QueryRoot>,
    pub costs: BTreeMap<String, CandidateCost>,
    /// For each valid candidate, per query, the accuracy Stage 3 accepted.
    pub accepted: BTreeMap<String, Vec<AcceptedAccuracy>>,
    pub rejected: Vec<Rejection>,
    /// Under [`ClusterObjective::ParetoFront`], the valid candidates no
    /// other beats on peak memory, disk and worst-query latency at once, in
    /// candidate order; empty under every other objective and cost model.
    pub front: Vec<String>,
    /// For each tumbling-pane chain of the selected candidate kept beyond
    /// one evaluation (maintained at ingestion time, or built at query time
    /// and kept across evaluations), by node id in its physical DAG: how
    /// far back from the newest evaluation the runtime must keep its
    /// completed panes, in milliseconds. It is how far its panes reach
    /// back: the offset its newest pane ends before the evaluation (`[60m]
    /// offset 10m`: 10 min), plus the chain's window, plus the longest
    /// range-evaluation span of its readers, so the earliest step of the
    /// next request still finds its panes; without an offset or range
    /// evaluations, just the window. Empty when the plan keeps no panes.
    pub pane_retention_ms: BTreeMap<PhysicalASAPNodeId, u64>,
    /// What the search behind this selection checked and discarded.
    pub search: SearchReport,
}

/// What the search behind a selection checked and discarded, for the
/// selection report.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SearchReport {
    /// The physical candidates Stage 3 checked, valid or not.
    pub candidates_checked: u64,
    /// Under cost models 1 and 2, the cost per second of the cheapest
    /// valid candidate priced more than rounding (1e-12 of it) above the
    /// cheapest: how close the selection came to a tie. Candidates within
    /// rounding of the cheapest cost the same. `None` when every valid
    /// candidate costs the same, under cost model 3, or for a combination
    /// of components.
    pub runner_up_cost_per_second: Option<f64>,
    /// The selected plan's cost per second (Stage 3's exact cost); `None`
    /// when no plan was selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incumbent_cost: Option<f64>,
    /// Joint selection metrics. Optimality is limited to the generated finite
    /// catalog and configured cost and constraint models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_connections: Option<CandidateConnectionReport>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CandidateConnectionReport {
    /// Pane-width domain of the most recently reported complete inventory.
    /// Inventory counts form one snapshot from this domain, even if a later
    /// expansion timed out. This does not certify full-domain optimality.
    #[serde(default)]
    pub inventory_pane_width_domain: Option<String>,
    /// Width domains attempted in order; state/physical catalogs are rebuilt per round.
    #[serde(default)]
    pub pane_width_rounds: Vec<String>,
    /// Some legal widths remain unsearched; restricted optima are not global certificates.
    #[serde(default)]
    pub pane_widths_pending: bool,
    /// Number of distinct allocated compatible-supplier lists.
    #[serde(default)]
    pub supplier_sets: usize,
    /// Supplier IDs physically stored across shared lists (not logical edges).
    #[serde(default)]
    pub stored_supplier_ids: usize,
    /// Conservative zero bounds used to stay within the solver coefficient range.
    #[serde(default)]
    pub rounded_down_cost_bounds: usize,
    /// Time spent obtaining a validated incumbent from the seed width domain.
    #[serde(default)]
    pub seed_planning_ms: f64,
    /// Post-seed expansion/selection wall time, including interrupted rounds.
    #[serde(default)]
    pub full_planning_ms: f64,
    #[serde(default)]
    pub seed_selected: bool,
    /// Full-width generation completed; distinct from exhaustive assignment traversal.
    #[serde(default)]
    pub full_catalog_completed: bool,
    #[serde(default)]
    pub seed_fallback_reason: Option<String>,
    #[serde(default)]
    pub independent_components: usize,
    #[serde(default)]
    pub catalog_exhausted: Option<bool>,
    #[serde(default)]
    pub evaluation_complete: Option<bool>,
    #[serde(default)]
    pub structurally_removed_candidates: usize,
    #[serde(default)]
    pub structurally_removed_connections: usize,
    pub catalog_scope: String,
    pub status: String,
    pub groups: usize,
    pub logical_candidates: usize,
    pub immediate_candidates: usize,
    pub physical_configurations: usize,
    pub compatible_connections: usize,
    pub output_references: usize,
    pub refinements: usize,
    pub max_refinements: usize,
    pub time_limit_ms: u64,
    pub lower_bound: Option<f64>,
    #[serde(default)]
    pub objective: String,
    #[serde(default)]
    pub calibration_scope: String,
    #[serde(default)]
    pub calibration_factor: f64,
    pub relative_gap: Option<f64>,
    /// Memo population and concrete state sharing only; excludes frontend
    /// lowering, initial memo identity, and Pass 1 local rule generation.
    pub memo_expansion_ms: f64,
    /// Initial memo construction and Pass 1 generation; absent when supplied by the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_candidate_generation_ms: Option<f64>,
    pub inventory_construction_ms: f64,
    pub search_and_validation_ms: f64,
    pub model_construction_ms: f64,
    pub solver_ms: f64,
    pub validation_ms: f64,
    pub solve_calls: usize,
    /// Restricted query-time proposals priced completely inside the MILP.
    #[serde(default)]
    pub direct_cost_solve_calls: usize,
    /// Direct solves jointly selecting supplier-dependent pricing options.
    #[serde(default)]
    pub joint_price_solve_calls: usize,
    /// Feasible direct proposals whose objective agrees with the evaluator.
    #[serde(default)]
    pub direct_cost_validated_plans: usize,
    #[serde(default)]
    pub direct_cost_best_validated_cost: Option<f64>,
    /// Direct-cost contexts rejected by numerical or modeling support checks.
    #[serde(default)]
    pub direct_cost_fallback_reasons: Vec<String>,
    pub peak_milp_variables: usize,
    pub peak_milp_constraints: usize,
    #[serde(default)]
    pub validation_rejection_counts: BTreeMap<String, usize>,
    /// Proven query-local failures excluded across unrelated workload choices.
    #[serde(default)]
    pub certified_query_cuts: usize,
    #[serde(default)]
    pub early_query_pruning_ms: f64,
    #[serde(default)]
    pub early_query_pruning_counts: BTreeMap<String, usize>,
    #[serde(default)]
    pub early_query_pruning_examples: Vec<String>,
    /// Up to eight representative failures, with bounded reason lengths.
    #[serde(default)]
    pub validation_rejection_examples: Vec<String>,
}

#[derive(Debug, Error)]
pub enum SelectionError {
    /// Evaluation stopped without proving feasibility or infeasibility.
    #[error("candidate evaluation deadline exceeded")]
    EvaluationDeadlineExceeded,
    /// Bad input: the deployment's plan selection settings
    /// ([`CostInputs::validate`]).
    #[error("plan selection settings: {0}")]
    InvalidSettings(AnalyticalCostError),
    /// Bad input: the caller gave a different number of workload entries
    /// than the candidate has query roots.
    #[error("candidate {candidate} has {roots} roots but {entries} workload entries were given")]
    EntryCount {
        candidate: String,
        roots: usize,
        entries: usize,
    },
    /// Bad input: the deployment's capabilities contradict the data
    /// workload ([`asap_logical_asap_optimization::DeploymentCapabilities::validate`]).
    #[error("deployment capabilities: {0}")]
    InvalidDeploymentCapabilities(DeploymentCapabilitiesError),
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
}
