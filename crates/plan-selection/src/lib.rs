//! #509 Stage 3, plan selection (MVP): the only stage that computes cost.
//!
//! Each CandidatePhysicalASAPDAG from Stage 2 is checked against every
//! query's accuracy target: the query's output meets it when its root
//! guarantee does (theoretical: Stage 1 composes it with the built-in
//! analytical rules, and this stage binds the statistics it left symbolic
//! from the data workload and the deployment's operand-domain contracts),
//! or when the deployment's empirical evidence shows this
//! candidate's output does (`AccuracyEvidenceProvider::output_guarantee`,
//! read only here). The deployment's accuracy model may instead accept the
//! output directly, whatever its guarantee says
//! (`AccuracyModel::accepts_output_regardless_of_guarantee`); the settings'
//! `ignore_accuracy_guarantees` (the one built-in way) accepts every
//! output so, and the report records each one the
//! guarantee check would have rejected. Before that, for
//! a query with a target, Count-Min is admitted only over weights proven
//! non-negative, which the executor needs to run it; a query without one
//! constrains nothing.
//! A miss rejects the candidate as invalid, with a reason; only this stage
//! discards valid candidates.
//! A candidate whose evaluation of some query takes longer than that
//! query's latency bound, or than its repeat interval, is rejected as
//! invalid too ("Latency" below).
//! Every valid candidate's resource usage is measured node by node over its
//! physical DAG ([`ResourceUsage`](asap_cost::ResourceUsage)), and the deployment's [`CostModel`](asap_cost::CostModel)
//! chooses among them ("Cost models" below). The rest are reported valid
//! but costlier (or slower, or larger).
//! Candidate-connection MILP search selects and validates workload DAGs
//! without constructing the Cartesian product of local candidates.
//! Dynamic-programming compression is outside the current implementation.
//!
//! ## Cost model: cost per second of wall time
//!
//! A candidate's cost is what running it costs per second ([`COST_UNIT`](asap_cost::COST_UNIT)),
//! so queries with different recurrences add up. A node here is one
//! operator node of the candidate's physical plan DAG (`PhysicalASAPDAG`),
//! not a machine. Each node's price for one
//! computation is multiplied by how often it is computed per second
//! ([`NodeCost::cost_per_second`](asap_cost::NodeCost::cost_per_second)):
//!
//! * A `NotMaterialized` node is recomputed for each consumer that reads it
//!   and is not stored: it is charged once per consumer
//!   ([`NodeCost::multiplier`](asap_cost::NodeCost::multiplier)) at the sum of their rates. Its consumers are
//!   the queries whose sub-DAG reaches it and the materialized nodes above
//!   it, counted without looking through a materialized node: a node below
//!   one is computed when that node is. Stage 2 keeps a shared node one
//!   node (it copies it only for readers at two timings). A shared
//!   `NotMaterialized` input costs what reading it separately does (#509
//!   Example 4) only when every consumer reads the same window: a query-time `Scan` is priced over the longest window
//!   read above it, so each consumer pays for that window, and a scan a 1 m
//!   and a 5 m window share costs more than two separate scans. Stage 2
//!   offers the highest shared node built once per evaluation (`MatAtQuery`)
//!   as another candidate.
//! * A `MatAtQuery` node is computed once for all the queries that reach
//!   it, at their combined rate: queries repeating at the same interval are
//!   evaluated together, so each distinct interval counts once.
//! * A `MatAtIngestion` node runs as data arrives: it is priced over one
//!   second of arriving rows, once per second, whatever its readers' rates.
//!   A tumbling-pane chain maintained this way updates only its newest
//!   pane: each arriving row is placed in it and updates it.
//! * A query repeating every `I` ms runs `1000 / I` times per second, and an
//!   estimated rate runs at that rate. One-off, scheduled and unknown
//!   queries are spread over the settings' `non_repeating_query_horizon_seconds`
//!   (default 3600 s): `invocations / horizon` per second.
//!
//! Under cost model 1, memory is charged as bytes held per second
//! ([`ResourceUsage::average_memory_bytes`](asap_cost::ResourceUsage::average_memory_bytes)), at
//! `cost_per_memory_byte_second` per byte per second. A `MatAtIngestion`
//! node keeps its state every second: twice for a whole window (the window
//! being built and the completed one queries read) and `N + 1` panes for a
//! chain of `N` (the `N` the query merges and the one being built). A
//! `MatAtQuery` node's state is reused only within one evaluation, so it is
//! held only while that evaluation runs: its bytes times the latency of the
//! longest query reading it, once per evaluation. A query-time
//! `NotMaterialized` node's working memory (a sketch being built, an exact
//! aggregate's values) is charged once per consumer that recomputes it:
//! its bytes times each reading query's latency, at that query's rate.
//! A `MatAtQueryKeptAcrossEvaluations` tumbling-pane chain (#509 Example
//! 4's B3, offered by Stage 2 when the deployment keeps query-time results,
//! [`asap_logical_asap_optimization::DeploymentCapabilities::query_time_results_retained`]) is kept across
//! evaluations: each evaluation scans one pane of raw data and builds the
//! newest pane, and the `N − 1` older panes, which complete the readers'
//! look-back, are kept every second (in memory, all `N` at the peak). When
//! Stage 2 offers both query-time options for a chain, plan selection
//! prices each and the cheaper wins.
//!
//! A query whose workload entry declares a range evaluation
//! ([`RangeEvaluation`](asap_types::workload::RangeEvaluation): each request evaluates it at every `step_ms` over
//! the last `span_ms`, as a dashboard's `query_range` does) is priced as
//! `S = span_ms / step_ms + 1` evaluations per request (61 for a 1 h span
//! at 1 min steps):
//!
//! * every query-time computation that is not kept across evaluations
//!   (`NotMaterialized`, `MatAtQuery`) runs once per step, `S` times per
//!   request; a `MatAtIngestion` state is maintained as before, and the
//!   merge and estimate over it run once per step;
//! * a chain kept across evaluations is built once per request: Stage 2
//!   offers it only when the requests repeat at its pane width, so in the
//!   steady state each request builds its newest pane and every other pane
//!   any of its steps reads was built by an earlier request or step;
//! * a pane chain kept beyond one evaluation (ingestion time or kept
//!   across evaluations) retains its input's offset plus its window plus
//!   the longest `span_ms` among its readers ([`Selection::pane_retention_ms`]),
//!   so the earliest step of the next request still finds its panes: that
//!   many more panes are kept every second.
//!
//! Latency stays per evaluation (one step). Without a range evaluation
//! `S = 1` and the retention is the window, so every price is as above.
//! Memory a computation holds only while it runs counts in
//! [`ResourceUsage::peak_memory_bytes`](asap_cost::ResourceUsage::peak_memory_bytes), which cost models 2 and 3 read, and
//! under cost model 1 for the time it is held, as above. (A `NotMaterialized`
//! node computed at ingestion time is a streaming operator holding one row,
//! and is not charged.)
//!
//! Prices come from [`asap_cost::analytical::estimate_operator`] over edge
//! statistics derived from the [`DataWorkload`](asap_types::workload::DataWorkload) and a fixed default group
//! count; summary build and estimation are priced as rows × sketch depth and
//! groups × k. A query-time scan reads the longest window above it (a time
//! shift moves a window without widening it); a time shift is free, and a
//! time range is priced by the rows it keeps. A tumbling-pane chain is
//! priced as one build over its whole window plus one operation per row to
//! place it in its pane, and keeps one state per group and pane; a merge
//! costs one operation per state it reads. Kept state is sized by its bytes: a sketch by its parameters, a
//! fixed-size exact accumulator (sum, count, min, max, rate, ...) as one
//! value per group, and exact state that grows with the data (quantile,
//! top-k, distinct count, L2, entropy keep every value or item) as one value
//! per input row of its group, assuming rows spread evenly over groups and,
//! for distinct items, that every row may be distinct. These numbers are
//! illustrative, not calibrated: a binary, join or concat node is priced from
//! its first input only (MVP; follow-up).
//! A cost error on any candidate aborts selection; it is reachable only from
//! bad declared rates or bad [`CostInputs`](asap_cost::CostInputs).
//!
//! ## Measured operation costs
//!
//! A deployment whose runtime measured its own costs gives them per
//! operator ([`CostInputs::measured_operation_costs`](asap_cost::CostInputs::measured_operation_costs),
//! [`MeasuredOperationCosts`](asap_cost::MeasuredOperationCosts)): for an operator kind at ingestion or query
//! time (and, for a summary operator, its sketch or exact accumulator; for
//! an aggregate, each measure), CPU ns per unit and per computation. A node
//! with an entry takes `units × ns_per_unit + ns_per_invocation` per
//! computation, its scanning included; the units are what the operator
//! processes ([`OperatorKind`](asap_cost::OperatorKind): samples a scan reads, rows a build inserts,
//! states a merge reads, groups an estimate reads, ...). A query-time scan
//! is the fetch and decode of raw data samples from the database holding them,
//! so a plan that reads raw data pays for it. A sketch whose parameters
//! no entry has exactly is priced between the two measured configurations
//! of the same sketch nearest in state size, on a log scale, or from the
//! nearest one outside their range. A kept state's bytes are the entry's
//! measured `memory_bytes_per_state` when it has one. The newest window's
//! compaction (see "Latency") merges with the state's query-time merge
//! entry too, and each query request pays the table's `ns_per_evaluation`
//! once, on the node it reads first and in its latency.
//!
//! A node without an entry is priced from the formulas above, its CPU
//! operations times one calibration factor: the measured time of every
//! node the table prices, over all candidates, divided by what the
//! formulas give for those nodes. Without it a table that measures some
//! operators (a slow sketch) and not others (the exact plan's scan and
//! aggregate) makes the unmeasured plan look cheap only because its nodes
//! keep the formulas' scale. The factor is one global heuristic, not a
//! measurement: an unmeasured operator whose real cost is far from the
//! measured ones' ratio is still mispriced, so a partial table can still
//! mis-rank plans (e.g. a cheap raw scan and exact quantile priced at the
//! slow sketch's ratio make the exact rebuild look costlier than it is).
//! Only a table covering every operator kind the candidates use makes the
//! choice the measured one. Each node reports how it was priced
//! ([`NodePriceSource`](asap_cost::NodePriceSource)).
//! Measured times count as CPU operations of [`ResourceSpeed`](asap_cost::ResourceSpeed), so latency
//! and all three cost models read them like any other operation.
//!
//! ## Cost models
//!
//! [`CostInputs::cost_model`](asap_cost::CostInputs::cost_model) (in [`PlanningModels`]) chooses how
//! usage becomes a choice; the default is cost model 1 with
//! [`ResourceUnitCost::illustrative`](asap_cost::ResourceUnitCost::illustrative):
//!
//! * [`CostModel::UsageOverTime`](asap_cost::CostModel::UsageOverTime) (cost model 1, pay-as-you-go): each
//!   node's CPU operations, scanned bytes and memory byte-seconds per second
//!   (and disk bytes, unpriced by default) times their prices; the cheapest
//!   candidate wins.
//! * [`CostModel::ProvisionedCapacity`](asap_cost::CostModel::ProvisionedCapacity) (cost model 2): the candidate's peak
//!   memory, peak CPU cores and disk times their prices per second; the
//!   cheapest wins. Peak memory assumes every evaluation overlaps: kept
//!   in-memory state plus each computation's working memory, once per
//!   consumer that recomputes it. Peak cores are the ingestion work's cores
//!   plus one per query evaluating at once.
//! * [`CostModel::ClusterCapacity`](asap_cost::CostModel::ClusterCapacity) (cost model 3, own cluster): no prices.
//!   A candidate over the cluster's memory, disk or cores is invalid, and
//!   [`ClusterObjective`](asap_cost::ClusterObjective) chooses among the rest (lowest worst-query
//!   latency, least peak memory, or the Pareto front of the three in
//!   [`Selection::front`]).
//!
//! Disk holds state materialized on `Medium::Disk` (Stage 2 places none
//! there yet) and the raw data query-time scans read: per source, the
//! longest window a scan reads. A deployment that does not retain raw data
//! ([`asap_logical_asap_optimization::DeploymentCapabilities::raw_data_retained`]) keeps no raw data samples
//! after they arrive, so a candidate with a query-time scan is invalid,
//! with the window it would read as the reason.
//!
//! ## Latency
//!
//! Following the sketch-bench RQE cost model (§5, batch latency), a query's
//! latency per evaluation is its longest chain of work, each step on a
//! full core of elastic CPU (no queueing): the compaction of the newest
//! window of an ingestion-time state it reads, then its own query-time
//! work. Its own work is the sum, over every query-time node it reaches
//! (shared ones included, each once), of one computation's CPU operations
//! and scanned bytes timed by [`ResourceSpeed`](asap_cost::ResourceSpeed)'s `ms_per_cpu_op` and
//! `ms_per_scanned_byte`. A state whose ingestion work needs `k` cores keeps
//! `k` partial copies of its newest window, and compacting them takes
//! `(k − 1)` merges for each of its states per window, each one operation
//! (times the calibration factor), or the state's measured merge time when
//! the measured table has one; with
//! one core there is nothing to compact. A query with
//! `Some(bound)` whose latency is above the
//! bound makes the candidate invalid, with the latency as the reason
//! ([`RejectionCause::LatencyBoundExceeded`]). With or without a bound, a
//! query repeating every `I` ms whose latency is above `I` makes the
//! candidate invalid too ([`RejectionCause::LatencyOverRepeatInterval`]):
//! each evaluation starts before the last one ends, so it falls further
//! behind with every evaluation. Only a fixed interval is checked: an
//! estimated rate, a schedule or a one-time query has none. A range
//! evaluation is checked by its latency per step, as priced: the latency
//! model gives each step a full core, so a request's steps may run in
//! parallel. Run one after another they take `S` times as long, which this
//! check does not assume.
//! [`ResourceUsage::latency_per_query`](asap_cost::ResourceUsage::latency_per_query) reports every query's latency.
//!
//! No memory limit applies outside cost model 3, where the cluster's memory
//! is declared ([`ClusterLimits::memory_bytes`](asap_cost::ClusterLimits::memory_bytes)); without one, a selected
//! plan whose peak memory is over [`LARGE_PEAK_MEMORY_BYTES`] gets a
//! warning, which `stage_pipeline` reports ([`large_peak_memory_warning`]).

#[cfg(test)]
use asap_types::workload::RepeatedDemand;

use asap_cost::analytical::AnalyticalCostError;
use asap_cost::state_bytes::{sketch_shape, summary_shape, StateInputRows};

mod accuracy;
mod accuracy_binding;
mod checks;
mod pricing;
mod report;
mod selector;
mod sizing;
mod statistics;
pub(crate) use accuracy_binding::bound_root_guarantee;
pub(crate) use checks::{accuracy_targets, guarantee_check};
pub(crate) use pricing::{panes_of, price_node, trailing_window, PricedNode, ProducerOutput};
pub use report::{
    large_peak_memory_warning, AcceptedAccuracy, CandidateConnectionReport, Rejection,
    RejectionCause, SearchReport, Selection, SelectionError, LARGE_PEAK_MEMORY_BYTES,
};
pub(crate) use selector::{cheaper, checked_candidate, choose, Ranking};
pub use selector::{select_plan, PlanSelector};
pub use sizing::resize_for_failure_probability;
pub(crate) use sizing::sized_for_failure_probability;
pub use statistics::{confirmed_weights, with_confirmed_weights};
pub(crate) use statistics::{
    declared_non_negative, declared_series, series_read, source_name, source_rows_per_ms,
};

/// The Stage 1 to Stage 3 driver and the optimization-pass interface.
pub mod search;
pub use search as pass;
pub use search::{
    optimize, run_stage1_to_stage3, run_stage1_to_stage3_with, MemoSteps, OptimizationInput,
    OptimizationInputError, OptimizationPass, OptimizeError, PassNameConflict, PassRegistry,
    PlanOutput, PlanningModels, QueryPlan, Stage1ToStage3Pass, Stage1ToStage3Run,
    Stage1ToStage3Search,
};

use asap_cost::measured::{lookup, StateFamily};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accuracy_binding::{bind_statistics, root_operators, workload_statistic};
    use crate::checks::{accuracy_check, CheckFailure};
    use crate::pricing::{kept_pane_chains, node_map, price_at_scale};
    use crate::statistics::values_per_result;
    use asap_accuracy::composition::INPUT_ROW_COUNT;
    use asap_accuracy::estimators::hll;
    use asap_cost::execution_demand::evaluation_rate;
    use asap_cost::{
        CalibrationFit, CandidateCost, ClusterLimits, ClusterObjective, CostInputs, CostModel,
        NodePriceSource, OperatorKind, ResourceUsage, COST_SOURCE, MEASURED_COST_SOURCE,
        MIXED_COST_SOURCE,
    };
    use asap_cost::{MeasuredOperationCosts, OperatorCost, ResourceUnitCost};
    use asap_logical_asap_optimization::pass1::per_computation_candidates::root_guarantee;
    use asap_physical_asap_optimization::PhysicalWorkloadPlan;
    use asap_types::ir::operator::{AggIntent, Reduction};
    use asap_types::ir::physical_export::{
        PhysicalASAPDAG, PhysicalASAPNodeId, PhysicalASAPOperatorPayload as Payload,
    };
    use asap_types::ir::properties::{AccuracyGuarantee, AccuracyScope, ExecutionTiming};
    use asap_types::ir::scalar::ScalarValue;
    use asap_types::ir::schema::DataType;
    use asap_types::ir::schema::{
        FieldDataType, Schema, SketchAlgorithm, SketchParams, WeightDomain,
    };
    use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot};
    use asap_types::physical::{Materialization, Medium};
    use asap_types::workload::{
        DataWorkload, QueryRecurrence, QueryWorkloadEntry, RangeEvaluation,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::rc::Rc;

    /// [`crate::pricing::price`] with `dag`'s node map and kept pane chains.
    fn price(
        dag: &PhysicalASAPDAG,
        recurrence: &[QueryRecurrence],
        data: &DataWorkload,
        settings: &CostInputs,
    ) -> Result<CandidateCost, (PhysicalASAPNodeId, AnalyticalCostError)> {
        crate::pricing::price(
            dag,
            &node_map(dag),
            &kept_pane_chains(dag),
            recurrence,
            &[],
            data,
            settings,
        )
    }
    use asap_accuracy::{
        AccuracyEvidenceProvider, AccuracyModel, BuiltinAccuracyModel, NoAccuracyEvidence,
    };
    use asap_logical_asap_optimization::deployment_capabilities::DeploymentCapabilities;
    use asap_logical_asap_optimization::test_support::{entry, entry_with, lower_promql};
    use asap_physical_asap_optimization::optimize_physical_asap;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::properties::{AccuracyMetric, BoundExpr, ProbabilityExpr};
    use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
    use asap_types::ir::schema::{GroupingStrategy, SketchKind};
    use asap_types::ir::{ExprSemantics, ScalarExpr};
    use asap_types::workload::{DataArrival, LabelSetDistinctValues, MetricType, SourceFacts};

    /// `G` of `root` over `data` ([`values_per_result`]).
    fn values_of(root: &QueryRoot, data: &DataWorkload) -> Option<u64> {
        values_per_result(root, series_read(root, data), data)
    }

    /// [`bound_root_guarantee`] of `root` over `data`.
    fn bound_of(
        root: &QueryRoot,
        data: &DataWorkload,
        models: &PlanningModels<'_>,
    ) -> Option<AccuracyGuarantee> {
        bound_root_guarantee(root, series_read(root, data), data, models)
    }

    /// `n` queries repeating every 10 s, as Example 1's panels do.
    fn recurrences(n: usize) -> Vec<QueryRecurrence> {
        vec![repeating(10_000); n]
    }

    /// A query repeating every `interval_ms`.
    fn repeating(interval_ms: u32) -> QueryRecurrence {
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
            asap_types::workload::RepetitionInterval(interval_ms),
        ))
    }

    /// The metrics and tables the tests' queries read.
    const TEST_SOURCES: [&str; 5] = ["m", "x", "a", "b", "t"];

    /// Data arriving at 10 000 rows per second, each test source holding
    /// 10 000 series.
    fn data() -> DataWorkload {
        with_series(Some(10_000))
    }

    /// [`data`] with every test source holding `series` (`None`: unknown).
    fn with_series(series: Option<u64>) -> DataWorkload {
        DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 10_000.0,
            },
            sources: TEST_SOURCES
                .iter()
                .map(|source| {
                    let facts = SourceFacts {
                        series_count: series,
                        ..SourceFacts::default()
                    };
                    (source.to_string(), facts)
                })
                .collect(),
        }
    }

    /// Explicit domain assumption needed for interpolated DDS quantiles.
    fn non_negative_data() -> DataWorkload {
        let mut data = data();
        for facts in data.sources.values_mut() {
            facts.values_non_negative = true;
        }
        data
    }

    /// `data` with each of `types`' metrics declared of its type.
    fn with_metric_types(mut data: DataWorkload, types: &[(&str, MetricType)]) -> DataWorkload {
        for (metric, metric_type) in types {
            data.sources
                .entry(metric.to_string())
                .or_default()
                .metric_type = Some(*metric_type);
        }
        data
    }

    /// Classic HLL's failure probability over a source binds only that
    /// source's distinct bound: a bound keyed to another source leaves it
    /// unknown.
    #[test]
    fn classic_hll_failure_probability_binds_only_its_own_sources_bound() {
        let statistic = asap_logical_asap_optimization::test_support::hll_classic_failure_statistic(
            15, 0.05, "a",
        );
        let bounded = |source: &str| {
            let mut data = data();
            data.sources
                .get_mut(source)
                .unwrap()
                .max_distinct_per_evaluation = Some(100);
            data
        };
        assert_eq!(workload_statistic(&statistic, None, &bounded("b")), None);
        assert_eq!(
            workload_statistic(&statistic, None, &bounded("a")),
            Some(hll::classic_failure_probability(15, 0.05, 100))
        );
        assert!(hll::classic_failure_probability(15, 0.05, 100) <= 0.01);
    }

    /// Exact (P1), whole-expression CMS+heap (P2) and CountSketch+heap (P3) realizations of
    /// one approximate top-k query, in Pass 1 catalog order.
    fn candidates() -> Vec<PhysicalWorkloadPlan> {
        let root = lower_promql(
            "topk by (job) (10, sum_over_time(m[1m]))",
            AccuracyTarget::max_error(0.01).failure_probability(0.001),
        );
        let inventory = asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        let topk = inventory
            .targets
            .iter()
            .position(|t| t.choice_count() > 2)
            .unwrap();
        [0, 1, 2]
            .into_iter()
            .map(|index| {
                let mut choice = vec![0; inventory.targets.len()];
                choice[topk] = index;
                let roots: Vec<_> = asap_logical_asap_optimization::pass1::per_computation_candidates::compose_logical_candidate(
                    &inventory, &choice,
                )
                .unwrap()
                .into_iter()
                .map(|(_, root)| root)
                .collect();
                let mut candidate = optimize_physical_asap(
                    "L",
                    &roots,
                    &[],
                    &data(),
                    false,
                )
                .unwrap()
                .remove(0);
                candidate.id = format!("P{}", index + 1);
                candidate
            })
            .collect()
    }

    /// A summary whose analytical guarantee misses the target is rejected as
    /// invalid with a reason; the exact plan is then selected. (Top-K
    /// estimates carry no guarantee since #575, so a quantile summary is used.)
    #[test]
    fn accuracy_failing_candidate_is_rejected_as_invalid() {
        let planned = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let candidates = all_candidates("quantile_over_time(0.99, m[1m])", &planned);
        let strict = AccuracyTarget::max_error(1e-6).failure_probability(1e-9);
        let summaries: Vec<_> = candidates
            .iter()
            .filter(|c| has_summary_estimate(c))
            .map(|c| c.id.as_str())
            .collect();
        assert!(!summaries.is_empty());
        let selection = select_plan(
            &candidates,
            &[entry_with(strict, repeating(10_000))],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        assert!(!summaries.contains(&selection.selected.as_str()));
        for id in &summaries {
            let rejection = selection.rejected.iter().find(|r| r.id == *id).unwrap();
            assert_eq!(rejection.cause, RejectionCause::AccuracyTargetMissed);
            assert!(
                rejection.reason.contains("misses target"),
                "{}",
                rejection.reason
            );
            assert!(!selection.costs.contains_key(*id));
        }
    }

    /// For a query with a target, Count-Min over weights not proven
    /// non-negative is invalid.
    #[test]
    fn count_min_over_signed_weights_is_rejected_as_invalid() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let selection = select_plan(
            &candidates(),
            &[entry_with(
                target
                    .clone()
                    .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L1),
                repeating(10_000),
            )],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        let p2 = selection.rejected.iter().find(|r| r.id == "P2").unwrap();
        assert_eq!(
            p2.cause,
            RejectionCause::CountMinWeightsNotProvenNonNegative
        );
        assert!(p2.reason.contains("non-negative"), "{}", p2.reason);
    }

    /// Count-Min over raw data samples of `m` is admitted only when the data
    /// workload declares `m` a counter: a gauge, an undeclared metric or a
    /// counter under another name leaves the requirement unmet. Admitted,
    /// the top-k still needs evidence about its output.
    #[test]
    fn count_min_over_raw_data_samples_needs_a_declared_counter() {
        use asap_types::workload::MetricType;
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let with_types = |types: &[(&str, MetricType)]| with_metric_types(data(), types);
        for (types, admitted) in [
            (vec![("m", MetricType::Counter)], true),
            (vec![("m", MetricType::Gauge)], false),
            (vec![("m_total", MetricType::Counter)], false),
            (vec![], false),
        ] {
            let data = with_types(&types);
            let selection = select_plan(
                &candidates(),
                &[entry_with(
                    target
                        .clone()
                        .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L1),
                    repeating(10_000),
                )],
                &data,
                PlanningModels::builtin(),
            )
            .unwrap();
            let p2 = selection.rejected.iter().find(|r| r.id == "P2").unwrap();
            // Admitted weights leave the top-k output without a guarantee,
            // which the built-in model does not accept; unproven weights
            // reject the plan before any accuracy check.
            let expected_cause = if admitted {
                RejectionCause::NoAccuracyGuarantee
            } else {
                RejectionCause::CountMinWeightsNotProvenNonNegative
            };
            assert_eq!(p2.cause, expected_cause);
            assert_eq!(
                !p2.reason.contains("non-negative"),
                admitted,
                "{types:?}: {}",
                p2.reason
            );
            if admitted {
                assert!(p2.reason.contains("no guarantee"), "{}", p2.reason);
            } else {
                assert!(
                    p2.reason.contains("not declared a counter"),
                    "{}",
                    p2.reason
                );
            }
        }
    }

    /// The Count-Min sketch algorithms (plain or with a heap) `candidate`
    /// builds.
    fn count_min_builds(candidate: &PhysicalWorkloadPlan) -> Vec<SketchAlgorithm> {
        candidate
            .dag
            .nodes
            .iter()
            .filter_map(|node| match &node.payload {
                Payload::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) if matches!(
                    kind.algorithm(),
                    SketchAlgorithm::Cms | SketchAlgorithm::CmsWithHeap
                ) =>
                {
                    Some(*kind.algorithm())
                }
                _ => None,
            })
            .collect()
    }

    /// `increase` (and `rate`, `irate`) over a counter is non-negative, since
    /// resets are corrected: the Count-Min-with-heap top-k of
    /// `topk(10, sum by (job) (increase(data_total[5m])))` passes the weight
    /// check when `data_total` is declared a counter, and goes on to the
    /// accuracy and cost checks. Over a gauge it is still rejected for its
    /// weights (issue #773).
    #[test]
    fn count_min_over_a_counter_derivative_needs_a_declared_counter() {
        let target = AccuracyTarget::max_error(0.01)
            .failure_probability(0.001)
            .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L1);
        // An `offset` moves the window, not the samples' sign.
        for query in [
            "topk(10, sum by (job) (increase(data_total[5m])))",
            "topk(10, sum by (job) (rate(data_total[5m])))",
            "topk(10, sum by (job) (irate(data_total[5m])))",
            "topk(10, sum by (job) (rate(data_total[5m] offset 1h)))",
        ] {
            let candidates = all_candidates(query, &target);
            let count_min: Vec<&str> = candidates
                .iter()
                .filter(|c| count_min_builds(c).contains(&SketchAlgorithm::CmsWithHeap))
                .map(|c| c.id.as_str())
                .collect();
            assert!(!count_min.is_empty(), "{query}");
            for (metric_type, admitted) in [(MetricType::Counter, true), (MetricType::Gauge, false)]
            {
                let data = with_metric_types(data(), &[("data_total", metric_type)]);
                let rejected = match select_plan(
                    &candidates,
                    &[entry_with(target.clone(), repeating(10_000))],
                    &data,
                    PlanningModels::builtin(),
                ) {
                    Ok(selection) => selection.rejected,
                    Err(SelectionError::NoValidCandidate(rejected)) => rejected,
                    Err(error) => panic!("{query}: {error}"),
                };
                for id in &count_min {
                    let weights_rejected = rejected.iter().any(|r| {
                        &r.id == id
                            && r.cause == RejectionCause::CountMinWeightsNotProvenNonNegative
                    });
                    assert_eq!(
                        weights_rejected, !admitted,
                        "{query} over a {metric_type:?}, {id}: {rejected:?}"
                    );
                }
            }
        }
    }

    /// A gauge declared non-negative
    /// ([`SourceFacts::values_non_negative`]) proves its raw data samples, and
    /// plain sums of them, non-negative: the Count-Min-with-heap top-k of
    /// `topk by (job) (10, sum_over_time(m[1m]))` (the ASAPQuery workloads'
    /// top-k-sum shape) passes the weight check, and the selected plan carries
    /// [`NonNegativeWeightProof::DeclaredNonNegativeRawDataSamples`]. A rate or
    /// increase of that gauge stays unproven: a reset correction over a
    /// gauge can still give a negative value.
    #[test]
    fn count_min_over_raw_data_samples_of_a_gauge_declared_non_negative_is_admitted() {
        use asap_types::ir::schema::NonNegativeWeightProof;
        let target = AccuracyTarget::max_error(0.01)
            .failure_probability(0.001)
            .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L1);
        let mut gauge = with_metric_types(data(), &[("m", MetricType::Gauge)]);
        gauge.sources.get_mut("m").unwrap().values_non_negative = true;
        for (query, admitted) in [
            ("topk by (job) (10, sum_over_time(m[1m]))", true),
            ("topk(10, sum by (job) (increase(m[5m])))", false),
        ] {
            let candidates = all_candidates(query, &target);
            let count_min: Vec<&PhysicalWorkloadPlan> = candidates
                .iter()
                .filter(|c| count_min_builds(c).contains(&SketchAlgorithm::CmsWithHeap))
                .collect();
            assert!(!count_min.is_empty(), "{query}");
            let rejected = match select_plan(
                &candidates,
                &[entry_with(target.clone(), repeating(10_000))],
                &gauge,
                PlanningModels::builtin(),
            ) {
                Ok(selection) => selection.rejected,
                Err(SelectionError::NoValidCandidate(rejected)) => rejected,
                Err(error) => panic!("{query}: {error}"),
            };
            for candidate in &count_min {
                let weights_rejected = rejected.iter().any(|r| {
                    r.id == candidate.id
                        && r.cause == RejectionCause::CountMinWeightsNotProvenNonNegative
                });
                assert_eq!(
                    weights_rejected, !admitted,
                    "{query}, {}: {rejected:?}",
                    candidate.id
                );
                if !admitted {
                    continue;
                }
                // The proof the executor reads.
                let proofs: Vec<WeightDomain> = with_confirmed_weights(&candidate.roots, &gauge)
                    .iter()
                    .flat_map(root_operators)
                    .flat_map(OperatorNode::reachable)
                    .filter_map(|n| match n.asap() {
                        Some(ASAPOp::SummaryAgg {
                            input,
                            family: FieldDataType::Sketch(kind, _),
                            ..
                        }) if *kind.algorithm() == SketchAlgorithm::CmsWithHeap => {
                            Some(input.weight_domain.clone())
                        }
                        _ => None,
                    })
                    .collect();
                assert!(!proofs.is_empty(), "{}", candidate.id);
                assert!(
                    proofs.iter().all(|domain| *domain
                        == WeightDomain::NonNegative {
                            proof: NonNegativeWeightProof::DeclaredNonNegativeRawDataSamples
                        }),
                    "{}: {proofs:?}",
                    candidate.id
                );
            }
        }
    }

    /// The sign gate is an indicator that only 1 (proven) opens: any other
    /// value a binder supplies, 0 included, leaves it unbound rather than
    /// reading as a perfect bound. Other statistics pass through.
    #[test]
    fn the_sign_gate_opens_only_at_one() {
        use asap_accuracy::composition::raw_data_samples_sign_statistic;
        let gated = AccuracyGuarantee::new(
            AccuracyMetric::Relative,
            BoundExpr::Product {
                factors: vec![
                    BoundExpr::Constant { value: 0.01 },
                    BoundExpr::Unknown {
                        statistic: raw_data_samples_sign_statistic("m"),
                    },
                ],
            },
            ProbabilityExpr::Constant { value: 0.001 },
            vec![],
        );
        for value in [0.0, 0.5, 2.0, -1.0] {
            assert_eq!(
                bind_statistics(&gated, &|_| Some(value))
                    .spec
                    .error
                    .evaluate(),
                None,
                "{value}"
            );
        }
        assert_eq!(
            bind_statistics(&gated, &|_| Some(1.0))
                .spec
                .error
                .evaluate(),
            Some(0.01)
        );
        let mut norm = gated;
        norm.spec.error = BoundExpr::Unknown {
            statistic: INPUT_ROW_COUNT.into(),
        };
        assert_eq!(
            bind_statistics(&norm, &|_| Some(0.0)).spec.error.evaluate(),
            Some(0.0)
        );
    }

    /// A relative bound gated on the sign of one metric's samples is bound
    /// when the data workload declares that metric a counter, and stays
    /// unknown for a gauge, an undeclared metric or a counter by another
    /// name.
    #[test]
    fn sign_of_counter_raw_data_samples_is_bound_from_the_declared_type() {
        use asap_accuracy::composition::raw_data_samples_sign_statistic;
        let gated = AccuracyGuarantee::new(
            AccuracyMetric::Relative,
            BoundExpr::Product {
                factors: vec![
                    BoundExpr::Constant { value: 0.01 },
                    BoundExpr::Unknown {
                        statistic: raw_data_samples_sign_statistic("m"),
                    },
                ],
            },
            ProbabilityExpr::Constant { value: 0.001 },
            vec![],
        );
        let node = lower_promql("sum(m)", AccuracyTarget::exact());
        let root = QueryRoot::Operator(Rc::new((*node).clone().with_guarantee(Some(gated))));
        let with = |types: &[(&str, MetricType)]| with_metric_types(data(), types);
        let bound = |data: &DataWorkload| {
            bound_of(&root, data, &PlanningModels::builtin())
                .unwrap()
                .spec
                .error
                .evaluate()
        };
        assert_eq!(bound(&with(&[("m", MetricType::Counter)])), Some(0.01));
        for types in [
            vec![("m", MetricType::Gauge)],
            vec![("m_total", MetricType::Counter)],
            vec![],
        ] {
            assert_eq!(bound(&with(&types)), None, "{types:?}");
        }
    }

    /// Every consumer of a run sees the selected plan with its weight
    /// requirements answered: the logical plan (`selected_logical`) and the
    /// Offered one at a time and keeping only the best so far (the Pareto
    /// front so far under `pareto-front`), the selector selects what
    /// `select_plan` selects over every candidate at once, at the same cost
    /// and with the same front, under every cost model.
    #[test]
    fn streaming_selection_selects_what_select_plan_selects() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let root = lower_promql("quantile_over_time(0.99, m[5m])", target.clone());
        let entries = [entry_with(target, repeating(60_000))];
        let data = with_series(Some(10_000));
        let run = crate::search::run_stage1_to_stage3_with(
            vec![(0, QueryRoot::Operator(root))],
            &entries,
            &data,
            PlanningModels::builtin(),
            crate::search::Stage1ToStage3Search::exhaustive(),
        )
        .unwrap();
        let candidates = &run.stage2_physical_asap_optimization;
        assert!(candidates.len() > 2);
        let cluster = |objective| {
            CostModel::ClusterCapacity(ClusterLimits {
                memory_bytes: u64::MAX,
                disk_bytes: u64::MAX,
                cpu_cores: None,
                objective,
            })
        };
        for cost_model in [
            CostModel::UsageOverTime(ResourceUnitCost::illustrative()),
            CostModel::ProvisionedCapacity(ResourceUnitCost::illustrative()),
            cluster(ClusterObjective::LowestLatency),
            cluster(ClusterObjective::LeastMemory),
            cluster(ClusterObjective::ParetoFront),
        ] {
            let settings = CostInputs {
                cost_model,
                ..CostInputs::default()
            };
            let models = PlanningModels::builtin().with_plan_selection(&settings);
            let all = select_plan(candidates, &entries, &data, models).unwrap();
            let mut selector = PlanSelector::new(&entries, &data, models, false).unwrap();
            for candidate in candidates {
                selector.offer(candidate).unwrap();
            }
            let (streamed, built) = selector.finish().unwrap();
            assert_eq!(streamed.selected, all.selected);
            assert_eq!(built.id, all.selected);
            assert_eq!(streamed.plan, all.plan);
            assert_eq!(streamed.costs[&streamed.selected], all.costs[&all.selected]);
            assert_eq!(streamed.front, all.front);
            assert_eq!(streamed.search.candidates_checked, candidates.len() as u64);
            // Only the selected candidate and the front are kept.
            assert!(streamed.costs.len() <= all.front.len().max(1));
        }
    }

    /// executable roots (`Selection::plan`) carry the counter proof, not the
    /// open requirement the executor rejects.
    #[test]
    fn the_selected_plan_carries_confirmed_weights() {
        use asap_types::workload::MetricType;
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let root = lower_promql("topk by (job) (10, sum_over_time(m[1m]))", target.clone());
        let root =
            asap_types::ir::schema::schema_support::with_promql_series_identity(&root).unwrap();
        let mut data =
            with_metric_types(with_series(Some(10_000_000)), &[("m", MetricType::Counter)]);
        // 10 jobs: one Count-Min per job is cheaper than the exact per-series sums.
        data.sources.get_mut("m").unwrap().distinct_values_per_label =
            [("job".to_string(), 10)].into();
        let run = crate::search::run_stage1_to_stage3_with(
            vec![(0, QueryRoot::Operator(root))],
            &[entry_with(target, repeating(10_000))],
            &data,
            PlanningModels::builtin().with_evidence(&EveryOutputMeasuredExact),
            crate::search::Stage1ToStage3Search::exhaustive(),
        )
        .unwrap();
        let domains = |roots: &[QueryRoot]| -> Vec<WeightDomain> {
            roots
                .iter()
                .flat_map(root_operators)
                .flat_map(OperatorNode::reachable)
                .filter_map(|n| match n.asap() {
                    Some(ASAPOp::SummaryAgg {
                        input,
                        family: FieldDataType::Sketch(kind, _),
                        ..
                    }) if *kind.algorithm() == SketchAlgorithm::CmsWithHeap => {
                        Some(input.weight_domain.clone())
                    }
                    _ => None,
                })
                .collect()
        };
        let proven = [WeightDomain::NonNegative {
            proof: asap_types::ir::schema::NonNegativeWeightProof::CounterRawDataSamples,
        }];
        let selection = run.stage3_plan_selection.as_ref().unwrap();
        assert_eq!(domains(&selection.plan), proven, "{selection:?}");
        assert_eq!(domains(run.selected_logical().unwrap()), proven);
    }

    /// The selected plan's open requirement becomes a proof when confirmed
    /// and stays unproven otherwise; the executor reads no data workload.
    #[test]
    fn confirmed_weights_rewrite_only_the_summary_update() {
        use asap_types::ir::schema::NonNegativeWeightProof;
        use asap_types::workload::MetricType;
        let p2 = &candidates()[1];
        let domains = |roots: &[QueryRoot]| -> Vec<WeightDomain> {
            roots
                .iter()
                .flat_map(root_operators)
                .flat_map(OperatorNode::reachable)
                .filter_map(|n| match n.asap() {
                    Some(ASAPOp::SummaryAgg { input, .. }) => Some(input.weight_domain.clone()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            domains(&p2.roots),
            [WeightDomain::RawDataSamplesOfMetric { metric: "m".into() }]
        );
        let counter = with_metric_types(data(), &[("m", MetricType::Counter)]);
        let confirmed = with_confirmed_weights(&p2.roots, &counter);
        assert_eq!(
            domains(&confirmed),
            [WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::CounterRawDataSamples
            }]
        );
        let mut gauge = with_metric_types(data(), &[("m", MetricType::Gauge)]);
        gauge.sources.get_mut("m").unwrap().values_non_negative = true;
        assert_eq!(
            domains(&with_confirmed_weights(&p2.roots, &gauge)),
            [WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::DeclaredNonNegativeRawDataSamples
            }]
        );
        assert_eq!(
            domains(&with_confirmed_weights(&p2.roots, &data())),
            [WeightDomain::UnknownOrSigned]
        );
        // Nothing open: the same roots.
        let exact = &candidates()[0];
        let QueryRoot::Operator(before) = &exact.roots[0] else {
            panic!()
        };
        let QueryRoot::Operator(after) = &with_confirmed_weights(&exact.roots, &counter)[0] else {
            panic!()
        };
        assert!(Rc::ptr_eq(before, after));
    }

    /// Each node is priced under its DAG id, and the total is the sum;
    /// every candidate is either selected or rejected as costlier. The
    /// CountSketch+heap estimate has no analytical guarantee, so deployment
    /// evidence admits it.
    #[test]
    fn per_node_costs_cover_the_dag_and_sum_to_the_total() {
        let candidates = candidates();
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let selection = select_plan(
            &candidates,
            &[entry_with(
                target
                    .clone()
                    .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L1),
                repeating(10_000),
            )],
            &data(),
            PlanningModels::builtin().with_evidence(&EveryOutputMeasuredExact),
        )
        .unwrap();
        for candidate in candidates.iter().filter(|c| c.id != "P2") {
            let cost = &selection.costs[&candidate.id];
            let ids: Vec<_> = candidate.dag.nodes.iter().map(|n| n.id).collect();
            let mut keys: Vec<_> = cost.per_node.keys().copied().collect();
            keys.sort();
            let mut sorted = ids.clone();
            sorted.sort();
            assert_eq!(keys, sorted);
            let sum: f64 = cost
                .per_node
                .values()
                .map(|n| n.cost_per_second.unwrap())
                .sum();
            assert_eq!(cost.cost_per_second.unwrap(), sum);
            assert!(cost.cost_per_second.unwrap() > 0.0);
        }
        let costlier: Vec<_> = selection
            .rejected
            .iter()
            .filter(|r| r.cause == RejectionCause::Costlier)
            .collect();
        assert_eq!(costlier.len(), 1);
        assert_ne!(costlier[0].id, selection.selected);
    }

    /// Query 1 is `sum(...)` over query 2's output. Every node reached by
    /// both queries is `NotMaterialized`, so recomputed for each: charged
    /// twice. Materializing query 2's root (`MatAtQuery`) charges it once,
    /// and the nodes below it once, since they run only when it does.
    #[test]
    fn not_materialized_shared_nodes_are_charged_per_query() {
        let outer = lower_promql("sum(sum_over_time(m[1m]))", AccuracyTarget::exact());
        let inner = outer.children()[0].clone();
        let mut candidate = optimize_physical_asap(
            "L",
            &[QueryRoot::Operator(outer), QueryRoot::Operator(inner)],
            &[],
            &data(),
            false,
        )
        .unwrap()
        .remove(0);
        let [outer, inner] = candidate.dag.roots[..] else {
            panic!("two roots")
        };
        // Kept state is priced apart; this compares computations only.
        let settings = unit_costs(|c| c.cost_per_memory_byte_second = 0.0);
        let cost = price(&candidate.dag, &recurrences(2), &data(), &settings).unwrap();
        let multipliers: BTreeMap<_, _> = cost
            .per_node
            .iter()
            .map(|(id, c)| (*id, c.multiplier))
            .collect();
        assert_eq!(multipliers[&outer], 1);
        assert_eq!(multipliers[&inner], 2);
        assert!(multipliers.len() > 2, "nodes below the shared root");
        for (id, times) in &multipliers {
            if *id != outer {
                assert_eq!(*times, 2, "node {id} is read by both queries");
            }
        }

        let node = candidate
            .dag
            .nodes
            .iter_mut()
            .find(|n| n.id == inner)
            .unwrap();
        node.materialization = Materialization::MatAtQuery(asap_types::physical::Medium::Memory);
        let stored = price(&candidate.dag, &recurrences(2), &data(), &settings).unwrap();
        for (id, c) in &stored.per_node {
            assert_eq!(c.multiplier, 1, "node {id} is computed once");
            let once = cost.per_node[id].cost_per_second.unwrap() / multipliers[id] as f64;
            assert!(
                (c.cost_per_second.unwrap() - once).abs() <= 1e-12 * once,
                "node {id}"
            );
        }
        assert!(stored.cost_per_second.unwrap() < cost.cost_per_second.unwrap());
    }

    /// A `NotMaterialized` node two queries read is computed for each, so
    /// its working memory is held once per query, each time for that
    /// query's latency: `bytes · Σ rate_q · latency_q`, not the longest
    /// latency at both rates. Query 1 is `sum(...)` over query 2's output,
    /// so it runs longer; they repeat every 10 s and every 20 s.
    #[test]
    fn shared_not_materialized_memory_is_held_for_each_reading_query() {
        let outer = lower_promql("sum(sum_over_time(m[1m]))", AccuracyTarget::exact());
        let inner = outer.children()[0].clone();
        let candidate = optimize_physical_asap(
            "L",
            &[QueryRoot::Operator(outer), QueryRoot::Operator(inner)],
            &[],
            &data(),
            false,
        )
        .unwrap()
        .remove(0);
        let recurrence = [repeating(10_000), repeating(20_000)];
        let cost = |price_per_byte_second| {
            let settings = unit_costs(|c| c.cost_per_memory_byte_second = price_per_byte_second);
            price(&candidate.dag, &recurrence, &data(), &settings).unwrap()
        };
        let (with, without) = (cost(1.0), cost(0.0));
        let [outer_ms, inner_ms] = [0, 1].map(|q| with.usage.latency_per_query[q].total_ms());
        assert!(outer_ms > inner_ms, "{outer_ms} vs {inner_ms}");
        let mut checked = 0;
        for (id, node) in &with.per_node {
            let Some(held) = node.detail.split("; holds ").nth(1) else {
                continue;
            };
            if node.multiplier != 2 {
                continue;
            }
            let bytes: f64 = held.split(' ').next().unwrap().parse().unwrap();
            let term =
                node.cost_per_second.unwrap() - without.per_node[id].cost_per_second.unwrap();
            let expected = bytes * (0.1 * outer_ms + 0.05 * inner_ms) / 1_000.0;
            assert!(close(term, expected), "node {id}: {term} vs {expected}");
            checked += 1;
        }
        assert!(checked > 0, "a shared node holds memory");
    }

    /// Every combination of Pass 1 candidates of one query, as Stage 2
    /// candidates `P1`, `P2`, ...
    fn all_candidates(query: &str, target: &AccuracyTarget) -> Vec<PhysicalWorkloadPlan> {
        let root = lower_promql(query, target.clone());
        let inventory = asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        let mut choices = vec![vec![]];
        for target in &inventory.targets {
            choices = choices
                .into_iter()
                .flat_map(|choice: Vec<usize>| {
                    (0..target.choice_count()).map(move |index| {
                        let mut choice = choice.clone();
                        choice.push(index);
                        choice
                    })
                })
                .collect();
        }
        choices
            .iter()
            .enumerate()
            .map(|(index, choice)| {
                let roots: Vec<_> = asap_logical_asap_optimization::pass1::per_computation_candidates::compose_logical_candidate(
                    &inventory, choice,
                )
                .unwrap()
                .into_iter()
                .map(|(_, root)| root)
                .collect();
                let mut candidate = optimize_physical_asap(
                    "L",
                    &roots,
                    &[],
                    &data(),
                    false,
                )
                .unwrap()
                .remove(0);
                candidate.id = format!("P{}", index + 1);
                candidate
            })
            .collect()
    }

    fn has_summary_estimate(candidate: &PhysicalWorkloadPlan) -> bool {
        candidate
            .dag
            .nodes
            .iter()
            .any(|n| matches!(n.payload, Payload::ASAP(ASAPOp::SummaryEstimate { .. })))
    }

    /// The KLL candidates of `quantile_over_time(0.99, x[5m])`, per series,
    /// within rank error 0.01 at δ = 0.01 over `scope`.
    fn per_series_kll(scope: AccuracyScope) -> (Vec<PhysicalWorkloadPlan>, AccuracyTarget) {
        let requested = AccuracyTarget::max_error(0.01)
            .failure_probability(0.01)
            .accuracy_metric(AccuracyMetric::Rank);
        let candidates = all_candidates("quantile_over_time(0.99, x[5m])", &requested)
            .into_iter()
            .filter(|c| !kll_params(&c.roots).is_empty())
            .collect::<Vec<_>>();
        assert!(!candidates.is_empty());
        let target = requested
            .clone()
            .accuracy_metric(AccuracyMetric::Rank)
            .accuracy_scope(scope);
        (candidates, target)
    }

    /// A candidate whose summaries, resized for its query's failure
    /// probability, do not rebuild into a plan (here a root with no
    /// execution timing) is rejected with the reason; selection goes on and
    /// selects another candidate instead of failing.
    #[test]
    fn a_candidate_that_fails_to_resize_is_rejected_alone() {
        let requested = AccuracyTarget::max_error(0.01)
            .failure_probability(0.01)
            .accuracy_metric(AccuracyMetric::Rank);
        let mut candidates = all_candidates("quantile_over_time(0.99, x[5m])", &requested);
        let broken = candidates
            .iter()
            .position(|c| !kll_params(&c.roots).is_empty())
            .expect("a KLL candidate");
        let QueryRoot::Operator(node) = &candidates[broken].roots[0] else {
            panic!("an operator root")
        };
        let mut untimed = (**node).clone();
        untimed.timing = None;
        candidates[broken].roots = vec![QueryRoot::Operator(Rc::new(untimed))];
        let selection = select_plan(
            &candidates,
            &[entry_with(requested, repeating(10_000))],
            &data(),
            PlanningModels::builtin(),
        )
        .expect("the other candidates are still judged");
        let rejection = selection
            .rejected
            .iter()
            .find(|r| r.id == candidates[broken].id)
            .unwrap();
        assert_eq!(
            rejection.cause,
            RejectionCause::FailureProbabilityResizeFailed
        );
        assert!(
            rejection
                .reason
                .starts_with("resizing for the failure probability failed"),
            "{}",
            rejection.reason
        );
        assert_ne!(selection.selected, candidates[broken].id);
    }

    /// Bad input: a workload with a different number of entries than a
    /// candidate has query roots is an error, not a rejection.
    #[test]
    fn an_entry_count_mismatch_is_an_error() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let entries = vec![entry_with(target, repeating(10_000)); 2];
        let Err(SelectionError::EntryCount {
            candidate,
            roots,
            entries,
        }) = select_plan(&candidates(), &entries, &data(), PlanningModels::builtin())
        else {
            panic!("an entry count mismatch is an error")
        };
        assert_eq!((candidate.as_str(), roots, entries), ("P1", 1, 2));
    }

    fn kll_params(roots: &[QueryRoot]) -> Vec<SketchParams> {
        roots
            .iter()
            .flat_map(root_operators)
            .flat_map(OperatorNode::reachable)
            .filter_map(|node| match node.asap() {
                Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) if *kind.algorithm() == SketchAlgorithm::Kll => Some(kind.params().clone()),
                _ => None,
            })
            .collect()
    }

    /// A whole-result target over `G` = 10 000 series (the declared input
    /// cardinality) needs each series' KLL to fail with probability at most
    /// δ / G = 1e-6 (#669b): Stage 3 enlarges it before pricing, and the
    /// selected plan runs the enlarged sketch. A per-value target keeps the
    /// Stage 1 size.
    #[test]
    fn a_whole_result_target_enlarges_each_sketch_for_g_values() {
        let (candidates, whole) = per_series_kll(AccuracyScope::WholeResult);
        let select = |target: &AccuracyTarget| {
            select_plan(
                &candidates,
                &[entry_with(target.clone(), repeating(10_000))],
                &data(),
                PlanningModels::builtin(),
            )
            .unwrap()
        };
        let stage1 = kll_params(&candidates[0].roots);
        let enlarged = kll_params(&select(&whole).plan);
        assert_eq!(enlarged.len(), 1);
        let (
            SketchParams::Kll { k: k1, .. },
            SketchParams::Kll {
                k,
                failure_probability,
            },
        ) = (&stage1[0], &enlarged[0])
        else {
            panic!("{stage1:?} {enlarged:?}")
        };
        assert_eq!(*failure_probability, Some(0.01 / 10_000.0));
        assert!(k > k1, "{k} vs {k1}");
        let per_value = whole.clone().accuracy_scope(AccuracyScope::PerValue);
        assert_eq!(kll_params(&select(&per_value).plan), stage1);
    }

    /// With its metric declared a counter, the whole-expression CMS+heap
    /// candidate (P2) proves its weights non-negative, is valid and is
    /// priced: its per-node costs cover its DAG and sum to its total.
    #[test]
    fn a_declared_counter_prices_the_count_min_heap_candidate() {
        use asap_types::workload::MetricType;
        let candidates = candidates();
        let target = AccuracyTarget::max_error(0.01)
            .failure_probability(0.001)
            .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L1);
        let counter = with_metric_types(data(), &[("m", MetricType::Counter)]);
        let selection = select_plan(
            &candidates,
            &[entry_with(target, repeating(10_000))],
            &counter,
            PlanningModels::builtin().with_evidence(&EveryOutputMeasuredExact),
        )
        .unwrap();
        let p2 = candidates.iter().find(|c| c.id == "P2").unwrap();
        let cost = &selection.costs["P2"];
        let mut ids: Vec<_> = p2.dag.nodes.iter().map(|n| n.id).collect();
        ids.sort();
        let mut keys: Vec<_> = cost.per_node.keys().copied().collect();
        keys.sort();
        assert_eq!(keys, ids);
        let sum: f64 = cost
            .per_node
            .values()
            .map(|n| n.cost_per_second.unwrap())
            .sum();
        assert_eq!(cost.cost_per_second.unwrap(), sum);
    }

    /// One summary two queries read is sized for the stricter one: its
    /// failure probability is the smallest share either query needs (the
    /// largest `n · G` over its readers).
    #[test]
    fn a_shared_summary_takes_the_smallest_failure_probability_share() {
        let (candidates, lenient) = per_series_kll(AccuracyScope::PerValue);
        let root = candidates[0].roots[0].clone();
        let strict = lenient.clone().failure_probability(0.001);
        let roots = [root.clone(), root];
        for targets in [[lenient.clone(), strict.clone()], [strict, lenient]] {
            let resized = resize_for_failure_probability(&roots, &targets, &data()).unwrap();
            let kll = kll_params(&resized);
            assert_eq!(kll.len(), 2, "{kll:?}");
            for params in kll {
                let SketchParams::Kll {
                    failure_probability,
                    ..
                } = params
                else {
                    panic!("{params:?}")
                };
                assert_eq!(failure_probability, Some(0.001));
            }
        }
    }

    /// The KLL failure probability each selected sketch is sized for.
    fn kll_failure_probabilities(roots: &[QueryRoot]) -> Vec<Option<f64>> {
        kll_params(roots)
            .into_iter()
            .map(|params| match params {
                SketchParams::Kll {
                    failure_probability,
                    ..
                } => failure_probability,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    /// A target on the groups holding at least 5% of the records covers at
    /// most 20 of them, so each sketch is sized for δ / 20 and the target is
    /// met even when the number of values the result returns is unbound
    /// (here a source with no declared series count), where a whole-result
    /// target stays unmet. With 10 000 series declared it still needs only
    /// δ / 20, not the whole result's δ / 10 000.
    #[test]
    fn a_record_share_target_sizes_each_sketch_for_one_over_share_groups() {
        let (candidates, five_percent) =
            per_series_kll(AccuracyScope::GroupsWithRecordShareAtLeast(0.05));
        for data in [with_series(None), data()] {
            let selection = select_plan(
                &candidates,
                &[entry_with(five_percent.clone(), repeating(10_000))],
                &data,
                PlanningModels::builtin(),
            )
            .expect("a KLL candidate meets the 5% target");
            assert_eq!(
                kll_failure_probabilities(&selection.plan),
                vec![Some(0.01 / 20.0)]
            );
        }
    }

    /// One summary read by queries over different scopes is sized for the
    /// strictest: every group (here `G` = 10 000 series) is stricter than
    /// groups holding at least 1% of the records (100 groups), which is
    /// stricter than 5% (20 groups), which is stricter than one value.
    #[test]
    fn a_shared_summary_is_sized_for_its_strictest_scope() {
        let (candidates, per_value) = per_series_kll(AccuracyScope::PerValue);
        let root = candidates[0].roots[0].clone();
        let roots = [root.clone(), root];
        let scoped = |scope| per_value.clone().accuracy_scope(scope);
        let one_percent = scoped(AccuracyScope::GroupsWithRecordShareAtLeast(0.01));
        let five_percent = scoped(AccuracyScope::GroupsWithRecordShareAtLeast(0.05));
        let every_group = scoped(AccuracyScope::WholeResult);
        for (pair, delta) in [
            ([per_value.clone(), five_percent.clone()], 0.01 / 20.0),
            ([five_percent.clone(), one_percent.clone()], 0.01 / 100.0),
            ([one_percent, every_group], 0.01 / 10_000.0),
        ] {
            for targets in [pair.clone(), [pair[1].clone(), pair[0].clone()]] {
                let resized = resize_for_failure_probability(&roots, &targets, &data()).unwrap();
                assert_eq!(
                    kll_failure_probabilities(&resized),
                    vec![Some(delta); 2],
                    "{targets:?}"
                );
            }
        }
    }

    /// Without a bound on the values one evaluation returns, a per-value
    /// guarantee does not meet a whole-result target: a PromQL vector whose
    /// source declares no series count, and a SQL relation (until a bound on
    /// its groups is an input). A scalar returns one value.
    #[test]
    fn an_unbound_number_of_values_leaves_a_whole_result_target_unmet() {
        let (candidates, whole) = per_series_kll(AccuracyScope::WholeResult);
        let no_series_count = with_series(None);
        let Err(SelectionError::NoValidCandidate(rejected)) = select_plan(
            &candidates,
            &[entry_with(whole, repeating(10_000))],
            &no_series_count,
            PlanningModels::builtin(),
        ) else {
            panic!("the KLL candidates are invalid")
        };
        assert!(
            rejected[0].reason.contains("is not bound"),
            "{}",
            rejected[0].reason
        );
        let table = asap_logical_asap_optimization::test_support::scan_from(
            asap_types::ir::operator::Source::Table {
                table_ref: "t".into(),
            },
            Schema::new(vec![asap_types::ir::schema::Field::plain(
                "v",
                DataType::Float64,
                false,
            )]),
        );
        assert_eq!(values_of(&QueryRoot::Operator(table), &data()), None);
        assert_eq!(
            values_of(
                &QueryRoot::Scalar(ScalarExpr::literal_f64(1.0)),
                &DataWorkload::default()
            ),
            Some(1)
        );
    }

    /// A root that returns at most one row has `G` = 1, provable from the
    /// query with no data workload: a global aggregate, PromQL or SQL, and
    /// what selects, renames or joins single rows above it. A grouped SQL
    /// aggregate stays unbound, and a grouped PromQL one, or a global top-k
    /// (k rows), is bound by the series count of the source it reads.
    #[test]
    fn a_root_of_at_most_one_row_returns_one_value() {
        let none = DataWorkload::default();
        let root =
            |query: &str| QueryRoot::Operator(lower_promql(query, AccuracyTarget::max_error(0.01)));
        assert_eq!(values_of(&root("sum(rate(x[5m]))"), &none), Some(1));
        assert_eq!(values_of(&root("topk(5, rate(x[5m]))"), &none), None);
        assert_eq!(values_of(&root("sum by (job) (rate(x[5m]))"), &none), None);
        assert_eq!(
            values_of(&root("sum by (job) (rate(x[5m]))"), &data()),
            Some(10_000)
        );
        let table = || {
            asap_logical_asap_optimization::test_support::scan_from(
                asap_types::ir::operator::Source::Table {
                    table_ref: "t".into(),
                },
                Schema::new(vec![
                    asap_types::ir::schema::Field::plain("k", DataType::Utf8, false),
                    asap_types::ir::schema::Field::plain("v", DataType::Float64, false),
                ]),
            )
        };
        let count = || AggIntent::Count {
            accuracy: AccuracyTarget::exact(),
        };
        let global = asap_logical_asap_optimization::test_support::agg(vec![], count(), table());
        let grouped = asap_logical_asap_optimization::test_support::agg(vec![0], count(), table());
        assert_eq!(
            values_of(&QueryRoot::Operator(global.clone()), &none),
            Some(1)
        );
        assert_eq!(values_of(&QueryRoot::Operator(grouped), &data()), None);
        let cross = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Join {
            kind: asap_types::ir::operator::operator_properties::JoinKind::Cross,
            pred: asap_types::ir::Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
            left: global.clone(),
            right: global,
        }))
        .unwrap();
        assert_eq!(values_of(&QueryRoot::Operator(cross), &none), Some(1));
    }

    /// `G` and the input row count are bound per query from the sources
    /// it reads: another source's series do not count, a query over two
    /// sources is bound by their sum, and one source without a series
    /// count leaves it unbound. Pricing reads the same per-source series, with
    /// the default for an undeclared source.
    #[test]
    fn values_per_result_is_bound_by_the_series_of_the_sources_read() {
        let root =
            |query: &str| QueryRoot::Operator(lower_promql(query, AccuracyTarget::max_error(0.01)));
        let series = |declared: &[(&str, u64)]| DataWorkload {
            sources: declared
                .iter()
                .map(|(source, series)| {
                    let facts = SourceFacts {
                        series_count: Some(*series),
                        ..SourceFacts::default()
                    };
                    (source.to_string(), facts)
                })
                .collect(),
            ..DataWorkload::default()
        };
        let x = root("sum by (job) (rate(x[5m]))");
        let both = root("sum by (job) (rate(x[5m])) + sum by (job) (rate(m[5m]))");
        assert_eq!(values_of(&x, &series(&[("x", 10)])), Some(10));
        assert_eq!(values_of(&x, &series(&[("m", 10)])), None);
        let declared = series(&[("x", 10), ("m", 20)]);
        assert_eq!(values_of(&x, &declared), Some(10));
        assert_eq!(values_of(&both, &declared), Some(30));
        assert_eq!(series_read(&both, &declared), Some(30));
        assert_eq!(values_of(&both, &series(&[("x", 10)])), None);
        // Pricing sums the series of the sources a node reads; an
        // undeclared source, or none, leaves them unknown.
        assert_eq!(declared_series(&["x", "m"].into(), &declared), Some(30));
        assert_eq!(declared_series(&["x", "y"].into(), &declared), None);
        assert_eq!(declared_series(&BTreeSet::new(), &declared), None);
    }

    /// A source's rows per ms: its series over its sample interval when it
    /// declares both, else its series share of the workload's rate.
    #[test]
    fn each_source_arrives_at_its_own_rate() {
        let data = DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 4_000.0,
            },
            sources: [("a", Some(100)), ("b", Some(300)), ("c", None)]
                .into_iter()
                .map(|(metric, interval)| {
                    let facts = SourceFacts {
                        series_count: Some(if metric == "a" { 1_000 } else { 3_000 }),
                        sample_interval: interval.map(asap_types::workload::DurationMs),
                        ..SourceFacts::default()
                    };
                    (metric.to_string(), facts)
                })
                .collect(),
        };
        // 1 000 series every 100 ms; 3 000 series every 300 ms.
        assert_eq!(source_rows_per_ms(&["a"].into(), &data), Some(10.0));
        assert_eq!(source_rows_per_ms(&["a", "b"].into(), &data), Some(20.0));
        // No interval: 3 000 of the 7 000 declared series, of 4 rows per ms.
        assert_eq!(
            source_rows_per_ms(&["c"].into(), &data),
            Some(4.0 * 3_000.0 / 7_000.0)
        );
        // A node reading no source sees the whole rate.
        assert_eq!(source_rows_per_ms(&BTreeSet::new(), &data), Some(4.0));
    }

    /// The exact physical DAG of `queries` planned together, over `data`.
    fn exact_dag(queries: &[&str], data: &DataWorkload) -> PhysicalASAPDAG {
        let roots = queries
            .iter()
            .enumerate()
            .map(|(index, query)| {
                let root = lower_promql(query, AccuracyTarget::exact());
                (index, QueryRoot::Operator(root))
            })
            .collect();
        let inventory = asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates(roots)
            .unwrap();
        let choice = vec![0; inventory.targets.len()];
        let roots: Vec<_> =
            asap_logical_asap_optimization::pass1::per_computation_candidates::compose_logical_candidate(
                &inventory, &choice,
            )
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        optimize_physical_asap("L1", &roots, &[], data, false)
            .unwrap()
            .remove(0)
            .dag
    }

    /// A node's operator, timing, work and price, to compare nodes across
    /// DAGs whose ids differ.
    fn node_prices(cost: &CandidateCost) -> Vec<(OperatorKind, ExecutionTiming, String, f64)> {
        cost.per_node
            .values()
            .map(|node| {
                (
                    node.operator,
                    node.timing,
                    node.detail.clone(),
                    node.cost_per_second.unwrap(),
                )
            })
            .collect()
    }

    /// No fact bounds the rows of a scan whose source declares neither an
    /// arrival rate (`rows_per_second`, or `series_count` with
    /// `sample_interval`) nor `rows_held`: pricing refuses, naming the
    /// source and the missing fact, instead of assuming a rate.
    #[test]
    fn a_scan_without_a_row_fact_is_refused_naming_its_source() {
        let data = DataWorkload::default();
        let error = price(
            &exact_dag(&["sum(rate(m[5m]))"], &data),
            &recurrences(1),
            &data,
            &CostInputs::default(),
        )
        .unwrap_err()
        .1;
        assert!(
            matches!(&error, AnalyticalCostError::MissingSourceFact { source_name, .. } if source_name == "m"),
            "{error}"
        );
        assert!(error.to_string().contains("rows_held"), "{error}");
    }

    // #955 example 1: `rate(m[5m] offset 1h)` at query time reads the 5
    // minutes `(T - 65m, T - 60m]`, so it scans the samples `rate(m[5m])`
    // scans but keeps and prices the 65 minutes of raw `m` `rate(m[65m])`
    // keeps.
    #[test]
    fn an_offset_window_at_query_time_keeps_its_whole_raw_lookback() {
        // The scan's samples read and raw data bytes kept, from its detail.
        let scan_of = |query: &str| {
            let dag = exact_dag(&[query], &data());
            let scan = node_of(&dag, is_scan);
            let cost = price(&dag, &recurrences(1), &data(), &CostInputs::default()).unwrap();
            let detail = &cost.per_node[&scan].detail;
            let kept_bytes: u64 = detail
                .split("; keeps ")
                .find_map(|part| {
                    part.split(" bytes of raw data on disk")
                        .next()?
                        .parse()
                        .ok()
                })
                .unwrap_or_else(|| panic!("no raw data kept: {detail}"));
            (scanned_samples(&cost, scan), kept_bytes)
        };
        let (offset_samples, offset_kept) = scan_of("sum(rate(m[5m] offset 1h))");
        let (five_minutes_samples, five_minutes_kept) = scan_of("sum(rate(m[5m]))");
        let (_, sixty_five_minutes_kept) = scan_of("sum(rate(m[65m]))");
        assert_eq!(offset_samples, five_minutes_samples);
        assert_eq!(offset_kept, sixty_five_minutes_kept);
        assert_eq!(offset_kept, 13 * five_minutes_kept);
    }

    /// Data at rest is bounded by the rows its source holds: a 5-minute
    /// window of `m`, which holds 5,000 rows and declares no arrival rate,
    /// reads at most those 5,000.
    #[test]
    fn rows_held_bound_a_scan_of_data_at_rest() {
        let data = DataWorkload {
            arrival: DataArrival::AtRest,
            sources: [(
                "m".to_string(),
                SourceFacts {
                    rows_held: Some(5_000),
                    ..SourceFacts::default()
                },
            )]
            .into(),
        };
        let cost = price(
            &exact_dag(&["sum(rate(m[5m]))"], &data),
            &recurrences(1),
            &data,
            &CostInputs::default(),
        )
        .unwrap();
        let details: Vec<String> = node_prices(&cost)
            .into_iter()
            .map(|(_, _, d, _)| d)
            .collect();
        assert!(
            details.iter().any(|d| d.starts_with("scan 5000 samples;")),
            "{details:?}"
        );
    }

    /// With no declared `series_count`, a per-series reduction is priced at
    /// one state per row read (each series has at least one), not at an
    /// assumed series count: 1,000 rows per second over 5 minutes.
    #[test]
    fn undeclared_series_are_bound_by_the_rows_read() {
        let data = DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 1_000.0,
            },
            sources: [("m".to_string(), SourceFacts::default())].into(),
        };
        let cost = price(
            &exact_dag(&["sum(rate(m[5m]))"], &data),
            &recurrences(1),
            &data,
            &CostInputs::default(),
        )
        .unwrap();
        let details: Vec<String> = node_prices(&cost)
            .into_iter()
            .map(|(_, _, d, _)| d)
            .collect();
        assert!(
            details.iter().any(|d| d.contains("into 300000 states")),
            "{details:?}"
        );
    }

    /// Each scan is priced at its own source's rate, `series_count` series
    /// each sampled every `sample_interval`, not at the whole workload's
    /// `rows_per_second`: adding a query on a second metric, which a
    /// runtime declares by raising `rows_per_second` by that metric's rate,
    /// leaves the first metric's node prices unchanged.
    #[test]
    fn a_query_on_another_metric_leaves_a_metrics_node_prices_unchanged() {
        // `a` and `b`: 10 000 series sampled every 100 ms, 100 000 rows/s each.
        let ingesting = |rows_per_second| DataWorkload {
            arrival: DataArrival::Ingesting { rows_per_second },
            sources: ["a", "b"]
                .into_iter()
                .map(|metric| {
                    let facts = SourceFacts {
                        series_count: Some(10_000),
                        sample_interval: Some(asap_types::workload::DurationMs(100)),
                        ..SourceFacts::default()
                    };
                    (metric.to_string(), facts)
                })
                .collect(),
        };
        let settings = CostInputs::default();
        let on_a = "sum(sum_over_time(a[1m]))";
        let alone = ingesting(100_000.0);
        let alone = price(
            &exact_dag(&[on_a], &alone),
            &recurrences(1),
            &alone,
            &settings,
        )
        .unwrap();
        let joint = ingesting(200_000.0);
        let joint = price(
            &exact_dag(&[on_a, "sum(sum_over_time(b[1m]))"], &joint),
            &recurrences(2),
            &joint,
            &settings,
        )
        .unwrap();
        let mut joint_nodes = node_prices(&joint);
        for node in node_prices(&alone) {
            let found = joint_nodes.iter().position(|other| *other == node);
            assert!(found.is_some(), "{node:?} not in {joint_nodes:#?}");
            joint_nodes.remove(found.unwrap());
        }
        // Each metric's scan reads its own 100 000 rows per second.
        assert_eq!(
            joint.usage.scanned_bytes_per_second,
            2.0 * alone.usage.scanned_bytes_per_second
        );
    }

    /// A partitioned sort and limit (`topk by (job)`) take their partition
    /// count from `job`'s declared distinct values. With `job` undeclared,
    /// no count is invented: each series is in one partition and each row
    /// read in at most one, so the partitions are at most the series and
    /// at most the rows the sort reads.
    #[test]
    fn a_partitioned_topk_has_one_partition_per_declared_label_value() {
        let details = |declared: &[(&str, u64)], series: u64| {
            let mut data = with_series(Some(series));
            data.sources.get_mut("m").unwrap().distinct_values_per_label = declared
                .iter()
                .map(|(label, count)| (label.to_string(), *count))
                .collect();
            let cost = price(
                &exact_dag(&["topk by (job) (3, m)"], &data),
                &recurrences(1),
                &data,
                &CostInputs::default(),
            )
            .unwrap();
            node_prices(&cost)
                .into_iter()
                .filter(|(operator, ..)| {
                    matches!(operator, OperatorKind::Sort | OperatorKind::Limit)
                })
                .map(|(_, _, detail, _)| detail)
                .collect::<Vec<_>>()
        };
        let declared = details(&[("job", 10)], 10_000);
        assert!(
            declared.iter().any(|d| d.contains(" in 10 partitions;")),
            "{declared:?}"
        );
        assert!(
            declared.iter().any(|d| d.starts_with("limit to 30 rows;")),
            "{declared:?}"
        );
        // Undeclared: the 2,000 rows read, fewer than the 10,000 series.
        let undeclared = details(&[("instance", 10)], 10_000);
        assert!(
            undeclared
                .iter()
                .any(|d| d.starts_with("sort 2000 rows in 2000 partitions;")),
            "{undeclared:?}"
        );
        assert!(
            undeclared
                .iter()
                .any(|d| d.starts_with("limit to 2000 rows;")),
            "{undeclared:?}"
        );
        // 50 series hold at most 50 groups, declared or not.
        let few = details(&[("job", 10_000)], 50);
        assert!(
            few.iter().any(|d| d.contains(" in 50 partitions;")),
            "{few:?}"
        );
        let few = details(&[], 50);
        assert!(
            few.iter().any(|d| d.contains(" in 50 partitions;")),
            "{few:?}"
        );
    }

    /// `x` holding 10,000 series over 4 regions, 25 services and 625
    /// endpoints, each endpoint in one service (the eval's RollupHeavy
    /// `http`), with `(service, endpoint)`'s 625 combinations declared when
    /// `declared_set`.
    fn hierarchical_labels(declared_set: bool) -> DataWorkload {
        let mut data = with_series(Some(10_000));
        let facts = data.sources.get_mut("x").unwrap();
        facts.distinct_values_per_label = [
            ("region".to_string(), 4),
            ("service".to_string(), 25),
            ("endpoint".to_string(), 625),
        ]
        .into();
        if declared_set {
            facts.distinct_values_per_label_set = vec![LabelSetDistinctValues {
                labels: vec!["service".to_string(), "endpoint".to_string()],
                distinct_values: 625,
            }];
        }
        data
    }

    /// A grouping is priced at the smallest bound the declared facts give:
    /// the declared `(service, endpoint)` set's 625 combinations, not
    /// 25 x 625 capped at the 10,000 series; a superset grouping at the set
    /// times `region`'s 4; without the set, at the per-label product capped
    /// at the series. A partitioned top-k takes the same count.
    #[test]
    fn a_grouping_is_priced_at_its_declared_label_set_count() {
        let details = |query: &str, declared_set: bool| {
            let data = hierarchical_labels(declared_set);
            let cost = price(
                &exact_dag(&[query], &data),
                &recurrences(1),
                &data,
                &CostInputs::default(),
            )
            .unwrap();
            node_prices(&cost)
                .into_iter()
                .map(|(_, _, detail, _)| detail)
                .collect::<Vec<_>>()
        };
        let sum = |keys: &str| format!("sum by ({keys}) (sum_over_time(x[15m]))");
        for (query, declared_set, expected) in [
            (sum("service, endpoint"), true, "into 625 states"),
            (sum("region, service, endpoint"), true, "into 2500 states"),
            (
                sum("service, endpoint"),
                false,
                "Sum accumulator into 10000 states",
            ),
            (sum("region, service"), true, "into 100 states"),
            (
                "topk by (service, endpoint) (3, sum_over_time(x[15m]))".to_string(),
                true,
                " in 625 partitions;",
            ),
            // A relabeled key no longer holds the declared set's values:
            // `endpoint` now joins `region` and `endpoint` (up to 4 x 625).
            (
                r#"sum by (service, endpoint) (label_join(sum_over_time(x[15m]), "endpoint", "-", "region", "endpoint"))"#.to_string(),
                true,
                "into 10000 states",
            ),
            // A binary operator pairs the labels of two rows: per-label only.
            (
                "sum by (service, endpoint) (sum_over_time(x[15m]) / on (service) group_left (endpoint) sum_over_time(x[15m]))".to_string(),
                true,
                "into 10000 states",
            ),
        ] {
            let details = details(&query, declared_set);
            assert!(
                details.iter().any(|d| d.contains(expected)),
                "{query} (set declared: {declared_set}): {details:#?}"
            );
        }
    }

    /// A grouped root's `G` takes the declared label set's count too: the
    /// set's 625 for `(service, endpoint)`, times `region`'s 4 for a
    /// superset; without the set, the per-label product capped at the
    /// series.
    #[test]
    fn values_per_result_is_bound_by_a_declared_label_set() {
        let root =
            |query: &str| QueryRoot::Operator(lower_promql(query, AccuracyTarget::max_error(0.01)));
        for (query, declared_set, values) in [
            ("sum by (service, endpoint) (rate(x[5m]))", true, 625),
            (
                "sum by (region, service, endpoint) (rate(x[5m]))",
                true,
                2_500,
            ),
            ("sum by (service, endpoint) (rate(x[5m]))", false, 10_000),
        ] {
            assert_eq!(
                values_of(&root(query), &hierarchical_labels(declared_set)),
                Some(values),
                "{query} (set declared: {declared_set})"
            );
        }
    }

    /// A grouped root's `G` is also bound by its keys' declared distinct
    /// values, at most the series read: `by (job)` with 10 jobs returns at
    /// most 10 values, also below a sort; a key without a declared count, a
    /// relabeled key, `without (...)` and a top-k (several rows per group)
    /// keep the series bound.
    #[test]
    fn values_per_result_is_bound_by_the_declared_distinct_values_of_its_keys() {
        let root =
            |query: &str| QueryRoot::Operator(lower_promql(query, AccuracyTarget::max_error(0.01)));
        let mut data = with_series(Some(10_000));
        data.sources.get_mut("x").unwrap().distinct_values_per_label =
            [("job".to_string(), 10), ("instance".to_string(), 5_000)].into();
        for (query, values) in [
            ("sum by (job) (rate(x[5m]))", 10),
            ("sum by (job, instance) (rate(x[5m]))", 10_000),
            ("sum by (job, region) (rate(x[5m]))", 10_000),
            ("sum without (instance) (rate(x[5m]))", 10_000),
            ("topk by (job) (3, rate(x[5m]))", 10_000),
            ("sum by (job) (rate(x[5m]) + rate(m[5m]))", 20_000),
            // A relabeled key no longer holds the declared label's values.
            (
                r#"sum by (job) (label_replace(rate(x[5m]), "job", "$1", "instance", "(.*)"))"#,
                10_000,
            ),
            ("sort_desc(sum by (job) (rate(x[5m])))", 10),
            // `count_values("job", ...)` overwrites `job` with sample values.
            (
                r#"sum by (job) (count_values by (job) ("job", rate(x[5m])))"#,
                10_000,
            ),
        ] {
            assert_eq!(values_of(&root(query), &data), Some(values), "{query}");
        }
    }

    /// `G` counts the values the root can return: an `or` returns the
    /// values of both branches (here up to `series(x)` series plus the
    /// groups of `x`), a constant vector one more, while a join or a fold
    /// of one source counts it once.
    #[test]
    fn values_per_result_adds_the_branches_of_an_or() {
        let root =
            |query: &str| QueryRoot::Operator(lower_promql(query, AccuracyTarget::max_error(0.01)));
        let data = with_series(Some(10));
        for (query, values) in [
            ("rate(x[5m]) or sum without (instance) (rate(x[5m]))", 20),
            ("rate(x[5m]) or vector(0)", 11),
            ("rate(x[5m]) or rate(m[5m])", 20),
            ("(rate(x[5m]) or vector(0)) or vector(1)", 12),
            ("sum by (job) (rate(x[5m]) or rate(x[5m]))", 20),
            ("rate(x[5m]) / rate(x[5m])", 10),
            ("sum without (instance) (rate(x[5m])) * rate(x[5m])", 10),
            ("rate(x[5m]) + rate(m[5m])", 20),
        ] {
            assert_eq!(values_of(&root(query), &data), Some(values), "{query}");
        }
    }

    /// `vector(s)` returns one row, but the operators below it still read
    /// their sources' rows: the input row count of
    /// `vector(scalar(max(rate(x[5m]))))` counts the 10 series the `max`
    /// folds (plus the one constant row), never shrinking going up the tree,
    /// while its `G` is 1.
    #[test]
    fn a_vector_of_a_scalar_keeps_the_rows_read_below_it() {
        let root =
            |query: &str| QueryRoot::Operator(lower_promql(query, AccuracyTarget::max_error(0.01)));
        let data = with_series(Some(10));
        let vector = root("vector(scalar(max(rate(x[5m]))))");
        assert_eq!(series_read(&vector, &data), Some(11));
        assert_eq!(values_of(&vector, &data), Some(1));
        assert_eq!(values_of(&root("vector(1)"), &data), Some(1));
        let or = root("rate(x[5m]) or vector(scalar(max(rate(x[5m]))))");
        assert_eq!(series_read(&or, &data), Some(21));
    }

    /// Deployment evidence that every output was measured exact.
    struct EveryOutputMeasuredExact;

    impl AccuracyEvidenceProvider for EveryOutputMeasuredExact {
        fn output_guarantee(&self, _output: &QueryRoot) -> Option<AccuracyGuarantee> {
            Some(AccuracyGuarantee::exact("measured"))
        }
    }

    /// Deployment evidence of a small error in a fixed metric for every
    /// output that is not exact.
    struct EveryOutputMeasuredIn(AccuracyMetric);

    impl AccuracyEvidenceProvider for EveryOutputMeasuredIn {
        fn output_guarantee(&self, _output: &QueryRoot) -> Option<AccuracyGuarantee> {
            Some(AccuracyGuarantee::new(
                self.0,
                BoundExpr::Constant { value: 1e-6 },
                ProbabilityExpr::Constant { value: 1e-6 },
                vec![],
            ))
        }
    }

    /// A bound stated in another metric misses the target however small it
    /// is: a relative bound does not bound a top-k frequency's L1-scaled
    /// error, so only the exact candidate stays valid; the same evidence in
    /// the target's metric admits the sketches.
    #[test]
    fn a_bound_in_another_metric_misses_the_target() {
        let counter = with_metric_types(data(), &[("m", MetricType::Counter)]);
        let target = AccuracyTarget::max_error(0.5)
            .failure_probability(0.5)
            .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L1);
        let select = |metric| {
            select_plan(
                &candidates(),
                &[entry_with(target.clone(), repeating(10_000))],
                &counter,
                PlanningModels::builtin().with_evidence(&EveryOutputMeasuredIn(metric)),
            )
            .unwrap()
        };
        let relative = select(AccuracyMetric::Relative);
        assert_eq!(relative.selected, "P1");
        let mut invalid: Vec<_> = relative
            .rejected
            .iter()
            .filter(|r| r.cause.failed_a_check())
            .collect();
        invalid.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            invalid.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["P2", "P3"]
        );
        for rejected in invalid {
            assert!(
                rejected.reason.contains("no evidence that it meets"),
                "{}",
                rejected.reason
            );
        }
        let l1 = select(AccuracyMetric::ABSOLUTE_STREAM_L1);
        assert!(
            l1.rejected.iter().all(|r| !r.cause.failed_a_check()),
            "{:?}",
            l1.rejected
        );
    }

    /// A difference of two quantile estimates has no output guarantee, though
    /// each estimate is certified: its summary candidates stay listed as
    /// invalid with a reason. The deployment's empirical accuracy for the
    /// output admits them.
    #[test]
    fn combined_estimates_need_evidence_about_the_output() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let candidates = all_candidates(
            "quantile_over_time(0.99, m[1m]) - quantile_over_time(0.5, m[1m])",
            &target,
        );
        let summaries: Vec<_> = candidates
            .iter()
            .filter(|c| has_summary_estimate(c))
            .map(|c| c.id.as_str())
            .collect();
        assert!(!summaries.is_empty());

        let builtin = select_plan(
            &candidates,
            &[entry_with(
                target.clone().accuracy_metric(AccuracyMetric::ABSOLUTE),
                repeating(10_000),
            )],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        for id in &summaries {
            let rejection = builtin.rejected.iter().find(|r| r.id == *id).unwrap();
            assert_eq!(rejection.cause, RejectionCause::NoAccuracyGuarantee);
            assert!(
                rejection.reason.contains("no guarantee"),
                "{}",
                rejection.reason
            );
        }
        assert!(!summaries.contains(&builtin.selected.as_str()));

        let measured = select_plan(
            &candidates,
            &[entry_with(
                target.clone().accuracy_metric(AccuracyMetric::ABSOLUTE),
                repeating(10_000),
            )],
            &data(),
            PlanningModels::builtin().with_evidence(&EveryOutputMeasuredExact),
        )
        .unwrap();
        for id in &summaries {
            assert!(measured.costs.contains_key(*id), "{id} is valid");
        }
    }

    /// Deployment evidence of a large absolute error (1.0) for every output.
    struct EveryOutputMeasuredLoose;

    impl AccuracyEvidenceProvider for EveryOutputMeasuredLoose {
        fn output_guarantee(&self, _output: &QueryRoot) -> Option<AccuracyGuarantee> {
            Some(AccuracyGuarantee::new(
                AccuracyMetric::ABSOLUTE,
                BoundExpr::Constant { value: 1.0 },
                ProbabilityExpr::Constant { value: 1e-6 },
                vec![],
            ))
        }
    }

    /// A model that accepts every output directly, whatever its guarantee.
    struct AcceptsEveryOutput;

    impl AccuracyModel for AcceptsEveryOutput {
        fn satisfies(&self, guarantee: &AccuracyGuarantee, target: &AccuracyTarget) -> bool {
            BuiltinAccuracyModel.satisfies(guarantee, target)
        }

        fn accepts_output_regardless_of_guarantee(
            &self,
            _output: &QueryRoot,
            _target: &AccuracyTarget,
        ) -> bool {
            true
        }
    }

    /// The hook overrides guarantees, not the executor's requirement on
    /// Count-Min update weights: over samples of a metric declared a gauge,
    /// the CmsWithHeap candidate (P2) is rejected as
    /// `CountMinWeightsNotProvenNonNegative` under a model that accepts
    /// every output, as under the built-in one.
    #[test]
    fn unproven_count_min_weights_reject_even_an_accepting_model() {
        let target = AccuracyTarget::max_error(0.01)
            .failure_probability(0.001)
            .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L1);
        let gauge = with_metric_types(data(), &[("m", MetricType::Gauge)]);
        for accuracy in [
            &AcceptsEveryOutput as &dyn AccuracyModel,
            &BuiltinAccuracyModel,
        ] {
            let selection = select_plan(
                &candidates(),
                &[entry_with(target.clone(), repeating(10_000))],
                &gauge,
                PlanningModels::builtin().with_accuracy(accuracy),
            )
            .unwrap();
            let p2 = selection.rejected.iter().find(|r| r.id == "P2").unwrap();
            assert_eq!(
                p2.cause,
                RejectionCause::CountMinWeightsNotProvenNonNegative,
                "{}",
                p2.reason
            );
            assert!(
                p2.reason.contains("not declared a counter"),
                "{}",
                p2.reason
            );
            assert!(p2.cause.failed_a_check());
        }
    }

    /// `ignore_accuracy_guarantees` (issue #774) does not override the
    /// executor's requirement on Count-Min update weights either: over
    /// samples of a gauge, P2's CmsWithHeap is still rejected as
    /// `CountMinWeightsNotProvenNonNegative`.
    #[test]
    fn unproven_count_min_weights_reject_even_when_ignoring_guarantees() {
        let target = AccuracyTarget::max_error(0.01)
            .failure_probability(0.001)
            .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L1);
        let gauge = with_metric_types(data(), &[("m", MetricType::Gauge)]);
        let settings = CostInputs {
            ignore_accuracy_guarantees: true,
            ..CostInputs::default()
        };
        let selection = select_plan(
            &candidates(),
            &[entry_with(target, repeating(10_000))],
            &gauge,
            PlanningModels::builtin().with_plan_selection(&settings),
        )
        .unwrap();
        let p2 = selection.rejected.iter().find(|r| r.id == "P2").unwrap();
        assert_eq!(
            p2.cause,
            RejectionCause::CountMinWeightsNotProvenNonNegative,
            "{}",
            p2.reason
        );
    }

    /// With `ignore_accuracy_guarantees` (issue #774), each summary
    /// candidate the built-in model rejects (no guarantee, or one that
    /// misses the target) is valid, and its accepted output records the
    /// cause the guarantee check would have rejected it with; an output
    /// whose guarantee meets the target records none.
    #[test]
    fn ignoring_guarantees_accepts_and_marks_outputs_the_check_rejects() {
        let target = AccuracyTarget::max_error(0.01)
            .failure_probability(0.001)
            .accuracy_metric(AccuracyMetric::ABSOLUTE);
        let entries = [entry_with(target, repeating(10_000))];
        let settings = CostInputs {
            ignore_accuracy_guarantees: true,
            ..CostInputs::default()
        };
        let builtin =
            select_plan(&candidates(), &entries, &data(), PlanningModels::builtin()).unwrap();
        let ignoring = select_plan(
            &candidates(),
            &entries,
            &data(),
            PlanningModels::builtin().with_plan_selection(&settings),
        )
        .unwrap();
        let mut marked = 0;
        for rejection in builtin.rejected.iter().filter(|r| {
            matches!(
                r.cause,
                RejectionCause::NoAccuracyGuarantee | RejectionCause::AccuracyTargetMissed
            )
        }) {
            let [accepted] = ignoring.accepted[&rejection.id].as_slice() else {
                panic!("{} has one query", rejection.id)
            };
            assert!(accepted.accepted_regardless_of_guarantee);
            assert_eq!(accepted.guarantee_check_rejection, Some(rejection.cause));
            assert!(accepted.to_string().contains("ignoring"), "{accepted}");
            marked += 1;
        }
        assert!(marked > 0, "{:?}", builtin.rejected);
        for (id, queries) in &builtin.accepted {
            assert_eq!(
                ignoring.accepted[id][0].guarantee_check_rejection, None,
                "{id}"
            );
            assert_eq!(queries[0].guarantee_check_rejection, None, "{id}");
        }
    }

    /// `node` with every CmsWithHeap summary build replaced by a Hydra
    /// Count-Min over the same update.
    fn with_hydra_count_min(node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        use asap_types::ir::schema::{HydraKind, HydraParams, SketchParams};
        let mut rebuilt = node.with_new_children(with_hydra_count_min).unwrap();
        if let Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) = &mut rebuilt.operator {
            if let FieldDataType::Sketch(kind, _) = family {
                if let SketchParams::CmsWithHeap { width, depth, .. } = kind.params() {
                    let (width, depth) = (*width, *depth);
                    *family = FieldDataType::Sketch(
                        SketchKind::new(SketchAlgorithm::Cms, SketchParams::Cms { width, depth }),
                        GroupingStrategy::SharedMultiSubpopulation {
                            kind: HydraKind::HydraCms,
                            params: HydraParams::HydraCms {
                                width,
                                depth,
                                shared_rows: depth,
                                shared_columns: width,
                            },
                        },
                    );
                }
            }
        }
        Rc::new(rebuilt)
    }

    /// A Hydra Count-Min over unproven weights is rejected by its own name:
    /// the reason says `HydraCms`, not `Cms`.
    #[test]
    fn unproven_hydra_count_min_weights_name_hydra_cms() {
        let target = AccuracyTarget::max_error(0.01)
            .failure_probability(0.001)
            .accuracy_metric(AccuracyMetric::ABSOLUTE_STREAM_L1);
        let gauge = with_metric_types(data(), &[("m", MetricType::Gauge)]);
        let mut p2 = candidates().into_iter().find(|c| c.id == "P2").unwrap();
        p2.roots = p2
            .roots
            .iter()
            .map(|root| match root {
                QueryRoot::Operator(node) => QueryRoot::Operator(with_hydra_count_min(node)),
                other => other.clone(),
            })
            .collect();
        let CheckFailure { cause, reason, .. } =
            accuracy_check(&p2, &[target], &gauge, &PlanningModels::builtin()).unwrap_err();
        assert_eq!(cause, RejectionCause::CountMinWeightsNotProvenNonNegative);
        assert!(
            reason.contains("HydraCms needs non-negative update weights"),
            "{reason}"
        );
        assert!(reason.contains("not declared a counter"), "{reason}");
    }

    /// The ids of `candidates` whose DAG reduces raw data samples per series with
    /// an exact `Aggregate`.
    fn with_per_entity_aggregate(candidates: &[PhysicalWorkloadPlan]) -> Vec<String> {
        candidates
            .iter()
            .filter(|c| {
                c.dag.nodes.iter().any(|node| {
                    matches!(
                        node.payload,
                        Payload::NonASAP(NonASAPOp::Aggregate {
                            reduction: Reduction::PerEntity,
                            ..
                        })
                    )
                })
            })
            .map(|c| c.id.clone())
            .collect()
    }

    /// The executor has no exact per-series `stddev_over_time`: each
    /// candidate that reduces raw data samples with one is rejected as
    /// `PerEntityAggregateNotExecutable` and never selected (issue #770).
    #[test]
    fn a_per_entity_aggregate_the_executor_cannot_run_is_rejected() {
        let target = AccuracyTarget::exact();
        let candidates = all_candidates("stddev_over_time(x[5m])", &target);
        let unsupported = with_per_entity_aggregate(&candidates);
        assert!(!unsupported.is_empty());
        let rejected = match select_plan(
            &candidates,
            &[entry_with(target, repeating(10_000))],
            &data(),
            PlanningModels::builtin(),
        ) {
            Ok(selection) => {
                assert!(!unsupported.contains(&selection.selected), "{selection:?}");
                selection.rejected
            }
            Err(SelectionError::NoValidCandidate(rejected)) => rejected,
            Err(error) => panic!("{error}"),
        };
        for id in &unsupported {
            let rejection = rejected.iter().find(|r| &r.id == id).unwrap();
            assert_eq!(
                rejection.cause,
                RejectionCause::PerEntityAggregateNotExecutable,
                "{}",
                rejection.reason
            );
            assert!(rejection.reason.contains("StdDev"), "{}", rejection.reason);
            assert!(rejection.cause.failed_a_check());
        }
    }

    /// An exact per-series `quantile_over_time`, which the executor runs, is
    /// not rejected as `PerEntityAggregateNotExecutable`.
    #[test]
    fn an_executable_per_entity_aggregate_is_not_rejected() {
        let target = AccuracyTarget::exact();
        let candidates = all_candidates("quantile_over_time(0.99, x[5m])", &target);
        assert!(!with_per_entity_aggregate(&candidates).is_empty());
        let selection = select_plan(
            &candidates,
            &[entry_with(target, repeating(10_000))],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        assert!(selection
            .rejected
            .iter()
            .all(|r| r.cause != RejectionCause::PerEntityAggregateNotExecutable));
    }

    /// The model's hook has priority over every guarantee. The summary
    /// candidates of a difference of quantile estimates have no theoretical
    /// guarantee: with evidence that misses the target the built-in model
    /// rejects them as `AccuracyTargetMissed`, and with no evidence as
    /// `NoAccuracyGuarantee`; a model that accepts every output accepts them
    /// in both cases, regardless of guarantee.
    #[test]
    fn the_accuracy_model_accepts_an_output_whatever_its_guarantee() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let candidates = all_candidates(
            "quantile_over_time(0.99, m[1m]) - quantile_over_time(0.5, m[1m])",
            &target,
        );
        let summaries: Vec<_> = candidates
            .iter()
            .filter(|c| has_summary_estimate(c))
            .map(|c| c.id.as_str())
            .collect();
        assert!(!summaries.is_empty());
        let select = |accuracy: &dyn AccuracyModel, evidence: &dyn AccuracyEvidenceProvider| {
            select_plan(
                &candidates,
                &[entry_with(
                    target.clone().accuracy_metric(AccuracyMetric::ABSOLUTE),
                    repeating(10_000),
                )],
                &data(),
                PlanningModels::builtin()
                    .with_accuracy(accuracy)
                    .with_evidence(evidence),
            )
            .unwrap()
        };
        for (evidence, builtin_cause) in [
            (
                &EveryOutputMeasuredLoose as &dyn AccuracyEvidenceProvider,
                RejectionCause::AccuracyTargetMissed,
            ),
            (&NoAccuracyEvidence, RejectionCause::NoAccuracyGuarantee),
        ] {
            let builtin = select(&BuiltinAccuracyModel, evidence);
            let accepting = select(&AcceptsEveryOutput, evidence);
            for id in &summaries {
                let rejection = builtin.rejected.iter().find(|r| r.id == *id).unwrap();
                assert_eq!(rejection.cause, builtin_cause, "{}", rejection.reason);
                assert!(accepting.costs.contains_key(*id), "{id} is valid");
                assert!(
                    accepting.accepted[*id][0].accepted_regardless_of_guarantee,
                    "{id}"
                );
            }
        }
    }

    /// `candidates` with every state node (a summary build or an exact
    /// aggregate) kept at ingestion time, so selection charges its kept
    /// bytes every second (a `MatAtQuery` state is held only for one
    /// evaluation).
    fn keeping_state(mut candidates: Vec<PhysicalWorkloadPlan>) -> Vec<PhysicalWorkloadPlan> {
        for candidate in &mut candidates {
            for node in &mut candidate.dag.nodes {
                if matches!(
                    node.payload,
                    Payload::ASAP(ASAPOp::SummaryAgg { .. })
                        | Payload::NonASAP(NonASAPOp::Aggregate { .. })
                ) {
                    node.materialization =
                        Materialization::MatAtIngestion(asap_types::physical::Medium::Memory);
                }
            }
        }
        candidates
    }

    /// Kept exact quantile state keeps every value of its group: over one
    /// group of 10 000 values it costs more than a quantile sketch sized for
    /// ε = 0.01, δ = 0.01, which is selected. Over 100 groups of 100 values
    /// (100 jobs declared), fewer than the sketch's `k`, the sketch keeps
    /// every value too, in an item array grown past them, so its state is
    /// no smaller than the exact state, which is selected.
    #[test]
    fn exact_quantile_state_costs_more_than_a_sketch_over_large_groups() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.01);
        let mut data = data();
        data.sources.get_mut("x").unwrap().distinct_values_per_label =
            [("job".to_string(), 100)].into();
        for (query, large_groups) in [
            ("quantile(0.99, rate(x[5m]))", true),
            ("quantile by (job) (0.99, rate(x[5m]))", false),
        ] {
            let candidates = keeping_state(all_candidates(query, &target));
            let selection = select_plan(
                &candidates,
                &[entry_with(
                    target.clone().accuracy_metric(AccuracyMetric::Rank),
                    repeating(10_000),
                )],
                &data,
                PlanningModels::builtin(),
            )
            .unwrap();
            let kept_bytes = |sketch: bool| {
                let candidate = candidates
                    .iter()
                    .find(|c| {
                        has_summary_estimate(c) == sketch && selection.costs.contains_key(&c.id)
                    })
                    .unwrap();
                selection.costs[&candidate.id].usage.average_memory_bytes
            };
            let selected = candidates
                .iter()
                .find(|c| c.id == selection.selected)
                .unwrap();
            match large_groups {
                true => {
                    assert!(has_summary_estimate(selected), "{query}: {selection:?}");
                    assert!(kept_bytes(true) < kept_bytes(false), "{query}");
                }
                false => {
                    assert!(!has_summary_estimate(selected), "{query}: {selection:?}");
                    assert!(kept_bytes(true) >= kept_bytes(false), "{query}");
                }
            }
        }
    }

    /// Exact L2 state keeps one counter per distinct value of its group, so
    /// it is priced like exact quantile state, which keeps every value.
    #[test]
    fn exact_l2_state_is_priced_like_exact_quantile_state() {
        let aggregate_cost = |query: &str| {
            let candidates = all_candidates(query, &AccuracyTarget::exact());
            let selection = select_plan(
                &candidates,
                &[entry(repeating(10_000))],
                &data(),
                PlanningModels::builtin(),
            )
            .unwrap();
            let exact = candidates
                .iter()
                .find(|c| !has_summary_estimate(c))
                .unwrap();
            selection.costs[&exact.id]
                .per_node
                .values()
                .find(|n| n.detail.starts_with("hash aggregate"))
                .unwrap()
                .cost_per_second
        };
        assert_eq!(
            aggregate_cost("l2_over_time(x[5m])"),
            aggregate_cost("quantile_over_time(0.99, x[5m])")
        );
    }

    /// Kept exact distinct-count state keeps every item of its group: over
    /// 10 000 groups of 300 rows it is smaller than a distinct-count sketch
    /// sized for ε = 0.01 and is selected; over one group of 3 000 000 rows a
    /// sketch is.
    #[test]
    fn exact_distinct_count_beats_a_sketch_over_small_groups() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.01);
        let one_series = with_series(Some(1));
        for (data, sketch_wins) in [(data(), false), (one_series, true)] {
            let candidates = keeping_state(all_candidates("distinct_over_time(x[5m])", &target));
            let selection = select_plan(
                &candidates,
                &[entry_with(
                    target.clone().accuracy_metric(AccuracyMetric::Relative),
                    repeating(10_000),
                )],
                &data,
                PlanningModels::builtin(),
            )
            .unwrap();
            let selected = candidates
                .iter()
                .find(|c| c.id == selection.selected)
                .unwrap();
            assert_eq!(has_summary_estimate(selected), sketch_wins, "{selection:?}");
        }
    }

    /// The candidates of `query` that build a DDSketch.
    fn ddsketch_candidates(candidates: &[PhysicalWorkloadPlan]) -> Vec<String> {
        candidates
            .iter()
            .filter(|c| {
                c.dag.nodes.iter().any(|node| {
                    matches!(&node.payload,
                        Payload::ASAP(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. })
                            if kind.algorithm() == &SketchAlgorithm::DDSketch)
                })
            })
            .map(|c| c.id.clone())
            .collect()
    }

    /// A per-series DDSketch quantile is within a relative error, which
    /// carries through `avg` and `sum` only over non-negative values, and
    /// through `max` whatever their sign. With `x` declared a gauge whose
    /// samples are non-negative, the DDSketch candidate of
    /// `avg by (region) (quantile_over_time(0.99, x[100s]))` (and of its `sum`)
    /// is valid and accepted with a relative-error guarantee; without the
    /// declaration it is rejected. Even `max` needs the primitive interpolated
    /// quantile contract, which can fail across zero.
    #[test]
    fn a_non_negative_gauge_carries_a_ddsketch_relative_bound_through_avg() {
        let target = AccuracyTarget::max_error(0.01)
            .failure_probability(0.01)
            .accuracy_metric(AccuracyMetric::Relative);
        // An `offset` moves the window, not the samples' sign.
        for (outer, window, needs_sign) in [
            ("avg", "x[100s]", true),
            ("sum", "x[100s]", true),
            ("max", "x[100s]", true),
            ("avg", "x[100s] offset 1h", true),
        ] {
            let query = format!("{outer} by (region) (quantile_over_time(0.99, {window}))");
            let candidates = all_candidates(&query, &target);
            let ddsketch = ddsketch_candidates(&candidates);
            assert!(!ddsketch.is_empty(), "{query}");
            for values_non_negative in [true, false] {
                let mut data = with_metric_types(data(), &[("x", MetricType::Gauge)]);
                data.sources.get_mut("x").unwrap().values_non_negative = values_non_negative;
                let selection = select_plan(
                    &candidates,
                    &[entry_with(target.clone(), repeating(10_000))],
                    &data,
                    PlanningModels::builtin(),
                )
                .unwrap();
                for id in &ddsketch {
                    let accepted = selection.accepted.get(id);
                    if values_non_negative || !needs_sign {
                        let [accepted] = accepted.map(Vec::as_slice).unwrap_or_default() else {
                            panic!("{query}, {id} not valid: {:?}", selection.rejected);
                        };
                        assert_eq!(accepted.accepted_metric, Some(AccuracyMetric::Relative));
                        assert!(!accepted.accepted_regardless_of_guarantee);
                    } else {
                        assert!(accepted.is_none(), "{query}, {id}: {accepted:?}");
                        let rejection = selection.rejected.iter().find(|r| &r.id == id).unwrap();
                        assert!(rejection.cause.failed_a_check(), "{rejection:?}");
                    }
                }
            }
        }
    }

    /// PromQL answers a quantile with φ outside [0, 1] as -Inf or +Inf,
    /// which no sketch estimates and the executor's sketches refuse: such a
    /// quantile gets no sketch candidate, so the exact plan is selected, and
    /// a sketch never inherits its samples' sign for it, even over a gauge
    /// declared non-negative.
    #[test]
    fn a_quantile_outside_the_unit_interval_is_answered_exactly() {
        let target = AccuracyTarget::max_error(0.01)
            .failure_probability(0.01)
            .accuracy_metric(AccuracyMetric::Relative);
        for query in [
            "avg by (region) (quantile_over_time(-1, x[100s]))",
            "avg by (region) (quantile_over_time(2, x[100s]))",
            "quantile_over_time(2, x[100s])",
            "max by (region) (quantile_over_time(-1, x[100s]))",
        ] {
            let candidates = all_candidates(query, &target);
            assert!(
                candidates.iter().all(|c| !has_summary_estimate(c)),
                "{query}: a sketch candidate is offered"
            );
            let mut data = with_metric_types(data(), &[("x", MetricType::Gauge)]);
            data.sources.get_mut("x").unwrap().values_non_negative = true;
            let selection = select_plan(
                &candidates,
                &[entry_with(target.clone(), repeating(10_000))],
                &data,
                PlanningModels::builtin(),
            )
            .unwrap();
            let selected = candidates
                .iter()
                .find(|c| c.id == selection.selected)
                .unwrap();
            assert!(
                !has_summary_estimate(selected),
                "{query}: {}",
                selection.selected
            );
        }
    }

    /// `topk(k, irate(m[w]))`, now a heavy-hitter ranking as `rate`'s is,
    /// still plans: Stages 1-3 compose its candidates and select one.
    #[test]
    fn a_direct_irate_ranking_plans() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        for function in ["rate", "irate"] {
            let query = format!("topk(10, {function}(data_total[5m]))");
            let candidates = all_candidates(&query, &target);
            let data = with_metric_types(data(), &[("data_total", MetricType::Counter)]);
            let selection = select_plan(
                &candidates,
                &[entry_with(target.clone(), repeating(10_000))],
                &data,
                PlanningModels::builtin(),
            );
            assert!(selection.is_ok(), "{query}: {selection:?}");
        }
    }

    /// A candidate that ties the selected one is rejected for losing on
    /// enumeration order, not as costlier.
    #[test]
    fn a_tie_is_reported_as_lost_on_enumeration_order() {
        let mut candidates = candidates();
        candidates.truncate(1);
        let mut twin = candidates[0].clone();
        twin.id = "P1-twin".into();
        candidates.push(twin);
        let selection = select_plan(
            &candidates,
            &[entry(repeating(10_000))],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        assert_eq!(selection.selected, "P1");
        let reason = &selection.rejected[0].reason;
        assert!(
            reason.contains("tied with the selected candidate P1")
                && reason.contains("enumeration order"),
            "{reason}"
        );
    }

    /// At δ = 0.001 a KLL candidate meets the target and is valid.
    #[test]
    fn kll_candidate_meets_a_failure_probability_below_one_percent() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let candidates = all_candidates("quantile(0.99, rate(x[5m]))", &target);
        let selection = select_plan(
            &candidates,
            &[entry_with(
                target.clone().accuracy_metric(AccuracyMetric::Rank),
                repeating(10_000),
            )],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        let kll = candidates
            .iter()
            .find(|c| {
                c.dag.nodes.iter().any(|n| {
                    matches!(&n.payload, Payload::ASAP(ASAPOp::SummaryAgg {
                        family: FieldDataType::Sketch(kind, _), ..
                    }) if *kind.algorithm() == SketchAlgorithm::Kll)
                })
            })
            .unwrap();
        assert!(selection.costs.contains_key(&kll.id), "{selection:?}");
    }

    /// `scalar(v)` inherits `v`'s guarantee; arithmetic over it composes
    /// (`c · x` scales an absolute bound, `x − y` adds them); a rank error
    /// does not compose (a rank interval counts as none); one over exact
    /// operands only is exact.
    #[test]
    fn scalar_root_guarantees_compose_through_arithmetic() {
        let node = lower_promql("sum(m)", AccuracyTarget::exact());
        let approximate = AccuracyGuarantee::new(
            AccuracyMetric::ABSOLUTE,
            BoundExpr::Constant { value: 0.01 },
            ProbabilityExpr::Constant { value: 0.001 },
            vec![],
        );
        let with = |guarantee: Option<AccuracyGuarantee>| {
            ScalarExpr::PromqlScalarFromVector(Rc::new((*node).clone().with_guarantee(guarantee)))
        };
        let doubled = |operand: ScalarExpr| {
            QueryRoot::Scalar(ScalarExpr::Arithmetic {
                op: ArithmeticOpKind::Mul,
                left: Box::new(operand),
                right: Box::new(ScalarExpr::literal_f64(2.0)),
                semantics: ExprSemantics::Promql,
            })
        };
        assert_eq!(
            root_guarantee(&QueryRoot::Scalar(with(Some(approximate.clone())))),
            Some(approximate.clone())
        );
        let scaled = root_guarantee(&doubled(with(Some(approximate.clone())))).unwrap();
        assert_eq!(scaled.spec.accuracy_metric, AccuracyMetric::ABSOLUTE);
        assert!((scaled.spec.error.evaluate().unwrap() - 0.02).abs() < 1e-12);
        assert_eq!(scaled.spec.failure_probability.evaluate(), Some(0.001));
        let difference = QueryRoot::Scalar(ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Sub,
            left: Box::new(with(Some(approximate.clone()))),
            right: Box::new(with(Some(approximate.clone()))),
            semantics: ExprSemantics::Promql,
        });
        let sum = root_guarantee(&difference).unwrap();
        assert!((sum.spec.error.evaluate().unwrap() - 0.02).abs() < 1e-12);
        let mut rank = approximate;
        rank.spec.accuracy_metric = AccuracyMetric::Rank;
        assert_eq!(root_guarantee(&doubled(with(Some(rank)))), None);
        assert!(
            root_guarantee(&doubled(with(Some(AccuracyGuarantee::exact("exact")))))
                .is_some_and(|g| g.is_exact())
        );
    }

    /// Every candidate of the Stage 1 to Stage 3 run of one query.
    fn run(
        query: &str,
        target: &AccuracyTarget,
        data: &DataWorkload,
        models: PlanningModels<'_>,
    ) -> (Vec<PhysicalWorkloadPlan>, Selection) {
        run_selecting(query, target, target, data, models)
    }

    /// As [`run`], with summaries sized for `planned` and outputs checked
    /// against `selected`.
    fn run_selecting(
        query: &str,
        planned: &AccuracyTarget,
        selected: &AccuracyTarget,
        data: &DataWorkload,
        models: PlanningModels<'_>,
    ) -> (Vec<PhysicalWorkloadPlan>, Selection) {
        let root = lower_promql(query, planned.clone());
        let run = crate::search::run_stage1_to_stage3_with(
            vec![(0, QueryRoot::Operator(root))],
            &[entry_with(selected.clone(), QueryRecurrence::Unknown)],
            data,
            models,
            crate::search::Stage1ToStage3Search::exhaustive(),
        )
        .unwrap();
        (
            run.stage2_physical_asap_optimization,
            run.stage3_plan_selection.unwrap(),
        )
    }

    fn sketches(candidate: &PhysicalWorkloadPlan) -> Vec<SketchAlgorithm> {
        candidate
            .dag
            .nodes
            .iter()
            .filter_map(|n| match &n.payload {
                Payload::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) => Some(*kind.algorithm()),
                _ => None,
            })
            .collect()
    }

    fn reads_only(candidate: &PhysicalWorkloadPlan, algorithm: SketchAlgorithm) -> bool {
        let sketches = sketches(candidate);
        !sketches.is_empty() && sketches.iter().all(|a| *a == algorithm)
    }

    fn output_guarantee(candidate: &PhysicalWorkloadPlan) -> Option<AccuracyGuarantee> {
        root_guarantee(&candidate.roots[0])
    }

    /// `max by (job)` over per-series DDSketch quantiles keeps their relative
    /// bound, and `2 · x` keeps it too (scaling preserves a relative error);
    /// so those candidates meet the target. Over KLL (a rank error) the
    /// result is a rank interval: no guarantee in the MVP.
    #[test]
    fn extremum_and_scaling_over_per_series_estimates_compose() {
        let target = AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Relative);
        for query in [
            "max by (job) (quantile_over_time(0.99, m[1m]))",
            "2 * max by (job) (quantile_over_time(0.99, m[1m]))",
        ] {
            let (candidates, selection) = run(
                query,
                &target,
                &non_negative_data(),
                PlanningModels::builtin(),
            );
            let ddsketch: Vec<_> = candidates
                .iter()
                .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
                .collect();
            assert!(!ddsketch.is_empty(), "{query}");
            for candidate in ddsketch {
                let g = bound_of(
                    &candidate.roots[0],
                    &non_negative_data(),
                    &PlanningModels::builtin(),
                )
                .expect(query);
                assert_eq!(g.spec.accuracy_metric, AccuracyMetric::Relative, "{query}");
                assert!(g.spec.error.evaluate().unwrap() <= 0.01, "{query}");
                assert!(selection.costs.contains_key(&candidate.id), "{query}");
            }
            for candidate in candidates
                .iter()
                .filter(|c| reads_only(c, SketchAlgorithm::Kll))
            {
                assert_eq!(
                    bound_of(
                        &candidate.roots[0],
                        &non_negative_data(),
                        &PlanningModels::builtin()
                    ),
                    None,
                    "{query}"
                );
            }
        }
    }

    /// Negation, `abs` and `clamp` keep a DDSketch quantile's relative bound,
    /// so those candidates are selectable; a shift by a constant, a
    /// comparison (filter or `bool`) over the estimate compose to no
    /// guarantee in the MVP, so those are not.
    #[test]
    fn lipschitz_functions_keep_and_shifts_and_comparisons_drop_a_relative_bound() {
        let target = AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Relative);
        let q = "quantile_over_time(0.99, m[1m])";
        for (query, kept) in [
            (format!("-{q}"), true),
            (format!("abs({q})"), true),
            (format!("clamp({q}, 1, 100)"), true),
            (format!("clamp_max({q}, 100)"), true),
            (format!("{q} + 1"), false),
            (format!("{q} > 5"), false),
            (format!("{q} > bool 5"), false),
        ] {
            let (candidates, selection) = run(
                &query,
                &target,
                &non_negative_data(),
                PlanningModels::builtin(),
            );
            let ddsketch: Vec<_> = candidates
                .iter()
                .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
                .collect();
            assert!(!ddsketch.is_empty(), "{query}");
            for candidate in ddsketch {
                let g = bound_of(
                    &candidate.roots[0],
                    &non_negative_data(),
                    &PlanningModels::builtin(),
                );
                if kept {
                    let g = g.expect(&query);
                    assert_eq!(g.spec.accuracy_metric, AccuracyMetric::Relative, "{query}");
                    assert!(g.spec.error.evaluate().unwrap() <= 0.01, "{query}");
                    assert!(selection.costs.contains_key(&candidate.id), "{query}");
                } else {
                    assert_eq!(g, None, "{query}");
                    assert!(!selection.costs.contains_key(&candidate.id), "{query}");
                }
            }
        }
    }

    /// A time-window fold over a subquery of estimates folds one estimate
    /// per subquery step and series, so its failure probability is scaled
    /// by the step count `⌈range / resolution⌉` (here 10), not by the input
    /// series count. Without an explicit resolution the step count is not in
    /// the query, and the fold has no guarantee.
    #[test]
    fn a_fold_over_subquery_estimates_counts_its_steps() {
        let target = AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Relative);
        let q = "quantile_over_time(0.99, m[1m])";
        let (candidates, selection) = run(
            &format!("max_over_time({q}[10m:1m])"),
            &target,
            &non_negative_data(),
            PlanningModels::builtin(),
        );
        let ddsketch: Vec<_> = candidates
            .iter()
            .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
            .collect();
        assert!(!ddsketch.is_empty());
        for candidate in ddsketch {
            let g = bound_of(
                &candidate.roots[0],
                &non_negative_data(),
                &PlanningModels::builtin(),
            )
            .expect("a guarantee");
            assert_eq!(g.spec.accuracy_metric, AccuracyMetric::Relative);
            assert!(g.spec.error.evaluate().unwrap() <= 0.01);
            let ProbabilityExpr::Scaled { count, .. } = &g.spec.failure_probability else {
                panic!("{g:?}");
            };
            assert_eq!(count, &BoundExpr::Constant { value: 10.0 });
            assert!(selection.costs.contains_key(&candidate.id));
        }
        let (candidates, _) = run(
            &format!("max_over_time({q}[10m:])"),
            &target,
            &non_negative_data(),
            PlanningModels::builtin(),
        );
        for candidate in candidates
            .iter()
            .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
        {
            assert_eq!(
                bound_of(
                    &candidate.roots[0],
                    &non_negative_data(),
                    &PlanningModels::builtin()
                ),
                None
            );
        }
    }

    /// Counting estimates by a label keeps the count exact: the group keys
    /// are labels, not the estimates (#669).
    #[test]
    fn counting_estimates_by_a_label_stays_exact() {
        let target = AccuracyTarget::max_error(0.01);
        let (candidates, _) = run(
            "count by (le) (quantile_over_time(0.99, m[1m]))",
            &target,
            &data(),
            PlanningModels::builtin(),
        );
        // The inner quantile is a sketch; the count itself is exact.
        let sketched: Vec<_> = candidates
            .iter()
            .filter(|c| {
                let sketches = sketches(c);
                !sketches.is_empty()
                    && sketches
                        .iter()
                        .all(|a| matches!(a, SketchAlgorithm::Kll | SketchAlgorithm::DDSketch))
            })
            .collect();
        assert!(!sketched.is_empty());
        for candidate in sketched {
            assert!(
                output_guarantee(candidate).is_some_and(|g| g.is_exact()),
                "{:?}",
                sketches(candidate)
            );
        }
    }

    /// `avg by (job)` over per-series DDSketch quantiles has a relative bound
    /// that needs the values' sign, a statistic no input supplies: it stays
    /// symbolic and plan selection treats it as unmet.
    #[test]
    fn average_over_relative_estimates_needs_the_sign() {
        use asap_logical_asap_optimization::pass1::per_computation_candidates as pcc;
        let target = AccuracyTarget::max_error(0.01);
        let root = lower_promql(
            "avg by (job) (quantile_over_time(0.99, m[1m]))",
            target.clone(),
        );
        let inventory =
            pcc::generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))]).unwrap();
        // Exercise the intended approximation directly; a joint optimizer may
        // legitimately prefer an exact implementation or a different sharing
        // topology, neither of which tests this compositional error contract.
        let choices: Vec<_> = inventory
            .targets
            .iter()
            .map(|target| {
                target
                    .ways_to_compute
                    .iter()
                    .position(|candidate| {
                        *candidate
                            == pcc::HowToComputeAggregateMeasure::Summary(SketchAlgorithm::DDSketch)
                    })
                    .unwrap_or(0)
            })
            .collect();
        assert!(inventory
            .targets
            .iter()
            .zip(&choices)
            .any(|(target, &choice)| target.ways_to_compute[choice]
                == pcc::HowToComputeAggregateMeasure::Summary(SketchAlgorithm::DDSketch)));
        let outputs = pcc::compose_logical_candidate(&inventory, &choices).unwrap();
        let output = &outputs[0].1;
        let guarantee = root_guarantee(output).expect("a symbolic bound");
        assert!(guarantee.spec.error.has_unknown(), "{guarantee:?}");
        assert!(guarantee_check(0, output, &target, &data(), &PlanningModels::builtin()).is_err());
    }

    /// `max` folds one value per series, so its failure probability is
    /// scaled by the symbolic input row count. Stage 3 binds it from the
    /// series count of the source the query reads; without one, the target
    /// is unmet.
    #[test]
    fn stage3_binds_the_input_row_count_from_the_data_workload() {
        let target = AccuracyTarget::max_error(0.01)
            .failure_probability(0.001)
            .accuracy_metric(AccuracyMetric::Relative);
        let query = "max by (job) (quantile_over_time(0.99, m[1m]))";
        let ddsketch = |candidates: &[PhysicalWorkloadPlan]| -> Vec<String> {
            candidates
                .iter()
                .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
                .map(|c| c.id.clone())
                .collect()
        };
        let (candidates, with) = run(
            query,
            &target,
            &non_negative_data(),
            PlanningModels::builtin(),
        );
        let ids = ddsketch(&candidates);
        assert!(!ids.is_empty());
        assert!(ids.iter().all(|id| with.costs.contains_key(id)));
        let mut unknown = with_series(None);
        for facts in unknown.sources.values_mut() {
            facts.values_non_negative = true;
        }
        // Evaluate the same physical candidates: IDs from independent timed
        // searches do not identify the same implementation or connections.
        let without = select_plan(
            &candidates,
            &[entry_with(target, QueryRecurrence::Unknown)],
            &unknown,
            PlanningModels::builtin(),
        )
        .unwrap();
        assert!(ids.iter().all(|id| !without.costs.contains_key(id)));
    }

    /// Deployment contract: every operand lies in `[1, 1000]`.
    struct BoundedOperands;

    impl AccuracyEvidenceProvider for BoundedOperands {
        fn operand_domain(&self, _operand: &OperatorNode) -> Option<asap_accuracy::OperandDomain> {
            Some(asap_accuracy::OperandDomain {
                lower: 1.0,
                upper: 1_000.0,
            })
        }
    }

    /// A ratio of two DDSketch quantiles has a closed-form relative bound,
    /// `(a + b)/(1 − b)`, with no operand domain: each operand sized for
    /// 0.01 gives about 0.0202, which meets a 0.05 target.
    #[test]
    fn ddsketch_ratio_meets_its_target_without_operand_domains() {
        let planned = AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Relative);
        let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
        let query = "quantile_over_time(0.99, m[1m]) / quantile_over_time(0.5, m[1m])";
        let (candidates, selection) = run_selecting(
            query,
            &planned,
            &target,
            &non_negative_data(),
            PlanningModels::builtin(),
        );
        let ids: Vec<_> = candidates
            .iter()
            .filter(|c| sketches(c) == [SketchAlgorithm::DDSketch, SketchAlgorithm::DDSketch])
            .map(|c| c.id.clone())
            .collect();
        // The Pass 1 candidate and its Pass 2 candidates sharing the input.
        assert!(!ids.is_empty());
        assert!(
            ids.iter().all(|id| selection.costs.contains_key(id)),
            "{selection:?}"
        );
    }

    /// `query` carrying `guarantee`, as a `scalar(…)` operand.
    fn scalar_of(query: &str, guarantee: AccuracyGuarantee) -> ScalarExpr {
        let node = lower_promql(query, AccuracyTarget::exact());
        ScalarExpr::PromqlScalarFromVector(Rc::new((*node).clone().with_guarantee(Some(guarantee))))
    }

    fn guarantee(metric: AccuracyMetric, bound: f64, delta: f64) -> AccuracyGuarantee {
        AccuracyGuarantee::new(
            metric,
            BoundExpr::Constant { value: bound },
            ProbabilityExpr::Constant { value: delta },
            vec![],
        )
    }

    fn scalar_root(op: ArithmeticOpKind, left: ScalarExpr, right: ScalarExpr) -> QueryRoot {
        QueryRoot::Scalar(ScalarExpr::Arithmetic {
            op,
            left: Box::new(left),
            right: Box::new(right),
            semantics: ExprSemantics::Promql,
        })
    }

    fn norms(sources: &[(&str, f64)]) -> DataWorkload {
        let mut data = data();
        for (source, l1) in sources {
            data.sources
                .entry(source.to_string())
                .or_default()
                .stream_norms = Some(asap_types::workload::StreamNorms {
                l1: Some(*l1),
                l2: None,
            });
        }
        data
    }

    /// A difference of two Count-Min estimates of the raw values of
    /// different metrics is bound by each metric's own stream norm from the
    /// data workload
    /// (`0.01 · 100 + 0.02 · 1000`); a metric without a norm leaves the
    /// bound unmet.
    #[test]
    fn stage3_binds_each_metric_stream_norm() {
        // Count-Min summaries of each metric's raw value column.
        let raw = |query: &str, g: AccuracyGuarantee| {
            ScalarExpr::PromqlScalarFromVector(
                asap_logical_asap_optimization::test_support::estimate_over(
                    asap_logical_asap_optimization::test_support::weighted(
                        asap_logical_asap_optimization::test_support::cms_summary(query),
                        asap_types::ir::schema::SummaryInputExpr::Column(
                            asap_types::ir::scalar::ColumnRef::SampleValue,
                        ),
                    ),
                    g,
                ),
            )
        };
        let root = scalar_root(
            ArithmeticOpKind::Sub,
            raw(
                "count by (job) (a)",
                guarantee(AccuracyMetric::ABSOLUTE_STREAM_L1, 0.01, 0.001),
            ),
            raw(
                "count by (job) (b)",
                guarantee(AccuracyMetric::ABSOLUTE_STREAM_L1, 0.02, 0.001),
            ),
        );
        let models = PlanningModels::builtin();
        let both = bound_of(&root, &norms(&[("a", 100.0), ("b", 1_000.0)]), &models).unwrap();
        assert!(
            (both.spec.error.evaluate().unwrap() - 21.0).abs() < 1e-9,
            "{both:?}"
        );
        let one = bound_of(&root, &norms(&[("a", 100.0)]), &models).unwrap();
        assert!(one.spec.error.has_unknown());
    }

    /// A stream norm is bound only for a summary of the source's raw value
    /// column: a count over the same source (constant weight) needs the
    /// source's row count, which nothing binds, and a summary of a computed
    /// value (`m * 1000`) needs a norm nothing binds; both stay unmet.
    #[test]
    fn stage3_binds_a_stream_norm_only_for_the_raw_value_column() {
        use asap_types::ir::scalar::ColumnRef;
        use asap_types::ir::schema::SummaryInputExpr;
        let frequency = guarantee(AccuracyMetric::ABSOLUTE_STREAM_L1, 0.01, 0.001);
        let doubled = |summary: Rc<OperatorNode>| {
            scalar_root(
                ArithmeticOpKind::Mul,
                ScalarExpr::literal_f64(2.0),
                ScalarExpr::PromqlScalarFromVector(
                    asap_logical_asap_optimization::test_support::estimate_over(
                        summary,
                        frequency.clone(),
                    ),
                ),
            )
        };
        let models = PlanningModels::builtin();
        let data = norms(&[("m", 100.0)]);
        let column = || SummaryInputExpr::Column(ColumnRef::SampleValue);
        let raw = doubled(asap_logical_asap_optimization::test_support::weighted(
            asap_logical_asap_optimization::test_support::cms_summary("count by (job) (m)"),
            column(),
        ));
        let g = bound_of(&raw, &data, &models).unwrap();
        assert!(
            (g.spec.error.evaluate().unwrap() - 2.0).abs() < 1e-12,
            "{g:?}"
        );
        for unbound in [
            doubled(Rc::new(
                asap_logical_asap_optimization::test_support::cms_summary("count by (job) (m)"),
            )),
            doubled(asap_logical_asap_optimization::test_support::weighted(
                asap_logical_asap_optimization::test_support::cms_summary(
                    "count by (job) (m * 1000)",
                ),
                column(),
            )),
        ] {
            let g = bound_of(&unbound, &data, &models).unwrap();
            assert!(g.spec.error.has_unknown(), "{g:?}");
        }
    }

    /// A product and a quotient of two estimates are bound from the
    /// deployment's operand domains (`[1, 1000]`):
    /// `|x|·e_y + |y|·e_x + e_x·e_y` and `(|x|·e_y + |y|·e_x) / (|y|·(|y| − e_y))`
    /// at the domain's extremes; without domains they stay unmet. A ratio of
    /// relative errors needs no domain: `(a + b)/(1 − b)`.
    #[test]
    fn stage3_binds_products_and_quotients_from_operand_domains() {
        let (x, y) = (
            scalar_of("sum(a)", guarantee(AccuracyMetric::ABSOLUTE, 0.01, 0.001)),
            scalar_of("sum(b)", guarantee(AccuracyMetric::ABSOLUTE, 0.02, 0.001)),
        );
        let product = scalar_root(ArithmeticOpKind::Mul, x.clone(), y.clone());
        let quotient = scalar_root(ArithmeticOpKind::Div, x, y);
        let bounded = PlanningModels::builtin().with_evidence(&BoundedOperands);
        for (root, expected) in [
            (&product, 1_000.0 * 0.02 + 1_000.0 * 0.01 + 0.01 * 0.02),
            (&quotient, (1_000.0 * 0.02 + 1_000.0 * 0.01) / (1.0 - 0.02)),
        ] {
            let g = bound_of(root, &data(), &bounded).unwrap();
            assert!(
                (g.spec.error.evaluate().unwrap() - expected).abs() < 1e-9,
                "{g:?}"
            );
            assert!((g.spec.failure_probability.evaluate().unwrap() - 0.002).abs() < 1e-12);
            let unbound = bound_of(root, &data(), &PlanningModels::builtin()).unwrap();
            assert!(unbound.spec.error.has_unknown(), "{unbound:?}");
        }
        let ratio = scalar_root(
            ArithmeticOpKind::Div,
            scalar_of("sum(a)", guarantee(AccuracyMetric::Relative, 0.01, 0.0)),
            scalar_of("sum(b)", guarantee(AccuracyMetric::Relative, 0.01, 0.0)),
        );
        let g = bound_of(&ratio, &data(), &PlanningModels::builtin()).unwrap();
        assert!(
            (g.spec.error.evaluate().unwrap() - 0.02 / 0.99).abs() < 1e-12,
            "{g:?}"
        );
    }

    /// A ratio divided by an exact vector, `(q99 / q50) / max_over_time(m)`,
    /// keeps the inner ratio's relative bound, so the candidate is
    /// selectable with no operand domain.
    #[test]
    fn a_scaled_ratio_keeps_its_relative_bound() {
        let planned = AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Relative);
        let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
        let query = "(quantile_over_time(0.99, m[1m]) / quantile_over_time(0.5, m[1m])) \
                     / max_over_time(m[1m])";
        let (candidates, selection) = run_selecting(
            query,
            &planned,
            &target,
            &non_negative_data(),
            PlanningModels::builtin(),
        );
        let ids: Vec<_> = candidates
            .iter()
            .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
            .map(|c| c.id.clone())
            .collect();
        assert!(!ids.is_empty());
        assert!(
            ids.iter().all(|id| selection.costs.contains_key(id)),
            "{selection:?}"
        );
    }

    /// The input row count is bound only from the series count of the
    /// sources the query reads: another source's declared series count
    /// leaves it unbound.
    #[test]
    fn stage3_binds_only_the_series_count_of_the_sources_read() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let query = "max by (job) (quantile_over_time(0.99, m[1m]))";
        let mut other_source = data();
        other_source.sources.remove("m");
        let (candidates, selection) = run(query, &target, &other_source, PlanningModels::builtin());
        let ddsketch: Vec<_> = candidates
            .iter()
            .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
            .collect();
        assert!(!ddsketch.is_empty());
        assert!(ddsketch
            .iter()
            .all(|c| !selection.costs.contains_key(&c.id)));
    }

    /// `quantile_over_time(0.99, m[1m])` with its KLL candidate chosen, its
    /// state materialized as `materialization`.
    fn kll_dag(materialization: Materialization) -> PhysicalASAPDAG {
        use asap_types::ir::{
            apply_materialization_timings, MaterializationAssignment, NodeToTimedNodeCache,
        };
        let root = lower_promql(
            "quantile_over_time(0.99, m[1m])",
            AccuracyTarget::max_error(0.01).failure_probability(0.01),
        );
        let inventory = asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        let choice: Vec<_> = inventory
            .targets
            .iter()
            .map(|t| {
                t.ways_to_compute
                    .iter()
                    .position(|c| {
                        *c == asap_logical_asap_optimization::pass1::per_computation_candidates::HowToComputeAggregateMeasure::Summary(
                            SketchAlgorithm::Kll,
                        )
                    })
                    .unwrap_or(0)
            })
            .collect();
        let roots =
            asap_logical_asap_optimization::pass1::per_computation_candidates::compose_logical_candidate(
                &inventory, &choice,
            )
            .unwrap();
        let QueryRoot::Operator(root) = &roots[0].1 else {
            panic!("operator root")
        };
        let state = OperatorNode::reachable(root)
            .into_iter()
            .find(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
            .expect("a KLL state");
        let mut assignment =
            MaterializationAssignment::with_default(Materialization::NotMaterialized);
        assignment.set(&state, materialization);
        let timed =
            apply_materialization_timings(root, &assignment, &mut NodeToTimedNodeCache::new())
                .unwrap();
        asap_types::ir::physical_export::compile_physical_asap_workload_with_node_ids(&[
            QueryRoot::Operator(timed),
        ])
        .unwrap()
        .dag
    }

    /// The physical DAG of `query`'s first per-computation candidates,
    /// with nothing materialized.
    fn first_candidate_dag(query: &str) -> PhysicalASAPDAG {
        let root = lower_promql(query, AccuracyTarget::exact());
        let inventory = asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        let choice = vec![0; inventory.targets.len()];
        let roots: Vec<_> =
            asap_logical_asap_optimization::pass1::per_computation_candidates::compose_logical_candidate(
                &inventory, &choice,
            )
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        optimize_physical_asap("L1", &roots, &[], &data(), false)
            .unwrap()
            .remove(0)
            .dag
    }

    /// A time shift is free and does not change what the scan below it
    /// reads: the scan reads the window above the shift, as without it. A
    /// time range is priced by the rows it keeps.
    #[test]
    fn time_shift_is_free_and_the_scan_reads_the_shifted_window() {
        let every_minute = [repeating(60_000)];
        let plain = first_candidate_dag("sum_over_time(m[5m])");
        let shifted = first_candidate_dag("sum_over_time(m[5m] offset 1h)");
        let (plain_cost, shifted_cost) = (
            priced(&plain, &every_minute),
            priced(&shifted, &every_minute),
        );
        let cost_of = |dag: &PhysicalASAPDAG, cost: &CandidateCost, pick: fn(&Payload) -> bool| {
            cost.per_node[&node_of(dag, pick)].cost_per_second.unwrap()
        };
        let is_shift = |p: &Payload| matches!(p, Payload::NonASAP(NonASAPOp::TimeShift { .. }));
        let is_range = |p: &Payload| matches!(p, Payload::NonASAP(NonASAPOp::TimeRange { .. }));
        assert_eq!(cost_of(&shifted, &shifted_cost, is_shift), 0.0);
        assert!(close(
            cost_of(&shifted, &shifted_cost, is_scan),
            cost_of(&plain, &plain_cost, is_scan)
        ));
        assert!(close(
            shifted_cost.cost_per_second.unwrap(),
            plain_cost.cost_per_second.unwrap()
        ));
        let range = &plain_cost.per_node[&node_of(&plain, is_range)];
        assert!(range.detail.contains("keep"), "{}", range.detail);
    }

    const AT_INGESTION: Materialization =
        Materialization::MatAtIngestion(asap_types::physical::Medium::Memory);
    const AT_QUERY: Materialization =
        Materialization::MatAtQuery(asap_types::physical::Medium::Memory);

    fn node_of(dag: &PhysicalASAPDAG, pick: impl Fn(&Payload) -> bool) -> PhysicalASAPNodeId {
        dag.nodes.iter().find(|n| pick(&n.payload)).unwrap().id
    }

    fn is_scan(payload: &Payload) -> bool {
        matches!(payload, Payload::NonASAP(NonASAPOp::Scan { .. }))
    }

    fn is_build(payload: &Payload) -> bool {
        matches!(payload, Payload::ASAP(ASAPOp::SummaryAgg { .. }))
    }

    fn priced(dag: &PhysicalASAPDAG, recurrence: &[QueryRecurrence]) -> CandidateCost {
        price(dag, recurrence, &data(), &CostInputs::default()).unwrap()
    }

    /// The default settings with cost model 1's unit costs edited.
    fn unit_costs(edit: impl FnOnce(&mut ResourceUnitCost)) -> CostInputs {
        let mut prices = ResourceUnitCost::illustrative();
        edit(&mut prices);
        CostInputs {
            cost_model: CostModel::UsageOverTime(prices),
            ..CostInputs::default()
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-12 * a.abs().max(b.abs()).max(1e-300)
    }

    /// An ingestion-time node is priced over one second of arriving rows,
    /// once per second, whatever its readers' evaluation rate; a query-time
    /// node scales with that rate.
    #[test]
    fn ingestion_time_node_is_priced_per_second_of_arriving_rows() {
        let dag = kll_dag(AT_INGESTION);
        let (fast, slow) = (
            priced(&dag, &[repeating(1_000)]),
            priced(&dag, &[repeating(10_000)]),
        );
        for id in [node_of(&dag, is_scan), node_of(&dag, is_build)] {
            assert_eq!(
                fast.per_node[&id].cost_per_second,
                slow.per_node[&id].cost_per_second
            );
            assert!(fast.per_node[&id].detail.contains("; x 1.0000/s"));
        }
        let estimate = dag.roots[0];
        assert!(close(
            fast.per_node[&estimate].cost_per_second.unwrap(),
            10.0 * slow.per_node[&estimate].cost_per_second.unwrap()
        ));
    }

    /// Kept state is charged per byte per second: at ingestion time, twice
    /// the state's bytes (the window being built and the completed one) for
    /// every second. At query time the state is reused only within one
    /// evaluation, so it is held only while that evaluation runs: its bytes
    /// times the evaluation's latency, once per evaluation. State computed
    /// and dropped within a query (`NotMaterialized`) is held while that
    /// query runs too, and charged the same way.
    #[test]
    fn kept_state_is_charged_per_byte_per_second() {
        let cost = |dag: &PhysicalASAPDAG, w| {
            let settings = unit_costs(|c| c.cost_per_memory_byte_second = w);
            price(dag, &recurrences(1), &data(), &settings).unwrap()
        };
        for materialization in [AT_INGESTION, AT_QUERY, Materialization::NotMaterialized] {
            let dag = kll_dag(materialization);
            let build = node_of(&dag, is_build);
            let (with, without) = (cost(&dag, 1.25e-7), cost(&dag, 0.0));
            let Payload::ASAP(ASAPOp::SummaryAgg { family, .. }) =
                &dag.nodes.iter().find(|n| n.id == build).unwrap().payload
            else {
                unreachable!()
            };
            // One state per series: data() declares 10 000. At query time
            // each sees its series' 12 samples: the 120 000 rows of its
            // source's 1-min window (data()'s 10 000 rows per second over its
            // five sources); at ingestion time its samples are unknown.
            let samples = match materialization {
                AT_INGESTION => StateInputRows::UNKNOWN,
                _ => StateInputRows {
                    rows_per_state: Some(12),
                    non_negative: false,
                },
            };
            let state = 10_000.0 * summary_shape(family, samples).1 as f64;
            let byte_seconds = if materialization == AT_INGESTION {
                2.0 * state
            } else {
                let evaluation_s = with.usage.latency_per_query[0].total_ms() / 1_000.0;
                assert!(evaluation_s > 0.0 && evaluation_s < 1.0, "{evaluation_s}");
                // Once per evaluation of the query repeating every 10 s.
                assert!(with.per_node[&build].detail.contains("; x 0.1000/s"));
                state * evaluation_s * 0.1
            };
            let term = with.per_node[&build].cost_per_second.unwrap()
                - without.per_node[&build].cost_per_second.unwrap();
            assert!(
                close(term, 1.25e-7 * byte_seconds),
                "{materialization:?}: {term} vs {}",
                1.25e-7 * byte_seconds
            );
            // The candidate's memory term covers every node's held memory,
            // the build's included.
            let candidate_term = with.cost_per_second.unwrap() - without.cost_per_second.unwrap();
            assert!(close(
                candidate_term,
                1.25e-7 * with.usage.average_memory_bytes
            ));
            assert!(candidate_term >= term);
        }
    }

    /// Under cost model 1, memory is charged as bytes held per second
    /// (average memory): the memory a computation holds while it runs (a
    /// sketch being built, an exact quantile's values) counts for the time
    /// it is held, once per evaluation. The memory price's term is exactly
    /// the average memory, which is below the peak memory.
    #[test]
    fn memory_held_only_while_a_query_runs_is_charged_for_the_time_it_is_held() {
        let memory_term = |dag: &PhysicalASAPDAG| {
            let cost = |price_per_byte_second| {
                let settings =
                    unit_costs(|c| c.cost_per_memory_byte_second = price_per_byte_second);
                price(dag, &recurrences(1), &data(), &settings).unwrap()
            };
            let (with, without) = (cost(1.0), cost(0.0));
            let term = with.cost_per_second.unwrap() - without.cost_per_second.unwrap();
            (term, with.usage)
        };
        let mut dags: Vec<(String, PhysicalASAPDAG)> = [
            "quantile_over_time(0.99, m[1m])",
            "quantile by (job) (0.99, m)",
        ]
        .into_iter()
        .map(|query| {
            let root = lower_promql(query, AccuracyTarget::exact());
            let dag =
                optimize_physical_asap("L", &[QueryRoot::Operator(root)], &[], &data(), false)
                    .unwrap()
                    .remove(0)
                    .dag;
            (query.to_string(), dag)
        })
        .collect();
        for materialization in [AT_INGESTION, AT_QUERY, Materialization::NotMaterialized] {
            dags.push((format!("{materialization:?}"), kll_dag(materialization)));
        }
        for (name, dag) in &dags {
            let (term, usage) = memory_term(dag);
            assert!(
                close(term, usage.average_memory_bytes),
                "{name}: {term} vs {}",
                usage.average_memory_bytes
            );
            assert!(
                usage.peak_memory_bytes as f64 > usage.average_memory_bytes,
                "{name}: {usage:?}"
            );
        }
        // Not materialized, nothing is kept, but the sketch being built is
        // held while the query runs: the memory price adds to the cost.
        let (term, _) = memory_term(&kll_dag(Materialization::NotMaterialized));
        assert!(term > 0.0, "{term}");
    }

    /// Kept exact state that grows with the data is charged by the values it
    /// keeps: a materialized exact quantile keeps every value of its group,
    /// a sum one value per group.
    #[test]
    fn kept_exact_state_is_charged_by_the_values_it_keeps() {
        let kept = |query: &str| {
            let root = lower_promql(query, AccuracyTarget::exact());
            let mut dag =
                optimize_physical_asap("L", &[QueryRoot::Operator(root)], &[], &data(), false)
                    .unwrap()
                    .remove(0)
                    .dag;
            let aggregate = dag
                .nodes
                .iter_mut()
                .find(|n| matches!(n.payload, Payload::NonASAP(NonASAPOp::Aggregate { .. })))
                .unwrap();
            aggregate.materialization = AT_QUERY;
            let id = aggregate.id;
            let cost = |w| {
                let settings = unit_costs(|c| c.cost_per_memory_byte_second = w);
                // 10 jobs, so each group keeps many values.
                let mut data = arriving(10_000.0);
                data.sources.get_mut("m").unwrap().distinct_values_per_label =
                    [("job".to_string(), 10)].into();
                price(&dag, &recurrences(1), &data, &settings)
                    .unwrap()
                    .per_node[&id]
                    .cost_per_second
                    .unwrap()
            };
            (cost(1.0) - cost(0.0)).round() as u64
        };
        let (quantile, sum) = (
            kept("quantile by (job) (0.99, m)"),
            kept("sum by (job) (m)"),
        );
        assert!(quantile > 10 * sum, "{quantile} vs {sum}");
    }

    /// A `NotMaterialized` node read by two queries is computed for each, at
    /// each one's rate; `MatAtQuery`, it is computed once at their combined
    /// rate, where queries at the same interval run together.
    #[test]
    fn rates_follow_per_consumer_charging() {
        let root = lower_promql("sum by (job) (rate(m[1m]))", AccuracyTarget::exact());
        let one = optimize_physical_asap(
            "L",
            &[QueryRoot::Operator(root.clone())],
            &[],
            &data(),
            false,
        )
        .unwrap()
        .remove(0)
        .dag;
        let mut two = optimize_physical_asap(
            "L",
            &[QueryRoot::Operator(root.clone()), QueryRoot::Operator(root)],
            &[],
            &data(),
            false,
        )
        .unwrap()
        .remove(0)
        .dag;
        let without_memory = unit_costs(|c| c.cost_per_memory_byte_second = 0.0);
        let total = |dag: &PhysicalASAPDAG, recurrence: &[QueryRecurrence]| {
            price(dag, recurrence, &data(), &without_memory)
                .unwrap()
                .cost_per_second
                .unwrap()
        };
        let single = total(&one, &recurrences(1));
        let mixed = [repeating(10_000), repeating(20_000)];
        assert!(close(total(&two, &recurrences(2)), 2.0 * single));
        assert!(close(total(&two, &mixed), 1.5 * single));
        assert_eq!(two.roots[0], two.roots[1], "one root node for both queries");
        for node in &mut two.nodes {
            node.materialization = AT_QUERY;
        }
        assert!(close(total(&two, &recurrences(2)), single));
        assert!(close(total(&two, &mixed), 1.5 * single));
    }

    /// Queries that refresh together at one interval and step evaluate at
    /// the same instants: a node they share runs at the most steps among
    /// them, not their sum. One over the last hour (61 steps) and one over
    /// the last 10 min (11 steps), both every minute at 1-min steps, and a
    /// third with no range (its one evaluation is the others' last step):
    /// 61 evaluations a minute. A different step adds only the instants
    /// the others do not evaluate at.
    #[test]
    fn readers_at_one_interval_and_step_share_their_steps() {
        let every_minute = repeating(60_000);
        let range = |span_ms: u64, step_ms: u64| Some(RangeEvaluation { step_ms, span_ms });
        let rate = |queries: &[Option<RangeEvaluation>]| {
            evaluation_rate(queries.iter().map(|r| (&every_minute, *r)), 3_600.0).unwrap()
        };
        let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
        assert!(close(rate(&[range(3_600_000, 60_000)]), 61.0 / 60.0));
        assert!(close(
            rate(&[range(3_600_000, 60_000), range(600_000, 60_000), None]),
            61.0 / 60.0
        ));
        assert!(close(rate(&[None, None]), 1.0 / 60.0));
        // A one-step range evaluates only at the request's end.
        assert!(close(
            rate(&[range(3_600_000, 60_000), range(0, 30_000)]),
            61.0 / 60.0
        ));
        // A 30 s step over the last 10 min shares 11 of its 21 instants
        // with the 1-min steps over the last hour: 71 distinct instants.
        assert!(close(
            rate(&[range(3_600_000, 60_000), range(600_000, 30_000)]),
            71.0 / 60.0
        ));
    }

    /// Queries every 20 s at phases 0 and 5 s evaluate at different
    /// instants: a node they share runs at both, twice as often as at one
    /// phase. Phases 0 and 40 s are one phase on 20 s, and a query of
    /// unknown phase runs with the known one.
    #[test]
    fn readers_at_different_phases_evaluate_apart() {
        let at = |phase_ms: u64| {
            QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
                interval: asap_types::workload::RepetitionInterval(20_000),
                evaluation_phase: asap_types::workload::TimestampMs(phase_ms),
            })
        };
        let unknown = repeating(20_000);
        let rate = |queries: &[&QueryRecurrence]| {
            evaluation_rate(queries.iter().map(|r| (*r, None)), 3_600.0).unwrap()
        };
        let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
        assert!(close(rate(&[&at(0), &at(5_000)]), 2.0 / 20.0));
        assert!(close(rate(&[&at(0), &at(40_000)]), 1.0 / 20.0));
        assert!(close(rate(&[&at(5_000), &unknown]), 1.0 / 20.0));
        assert!(close(rate(&[&at(0), &at(5_000), &unknown]), 2.0 / 20.0));
    }

    /// One-off and unknown recurrence are spread over the horizon:
    /// invocations × per-evaluation cost / non_repeating_query_horizon_seconds.
    #[test]
    fn one_off_recurrence_is_spread_over_the_horizon() {
        let root = lower_promql("sum by (job) (rate(m[1m]))", AccuracyTarget::exact());
        let dag = optimize_physical_asap("L", &[QueryRoot::Operator(root)], &[], &data(), false)
            .unwrap()
            .remove(0)
            .dag;
        let horizon = CostInputs::default().non_repeating_query_horizon_seconds;
        assert_eq!(horizon, 3_600.0);
        let per_evaluation = priced(&dag, &[repeating(1_000)]).cost_per_second.unwrap();
        let once = priced(
            &dag,
            &[QueryRecurrence::OneTime {
                invocations: 3,
                execute_at: None,
            }],
        )
        .cost_per_second
        .unwrap();
        assert!(close(once, 3.0 * per_evaluation / horizon), "{once}");
        assert!(close(
            priced(&dag, &[QueryRecurrence::Unknown])
                .cost_per_second
                .unwrap(),
            per_evaluation / horizon
        ));
    }

    /// Settings outside their domain are an error, not a price, checked once
    /// before any candidate: a price the cost model reads that is negative,
    /// the model's prices all 0 (cost model 1 with only provisioned prices
    /// set), or a horizon that is not positive. A price the model does not
    /// read is not checked.
    #[test]
    fn invalid_settings_are_rejected() {
        let candidates = pane_candidates(10_000.0);
        let select = |settings: &CostInputs| {
            select_plan(
                &candidates,
                &[every_minute()],
                &arriving(10_000.0),
                PlanningModels::builtin().with_plan_selection(settings),
            )
        };
        let only_provisioned = ResourceUnitCost {
            cost_per_cpu_op: 0.0,
            cost_per_scanned_byte: 0.0,
            cost_per_memory_byte_second: 0.0,
            cost_per_disk_byte_second: 0.0,
            ..ResourceUnitCost::illustrative()
        };
        for settings in [
            unit_costs(|c| c.cost_per_memory_byte_second = -1.0),
            under(CostModel::UsageOverTime(only_provisioned.clone())),
            CostInputs {
                non_repeating_query_horizon_seconds: 0.0,
                ..CostInputs::default()
            },
        ] {
            assert!(
                matches!(select(&settings), Err(SelectionError::InvalidSettings(_))),
                "{settings:?}"
            );
        }
        assert!(select(&under(CostModel::ProvisionedCapacity(only_provisioned))).is_ok());
        assert!(select(&unit_costs(
            |c| c.cost_per_provisioned_cpu_core_second = -1.0
        ))
        .is_ok());
    }

    // ── Stage 2 materialization and the latency check ────────────────────

    const MINUTE: u32 = 60_000;

    /// The p99 target [`pane_candidates`] are sized for.
    fn p99_target() -> AccuracyTarget {
        AccuracyTarget::max_error(0.01)
            .failure_probability(0.001)
            .accuracy_metric(AccuracyMetric::Rank)
    }

    /// A predictable p99 query evaluated every minute, with no latency bound.
    fn every_minute() -> QueryWorkloadEntry {
        QueryWorkloadEntry {
            predictability: asap_types::workload::Predictability::Predictable { known_at: None },
            ..entry_with(p99_target(), repeating(MINUTE))
        }
    }

    /// [`data`] with each test source arriving at `rows_per_s`: pricing
    /// gives each source its series share of the workload's rate.
    fn arriving(rows_per_s: f64) -> DataWorkload {
        DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: rows_per_s * TEST_SOURCES.len() as f64,
            },
            ..data()
        }
    }

    /// The physical candidates of a per-series p99 over 5 min with its KLL
    /// in five 1-min tumbling panes, every minute over data arriving at
    /// `rows_per_s`: rebuilt at query time (B2), then maintained at
    /// ingestion time (B1).
    fn pane_candidates(rows_per_s: f64) -> Vec<PhysicalWorkloadPlan> {
        let candidates = pane_candidates_for(rows_per_s, &DeploymentCapabilities::default());
        assert_eq!(candidates.len(), 2);
        candidates
    }

    /// [`pane_candidates`] for a deployment with `deployment`'s
    /// capabilities: B2, B1, then, when it keeps query-time results, B3
    /// (built at query time and kept across evaluations).
    fn pane_candidates_for(
        rows_per_s: f64,
        deployment: &DeploymentCapabilities,
    ) -> Vec<PhysicalWorkloadPlan> {
        let root = lower_promql("quantile_over_time(0.99, m[5m])", p99_target());
        let inventory = asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        let kll = inventory.targets[0]
            .ways_to_compute
            .iter()
            .position(|c| {
                matches!(
                    c,
                    asap_logical_asap_optimization::pass1::per_computation_candidates::HowToComputeAggregateMeasure::Summary(
                        SketchAlgorithm::Kll
                    )
                )
            })
            .unwrap();
        let roots: Vec<_> =
            asap_logical_asap_optimization::pass1::per_computation_candidates::compose_logical_candidate(
                &inventory,
                &[kll],
            )
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        let rule_candidates =
            asap_logical_asap_optimization::pass2::asap_aware_cse::pane_candidates(
                &roots,
                &[Some(u64::from(MINUTE))],
            );
        let roots =
            asap_logical_asap_optimization::pass2::asap_aware_cse::window_composition_pass2_candidates(
                &roots,
                &rule_candidates,
            )
            .unwrap()
            .pop()
            .unwrap()
            .roots
            .unwrap();
        let mut candidates = optimize_physical_asap(
            "L1",
            &roots,
            &[every_minute()],
            &arriving(rows_per_s),
            deployment.query_time_results_retained,
        )
        .unwrap();
        for (k, c) in candidates.iter_mut().enumerate() {
            c.id = format!("P1-m{k}");
        }
        candidates
    }

    /// The raw data samples the scan `scan` reads per computation, from its
    /// priced detail (`scan <n> samples; ...`).
    fn scanned_samples(cost: &CandidateCost, scan: PhysicalASAPNodeId) -> u64 {
        let detail = &cost.per_node[&scan].detail;
        detail
            .strip_prefix("scan ")
            .and_then(|rest| rest.split(' ').next())
            .and_then(|rows| rows.parse().ok())
            .unwrap_or_else(|| panic!("not a scan: {detail}"))
    }

    /// A deployment that keeps query-time results across evaluations.
    fn keeping_query_time_results() -> DeploymentCapabilities {
        DeploymentCapabilities {
            query_time_results_retained: true,
            ..DeploymentCapabilities::default()
        }
    }

    /// `query_time_results_retained` gates keeping a query-time result across
    /// evaluations, and only that. Without it, Stage 2 offers the 1-min
    /// panes of a query repeating every minute rebuilt (B2) or maintained
    /// (B1); with it, also built at query time and kept (B3,
    /// `MatAtQueryKeptAcrossEvaluations`). B3 is computed once per
    /// evaluation, reads one pane (1 min) of raw data, keeps that much raw
    /// data on disk, and holds its 4 older panes in memory between
    /// evaluations (on disk, if it were placed there). The same chain
    /// `MatAtQuery` is reused only within one evaluation: the scan reads the
    /// 5-min window and nothing is kept between evaluations.
    #[test]
    fn query_time_results_retained_keeps_a_pane_chain_across_evaluations() {
        assert_eq!(pane_candidates(10_000.0).len(), 2);
        let candidates = pane_candidates_for(10_000.0, &keeping_query_time_results());
        assert_eq!(candidates.len(), 3);
        let b2 = &candidates[0].dag;
        let b3 = &candidates[2].dag;
        let chain = node_of(b3, is_chain);
        let with_chain = |materialization: Materialization| {
            let mut dag = b3.clone();
            for node in dag.nodes.iter_mut().filter(|n| n.id == chain) {
                node.materialization = materialization;
            }
            dag
        };
        assert_eq!(
            *b3,
            with_chain(Materialization::MatAtQueryKeptAcrossEvaluations(
                Medium::Memory
            ))
        );
        let recurrence = [repeating(MINUTE)];
        let settings = CostInputs::default();
        let kept = price(b3, &recurrence, &arriving(10_000.0), &settings).unwrap();
        let rebuilt = price(b2, &recurrence, &arriving(10_000.0), &settings).unwrap();
        let scan = node_of(b3, is_scan);
        assert_eq!(scanned_samples(&kept, scan), 10_000 * 60);
        assert_eq!(
            scanned_samples(&rebuilt, node_of(b2, is_scan)),
            10_000 * 300
        );
        assert_eq!(kept.usage.disk_bytes * 5, rebuilt.usage.disk_bytes);
        let Payload::ASAP(ASAPOp::PaneChain { family, .. }) =
            &b3.nodes.iter().find(|n| n.id == chain).unwrap().payload
        else {
            unreachable!()
        };
        // Each pane sees its series' 60 samples: the one minute of 10 000
        // rows per second over 10 000 series it reads.
        let samples = StateInputRows {
            rows_per_state: Some(60),
            non_negative: false,
        };
        let (_, state_bytes) = summary_shape(family, samples);
        let chain_cost = &kept.per_node[&chain];
        assert!(chain_cost.detail.contains("; x 0.0167/s"), "{chain_cost:?}");
        // One state per series: arriving() declares 10 000.
        let between = 4 * 10_000 * state_bytes;
        assert!(chain_cost
            .detail
            .contains(&format!("keeps {between} bytes across evaluations")));
        // On disk, the same N − 1 older panes are kept.
        let on_disk = with_chain(Materialization::MatAtQueryKeptAcrossEvaluations(
            Medium::Disk,
        ));
        let on_disk = price(&on_disk, &recurrence, &arriving(10_000.0), &settings).unwrap();
        assert_eq!(
            on_disk.usage.disk_bytes,
            kept.usage.disk_bytes + between,
            "{on_disk:?}"
        );
        // Shared within one evaluation, nothing is kept between them.
        let shared = with_chain(Materialization::MatAtQuery(Medium::Memory));
        let within_one = price(&shared, &recurrence, &arriving(10_000.0), &settings).unwrap();
        assert_eq!(scanned_samples(&within_one, scan), 10_000 * 300);
        assert!(!within_one.per_node[&chain]
            .detail
            .contains("across evaluations"));
        assert!(within_one.usage.average_memory_bytes < kept.usage.average_memory_bytes);
    }

    /// Without raw data retained, a candidate that scans raw data at query
    /// time is invalid, with the window it would read: B2 (5 min) and B3
    /// (one 1-min pane) are rejected, and B1, which reads raw rows only at
    /// ingestion time, is selected. With raw data retained, all three are
    /// valid.
    #[test]
    fn without_raw_data_only_ingestion_time_scans_are_valid() {
        let candidates = pane_candidates_for(10_000.0, &keeping_query_time_results());
        let select = |deployment: &DeploymentCapabilities| {
            select_plan(
                &candidates,
                &[every_minute()],
                &arriving(10_000.0),
                PlanningModels::builtin().with_deployment_capabilities(deployment),
            )
            .unwrap()
        };
        let retained = select(&keeping_query_time_results());
        assert!(
            retained.rejected.iter().all(|r| !r.cause.failed_a_check()),
            "{retained:?}"
        );
        let dropped = select(&DeploymentCapabilities {
            raw_data_retained: false,
            ..keeping_query_time_results()
        });
        assert_eq!(dropped.selected, candidates[1].id);
        let reasons: BTreeMap<&str, (RejectionCause, &str)> = dropped
            .rejected
            .iter()
            .map(|r| (r.id.as_str(), (r.cause, r.reason.as_str())))
            .collect();
        let unretained = "at query time, but the deployment does not retain raw data";
        assert_eq!(
            reasons[candidates[0].id.as_str()],
            (
                RejectionCause::RawDataNotRetained,
                format!("scans 300000 ms of raw m {unretained}").as_str()
            )
        );
        assert_eq!(
            reasons[candidates[2].id.as_str()],
            (
                RejectionCause::RawDataNotRetained,
                format!("scans 60000 ms of raw m {unretained}").as_str()
            )
        );
    }

    /// Without raw data retained, the rejection lists every query-time
    /// scan with the window it would read, not just the first.
    #[test]
    fn without_raw_data_every_query_time_scan_is_listed() {
        let roots: Vec<(usize, QueryRoot)> = ["sum_over_time(a[5m])", "sum_over_time(b[1m])"]
            .into_iter()
            .map(|q| QueryRoot::Operator(lower_promql(q, AccuracyTarget::exact())))
            .enumerate()
            .collect();
        // The exact candidate of each query.
        let inventory =
            asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates(roots).unwrap();
        let exact = vec![0; inventory.targets.len()];
        let roots: Vec<QueryRoot> =
            asap_logical_asap_optimization::pass1::per_computation_candidates::compose_logical_candidate(
                &inventory, &exact,
            )
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        let mut candidate = optimize_physical_asap("L1", &roots, &[], &data(), false)
            .unwrap()
            .remove(0);
        candidate.id = "P1".into();
        let entries = [entry(repeating(MINUTE)), entry(repeating(MINUTE))];
        let no_raw_data = DeploymentCapabilities {
            raw_data_retained: false,
            ..DeploymentCapabilities::default()
        };
        let error = select_plan(
            &[candidate],
            &entries,
            &data(),
            PlanningModels::builtin().with_deployment_capabilities(&no_raw_data),
        )
        .unwrap_err();
        let SelectionError::NoValidCandidate(rejected) = error else {
            panic!("{error}")
        };
        assert_eq!(
            rejected[0].reason,
            "scans 300000 ms of raw a, 60000 ms of raw b at query time, \
             but the deployment does not retain raw data"
        );
    }

    /// Stored data (at rest, or mixed) with no raw data retained is a
    /// contradiction in the inputs, refused before any candidate is
    /// measured; arriving or undeclared data is not.
    #[test]
    fn stored_data_without_raw_data_retained_is_bad_input() {
        let candidates = pane_candidates(10_000.0);
        let no_raw_data = DeploymentCapabilities {
            raw_data_retained: false,
            ..DeploymentCapabilities::default()
        };
        let select = |arrival: DataArrival| {
            // Each source's own rate, so an unknown arrival is priced too.
            let mut data = DataWorkload { arrival, ..data() };
            for facts in data.sources.values_mut() {
                facts.sample_interval = Some(asap_types::workload::DurationMs(1_000));
            }
            select_plan(
                &candidates,
                &[every_minute()],
                &data,
                PlanningModels::builtin().with_deployment_capabilities(&no_raw_data),
            )
        };
        for arrival in [
            DataArrival::AtRest,
            DataArrival::Mixed {
                rows_per_second: 10_000.0,
            },
        ] {
            let error = select(arrival).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "deployment capabilities: the data is stored ({arrival:?}), \
                     but the deployment does not retain raw data"
                ),
            );
        }
        for arrival in [
            DataArrival::Ingesting {
                rows_per_second: 10_000.0,
            },
            DataArrival::Unknown,
        ] {
            assert!(select(arrival).is_ok());
        }
    }

    fn is_chain(payload: &Payload) -> bool {
        matches!(payload, Payload::ASAP(ASAPOp::PaneChain { .. }))
    }

    /// #509 Example 4, B1: maintained at ingestion time, a chain of N panes
    /// pays the update of the newest pane only, one second of arriving rows
    /// once per second, and keeps N + 1 panes (the N the query merges and
    /// the one being built) per series.
    #[test]
    fn a_chain_at_ingestion_pays_the_newest_pane_and_keeps_n_plus_one_panes() {
        let candidates = pane_candidates(10_000.0);
        let dag = &candidates[1].dag;
        let chain = node_of(dag, is_chain);
        let Payload::ASAP(ASAPOp::PaneChain {
            family, pane_count, ..
        }) = &dag.nodes.iter().find(|n| n.id == chain).unwrap().payload
        else {
            unreachable!()
        };
        assert_eq!(*pane_count, 5);
        // Each pane sees its series' 60 samples: one minute of 10 000 rows
        // per second over 10 000 series.
        let samples = StateInputRows {
            rows_per_state: Some(60),
            non_negative: false,
        };
        let (depth, state_bytes) = summary_shape(family, samples);
        let recurrence = [repeating(MINUTE)];
        let without_memory = unit_costs(|c| c.cost_per_memory_byte_second = 0.0);
        let work = &price(dag, &recurrence, &arriving(10_000.0), &without_memory)
            .unwrap()
            .per_node[&chain];
        assert!(work.detail.contains("; x 1.0000/s"), "{work:?}");
        assert!(
            close(
                work.cost_per_second.unwrap(),
                10_000.0 * (depth + 1.0) * 1e-6
            ),
            "{work:?}"
        );
        let with_memory = &price(
            dag,
            &recurrence,
            &arriving(10_000.0),
            &CostInputs::default(),
        )
        .unwrap()
        .per_node[&chain];
        let kept = 6 * 10_000 * state_bytes;
        assert!(
            close(
                with_memory.cost_per_second.unwrap() - work.cost_per_second.unwrap(),
                1.25e-7 * kept as f64
            ),
            "{with_memory:?}"
        );
        assert!(with_memory.detail.contains(&format!("keeps {kept} bytes")));
    }

    /// The latency check (S6, sketch-bench cost model §5): a query's
    /// latency is its query-time work on one core. Rebuilding the panes at
    /// query time (B2) takes longer than merging the maintained ones (B1);
    /// with a bound between the two, B2 is rejected as invalid with its
    /// latency and B1 is selected.
    #[test]
    fn a_candidate_over_a_querys_latency_bound_is_rejected_with_its_latency() {
        let candidates = pane_candidates(10_000.0);
        let latency = |c: &PhysicalWorkloadPlan| {
            let cost = price(
                &c.dag,
                &[repeating(MINUTE)],
                &arriving(10_000.0),
                &CostInputs::default(),
            )
            .unwrap();
            cost.usage.latency_per_query[0]
        };
        let (b2, b1) = (latency(&candidates[0]), latency(&candidates[1]));
        assert_eq!((b2.compaction_ms, b1.compaction_ms), (0.0, 0.0));
        assert!(b1.query_ms < b2.query_ms, "{b1:?} vs {b2:?}");
        let bound = (b1.query_ms + b2.query_ms) / 2.0;
        let mut bounded = every_minute();
        bounded.requirements.max_latency_ms = Some(bound);
        let selection = select_plan(
            &candidates,
            &[bounded],
            &arriving(10_000.0),
            PlanningModels::builtin(),
        )
        .unwrap();
        assert_eq!(selection.selected, candidates[1].id);
        let rejected = &selection.rejected[0];
        assert_eq!(rejected.id, candidates[0].id);
        assert_eq!(rejected.cause, RejectionCause::LatencyBoundExceeded);
        assert!(
            rejected.reason.starts_with(&format!(
                "q1: latency {:.1} ms per evaluation (compaction 0.0 ms + query",
                b2.total_ms()
            )) && rejected
                .reason
                .ends_with(&format!("is over its {bound} ms bound")),
            "{}",
            rejected.reason
        );
        assert!(!selection.costs.contains_key(&candidates[0].id));
    }

    /// A query evaluated every minute whose evaluation takes longer than a
    /// minute never keeps up, so even without a latency bound the
    /// candidate is invalid. Over data fast enough that rebuilding the
    /// panes at query time (B2) takes over a minute while merging the
    /// maintained ones (B1) does not, B2 is rejected with its latency and
    /// the interval, and B1 is selected.
    #[test]
    fn a_candidate_slower_than_a_querys_repeat_interval_is_rejected_without_a_bound() {
        let rows_per_s = 1e9;
        let candidates = pane_candidates(rows_per_s);
        let latency = |c: &PhysicalWorkloadPlan| {
            price(
                &c.dag,
                &[repeating(MINUTE)],
                &arriving(rows_per_s),
                &CostInputs::default(),
            )
            .unwrap()
            .usage
            .latency_per_query[0]
        };
        let (b2, b1) = (latency(&candidates[0]), latency(&candidates[1]));
        assert!(
            b1.total_ms() < f64::from(MINUTE) && b2.total_ms() > f64::from(MINUTE),
            "{b1:?} vs {b2:?}"
        );
        let unbounded = every_minute();
        assert_eq!(unbounded.requirements.max_latency_ms, None);
        let selection = select_plan(
            &candidates,
            &[unbounded],
            &arriving(rows_per_s),
            PlanningModels::builtin(),
        )
        .unwrap();
        assert_eq!(selection.selected, candidates[1].id);
        let rejected = &selection.rejected[0];
        assert_eq!(rejected.id, candidates[0].id);
        assert_eq!(rejected.cause, RejectionCause::LatencyOverRepeatInterval);
        assert!(rejected.cause.failed_a_check());
        assert_eq!(
            rejected.reason,
            format!(
                "q1: latency {:.1} ms per evaluation (compaction {:.1} ms + query {:.1} ms) is \
                 over its {MINUTE} ms repeat interval, so its evaluations cannot keep up",
                b2.total_ms(),
                b2.compaction_ms,
                b2.query_ms
            )
        );
        assert!(!selection.costs.contains_key(&candidates[0].id));
    }

    /// With no repeat interval (a one-time query) there is nothing to keep
    /// up with: the slow candidate stays valid and is selected when it is
    /// cheaper.
    #[test]
    fn a_one_time_query_has_no_repeat_interval_to_keep_up_with() {
        let rows_per_s = 1e9;
        let candidates = pane_candidates(rows_per_s);
        let once = QueryWorkloadEntry {
            recurrence: QueryRecurrence::OneTime {
                invocations: 1,
                execute_at: None,
            },
            ..every_minute()
        };
        let selection = select_plan(
            &candidates,
            &[once],
            &arriving(rows_per_s),
            PlanningModels::builtin(),
        )
        .unwrap();
        // Run once an hour, rebuilding at query time is the cheaper plan,
        // over a minute per evaluation or not.
        assert_eq!(selection.selected, candidates[0].id);
        assert_eq!(selection.rejected.len(), 1, "{:?}", selection.rejected);
        assert_eq!(selection.rejected[0].id, candidates[1].id);
        assert_eq!(selection.rejected[0].cause, RejectionCause::Costlier);
    }

    /// A query over both its latency bound and its repeat interval is
    /// reported over its bound: the bound is the user's own requirement.
    /// The interval is also checked for a repetition at a declared phase.
    #[test]
    fn a_broken_latency_bound_is_reported_before_the_repeat_interval() {
        let rows_per_s = 1e9;
        let candidates = pane_candidates(rows_per_s);
        let mut bounded = QueryWorkloadEntry {
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
                interval: asap_types::workload::RepetitionInterval(MINUTE),
                evaluation_phase: asap_types::workload::TimestampMs(0),
            }),
            ..every_minute()
        };
        let select = |entry: &QueryWorkloadEntry| {
            select_plan(
                &candidates,
                std::slice::from_ref(entry),
                &arriving(rows_per_s),
                PlanningModels::builtin(),
            )
            .unwrap()
        };
        let unbounded = select(&bounded);
        assert_eq!(
            unbounded.rejected[0].cause,
            RejectionCause::LatencyOverRepeatInterval
        );
        bounded.requirements.max_latency_ms = Some(f64::from(MINUTE));
        let selection = select(&bounded);
        assert_eq!(selection.selected, candidates[1].id);
        assert_eq!(selection.rejected[0].id, candidates[0].id);
        assert_eq!(
            selection.rejected[0].cause,
            RejectionCause::LatencyBoundExceeded
        );
    }

    /// When a maintained state's ingestion work needs more than one core,
    /// its `k` workers' partial copies of the newest window are merged
    /// before the query reads it: `(k − 1)` merges of one operation for
    /// each of its states per window, on the query's chain.
    #[test]
    fn ingestion_work_over_one_core_adds_the_newest_windows_compaction() {
        let latency = |rows_per_s: f64| {
            let candidates = pane_candidates(rows_per_s);
            price(
                &candidates[1].dag,
                &[repeating(MINUTE)],
                &arriving(rows_per_s),
                &CostInputs::default(),
            )
            .unwrap()
            .usage
            .latency_per_query[0]
        };
        assert_eq!(latency(10_000.0).compaction_ms, 0.0);
        let fast = latency(1e10);
        // 10,000 series, one state each per window, 1 ns per merge.
        let merges = fast.compaction_ms / (10_000.0 * 1e-6);
        assert!(
            merges >= 1.0 && (merges - merges.round()).abs() < 1e-6,
            "{fast:?}"
        );
    }

    /// The newest window's compaction merges the `k` workers' partial
    /// sketches with the state's measured query-time merge when the table
    /// has one: each of the `(k − 1)·I` merges takes the measured 1,200 ns
    /// instead of one operation.
    #[test]
    fn compaction_prices_merges_from_the_measured_merge() {
        let candidates = pane_candidates(1e10);
        let dag = &candidates[1].dag;
        let kll = dag
            .nodes
            .iter()
            .find_map(|n| match &n.payload {
                Payload::ASAP(
                    ASAPOp::SummaryAgg {
                        family: FieldDataType::Sketch(kind, _),
                        ..
                    }
                    | ASAPOp::PaneChain {
                        family: FieldDataType::Sketch(kind, _),
                        ..
                    },
                ) => Some(kind.params().clone()),
                _ => None,
            })
            .expect("a KLL build");
        let latency = price(
            dag,
            &[repeating(MINUTE)],
            &arriving(1e10),
            &measured(vec![sketch_entry(OperatorKind::SummaryMerge, kll, 1200.0)]),
        )
        .unwrap()
        .usage
        .latency_per_query[0];
        assert!(latency.compaction_ms > 0.0, "{latency:?}");
        let merges = latency.compaction_ms / 1200e-6;
        assert!(
            merges >= 1.0 && (merges - merges.round()).abs() < 1e-6,
            "{latency:?}"
        );
    }

    // ── Cost models ──────────────────────────────────────────────────────

    /// [`CostInputs`] with `cost_model`.
    fn under(cost_model: CostModel) -> CostInputs {
        CostInputs {
            cost_model,
            ..CostInputs::default()
        }
    }

    /// The candidate's usage is what its nodes are charged for: priced at
    /// one unit for a single resource under cost model 1 (CPU, scanned
    /// bytes, memory byte-seconds or disk), the nodes' prices add up to the
    /// candidate's usage of it.
    #[test]
    fn node_prices_add_up_to_the_candidates_usage() {
        let only = |set: fn(&mut ResourceUnitCost)| {
            let mut prices = ResourceUnitCost {
                cost_per_cpu_op: 0.0,
                cost_per_scanned_byte: 0.0,
                cost_per_memory_byte_second: 0.0,
                cost_per_provisioned_memory_byte_second: 0.0,
                cost_per_provisioned_cpu_core_second: 0.0,
                cost_per_disk_byte_second: 0.0,
            };
            set(&mut prices);
            CostInputs {
                cost_model: CostModel::UsageOverTime(prices),
                ..CostInputs::default()
            }
        };
        // Each resource's unit price and the candidate's usage of it.
        type Resource = (fn(&mut ResourceUnitCost), fn(&ResourceUsage) -> f64);
        let resources: [Resource; 4] = [
            (|p| p.cost_per_cpu_op = 1.0, |u| u.cpu_ops_per_second),
            (
                |p| p.cost_per_scanned_byte = 1.0,
                |u| u.scanned_bytes_per_second,
            ),
            (
                |p| p.cost_per_memory_byte_second = 1.0,
                |u| u.average_memory_bytes,
            ),
            (
                |p| p.cost_per_disk_byte_second = 1.0,
                |u| u.disk_bytes as f64,
            ),
        ];
        for candidate in pane_candidates(10_000.0) {
            for (set, used) in resources {
                let cost = price(
                    &candidate.dag,
                    &[repeating(MINUTE)],
                    &arriving(10_000.0),
                    &only(set),
                )
                .unwrap();
                let charged: f64 = cost
                    .per_node
                    .values()
                    .filter_map(|n| n.cost_per_second)
                    .sum();
                assert!(
                    close(charged, used(&cost.usage)),
                    "{}: {charged} vs {}",
                    candidate.id,
                    used(&cost.usage)
                );
                assert_eq!(cost.usage.latency_per_query.len(), 1);
            }
        }
    }

    /// Cost model 2 prices the candidate's peaks per second: provisioned
    /// memory, cores and disk; its nodes carry no price. B1 (maintained
    /// panes) needs a core for ingestion and one for the query; B2 (rebuilt
    /// at query time) one.
    #[test]
    fn provisioned_capacity_prices_peak_memory_cores_and_disk() {
        let prices = ResourceUnitCost {
            cost_per_disk_byte_second: 1e-9,
            ..ResourceUnitCost::illustrative()
        };
        let settings = under(CostModel::ProvisionedCapacity(prices.clone()));
        let candidates = pane_candidates(10_000.0);
        let cores: Vec<u32> = candidates
            .iter()
            .map(|candidate| {
                let cost = price(
                    &candidate.dag,
                    &[repeating(MINUTE)],
                    &arriving(10_000.0),
                    &settings,
                )
                .unwrap();
                let usage = &cost.usage;
                let expected = prices.cost_per_provisioned_memory_byte_second
                    * usage.peak_memory_bytes as f64
                    + prices.cost_per_provisioned_cpu_core_second * f64::from(usage.peak_cpu_cores)
                    + prices.cost_per_disk_byte_second * usage.disk_bytes as f64;
                assert_eq!(cost.cost_per_second, Some(expected));
                assert!(cost.per_node.values().all(|n| n.cost_per_second.is_none()));
                usage.peak_cpu_cores
            })
            .collect();
        assert_eq!(cores, [1, 2]);
    }

    /// Cost model 3 has no prices; a candidate over the cluster's cores is
    /// invalid with the reason, and the rest are chosen by the objective.
    #[test]
    fn cluster_capacity_rejects_a_candidate_over_its_cores() {
        let limits = |cpu_cores| ClusterLimits {
            memory_bytes: u64::MAX,
            disk_bytes: u64::MAX,
            cpu_cores,
            objective: ClusterObjective::LowestLatency,
        };
        let candidates = pane_candidates(10_000.0);
        let select = |cpu_cores| {
            let settings = under(CostModel::ClusterCapacity(limits(cpu_cores)));
            select_plan(
                &candidates,
                &[every_minute()],
                &arriving(10_000.0),
                PlanningModels::builtin().with_plan_selection(&settings),
            )
            .unwrap()
        };
        let unlimited = select(None);
        assert_eq!(unlimited.selected, candidates[1].id, "B1 merges faster");
        assert!(unlimited
            .costs
            .values()
            .all(|c| c.cost_per_second.is_none()));
        let one_core = select(Some(1));
        assert_eq!(one_core.selected, candidates[0].id);
        let b1 = one_core
            .rejected
            .iter()
            .find(|r| r.id == candidates[1].id)
            .unwrap();
        assert_eq!(b1.cause, RejectionCause::OverClusterCapacity);
        assert_eq!(b1.reason, "2 CPU cores at peak is over the cluster's 1");
    }

    /// Raw data kept on disk for query-time scans is counted once per
    /// source, for the longest window read: two queries over `m` keep its
    /// last 5 minutes, as the 5-minute query alone does.
    #[test]
    fn raw_data_on_disk_is_the_longest_window_per_source() {
        let disk = |queries: &[&str]| {
            let roots: Vec<_> = queries
                .iter()
                .map(|q| QueryRoot::Operator(lower_promql(q, AccuracyTarget::exact())))
                .collect();
            let dag = optimize_physical_asap("L", &roots, &[], &data(), false)
                .unwrap()
                .remove(0)
                .dag;
            priced(&dag, &recurrences(queries.len())).usage.disk_bytes
        };
        let five_minutes = disk(&["sum_over_time(m[5m])"]);
        assert!(five_minutes > disk(&["sum_over_time(m[1m])"]));
        assert_eq!(
            disk(&["sum_over_time(m[1m])", "sum_over_time(m[5m])"]),
            five_minutes
        );
    }

    /// Cost model 3's disk limit: B2 reads its 5-minute window of raw data
    /// at query time and keeps it on disk; B1 keeps none. A cluster with
    /// less disk than B2's raw data leaves only B1.
    #[test]
    fn cluster_capacity_rejects_a_candidate_over_its_disk() {
        let candidates = pane_candidates(10_000.0);
        let select = |disk_bytes| {
            let settings = under(CostModel::ClusterCapacity(ClusterLimits {
                memory_bytes: u64::MAX,
                disk_bytes,
                cpu_cores: None,
                objective: ClusterObjective::LeastMemory,
            }));
            select_plan(
                &candidates,
                &[every_minute()],
                &arriving(10_000.0),
                PlanningModels::builtin().with_plan_selection(&settings),
            )
            .unwrap()
        };
        let unlimited = select(u64::MAX);
        let raw = unlimited.costs[&candidates[0].id].usage.disk_bytes;
        assert!(raw > 0);
        assert_eq!(unlimited.costs[&candidates[1].id].usage.disk_bytes, 0);
        assert_eq!(unlimited.selected, candidates[0].id, "B2 needs less memory");
        let small = select(raw - 1);
        assert_eq!(small.selected, candidates[1].id);
        let b2 = small
            .rejected
            .iter()
            .find(|r| r.id == candidates[0].id)
            .unwrap();
        assert_eq!(b2.cause, RejectionCause::OverClusterCapacity);
        assert_eq!(
            b2.reason,
            format!("disk {raw} bytes is over the cluster's {} bytes", raw - 1)
        );
    }

    /// State materialized on disk is counted as disk, not memory: the KLL
    /// state kept on disk at ingestion time (its window being built and the
    /// completed one) or for one evaluation at query time adds its bytes to
    /// disk, nothing to average memory, and costs only its disk price.
    #[test]
    fn state_on_disk_is_counted_as_disk_not_memory() {
        use asap_types::physical::Medium;
        let settings = unit_costs(|c| c.cost_per_disk_byte_second = 1e-9);
        let work_only = unit_costs(|c| c.cost_per_memory_byte_second = 0.0);
        let state = |dag: &PhysicalASAPDAG| node_of(dag, is_build);
        for (on_disk, in_memory) in [
            (Materialization::MatAtIngestion(Medium::Disk), AT_INGESTION),
            (Materialization::MatAtQuery(Medium::Disk), AT_QUERY),
        ] {
            let (disk_dag, memory_dag) = (kll_dag(on_disk), kll_dag(in_memory));
            let disk = price(&disk_dag, &recurrences(1), &data(), &settings).unwrap();
            let memory = price(&memory_dag, &recurrences(1), &data(), &settings).unwrap();
            // The bytes kept are the same, on disk instead of in memory.
            let bytes = disk.usage.disk_bytes - memory.usage.disk_bytes;
            assert!(bytes > 0);
            assert!(
                memory.per_node[&state(&memory_dag)]
                    .detail
                    .contains(&format!(" {bytes} bytes")),
                "{in_memory:?}: {bytes}"
            );
            assert!(
                disk.usage.average_memory_bytes < memory.usage.average_memory_bytes,
                "{on_disk:?}"
            );
            assert_eq!(
                disk.usage.peak_memory_bytes + bytes,
                memory.usage.peak_memory_bytes,
                "{on_disk:?}"
            );
            // On disk, the state costs only its disk price beside the work.
            let work = price(&memory_dag, &recurrences(1), &data(), &work_only)
                .unwrap()
                .per_node[&state(&memory_dag)]
                .cost_per_second
                .unwrap();
            let on_disk_node = &disk.per_node[&state(&disk_dag)];
            assert!(
                close(
                    on_disk_node.cost_per_second.unwrap(),
                    work + 1e-9 * bytes as f64
                ),
                "{on_disk_node:?}"
            );
        }
    }

    /// Under every cluster objective, two candidates with the same usage
    /// tie, and the earlier is selected; the later is reported as tied.
    /// Under the Pareto front neither dominates the other: both are on it.
    #[test]
    fn cluster_objectives_keep_the_earlier_of_tied_candidates() {
        let b2 = pane_candidates(10_000.0).remove(0);
        let twin = PhysicalWorkloadPlan {
            id: format!("{}-twin", b2.id),
            ..b2.clone()
        };
        let candidates = [b2, twin];
        for objective in [
            ClusterObjective::LowestLatency,
            ClusterObjective::LeastMemory,
            ClusterObjective::ParetoFront,
        ] {
            let settings = under(CostModel::ClusterCapacity(ClusterLimits {
                memory_bytes: u64::MAX,
                disk_bytes: u64::MAX,
                cpu_cores: None,
                objective,
            }));
            let selection = select_plan(
                &candidates,
                &[every_minute()],
                &arriving(10_000.0),
                PlanningModels::builtin().with_plan_selection(&settings),
            )
            .unwrap();
            assert_eq!(selection.selected, candidates[0].id, "{objective:?}");
            let twin = &selection.rejected[0];
            assert_eq!(twin.id, candidates[1].id);
            assert_eq!(twin.cause, RejectionCause::TiedLostOnEnumerationOrder);
            assert!(
                twin.reason.starts_with(&format!(
                    "tied with the selected candidate {}",
                    candidates[0].id
                )),
                "{objective:?}: {}",
                twin.reason
            );
            let expected_front = match objective {
                ClusterObjective::ParetoFront => {
                    vec![candidates[0].id.clone(), candidates[1].id.clone()]
                }
                _ => Vec::new(),
            };
            assert_eq!(selection.front, expected_front, "{objective:?}");
        }
    }

    /// Every Pass 2 window-composition candidate of a per-series p50 over
    /// 2 min and a p99 over 5 min, both every minute, with both builds in
    /// 1-min panes: `shared` picks one chain read by both (the 2-min query
    /// merging its newest 2 panes) or a chain each. Its physical candidate
    /// with every chain maintained at ingestion time.
    fn nested_windows_at_ingestion(shared: bool, rows_per_s: f64) -> PhysicalWorkloadPlan {
        let queries = [
            ("quantile_over_time(0.5, m[2m])", MINUTE),
            ("quantile_over_time(0.99, m[5m])", MINUTE),
        ];
        shared_windows_at_ingestion(queries, shared, rows_per_s)
    }

    /// [`nested_windows_at_ingestion`] for two queries, each with its own
    /// repeat interval in ms.
    fn shared_windows_at_ingestion(
        queries: [(&str, u32); 2],
        shared: bool,
        rows_per_s: f64,
    ) -> PhysicalWorkloadPlan {
        use asap_logical_asap_optimization::pass1::per_computation_candidates as pcc;
        let inventory = pcc::generate_per_computation_candidates(
            queries
                .iter()
                .enumerate()
                .map(|(q, (query, _))| (q, QueryRoot::Operator(lower_promql(query, p99_target()))))
                .collect(),
        )
        .unwrap();
        let kll: Vec<usize> = inventory
            .targets
            .iter()
            .map(|target| {
                target
                    .ways_to_compute
                    .iter()
                    .position(|c| {
                        matches!(
                            c,
                            pcc::HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll)
                        )
                    })
                    .unwrap()
            })
            .collect();
        let roots: Vec<_> = pcc::compose_logical_candidate(&inventory, &kll)
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        let rule_candidates =
            asap_logical_asap_optimization::pass2::asap_aware_cse::pane_candidates(
                &roots,
                &queries.map(|(_, interval_ms)| Some(u64::from(interval_ms))),
            );
        let roots =
            asap_logical_asap_optimization::pass2::asap_aware_cse::window_composition_pass2_candidates(
                &roots,
                &rule_candidates,
            )
            .unwrap()
            .into_iter()
            .find(|c| c.pane_candidates.len() == 2 && c.shared_chains.is_empty() != shared)
            .unwrap()
            .roots
            .unwrap();
        optimize_physical_asap(
            "L1",
            &roots,
            &queries.map(|(_, interval_ms)| QueryWorkloadEntry {
                predictability: asap_types::workload::Predictability::Predictable {
                    known_at: None,
                },
                ..entry_with(p99_target(), repeating(interval_ms))
            }),
            &arriving(rows_per_s),
            false,
        )
        .unwrap()
        .into_iter()
        .find(|c| {
            c.dag.nodes.iter().all(|n| match n.payload {
                Payload::ASAP(ASAPOp::PaneChain { .. }) => {
                    matches!(n.materialization, Materialization::MatAtIngestion(_))
                }
                _ => n.materialization == Materialization::NotMaterialized,
            })
        })
        .unwrap()
    }

    /// One chain of 5 panes maintained at ingestion time serves a 5-min and
    /// a 2-min window: the 2-min query's merge reads its newest 2 panes per
    /// series, the 5-min query's all 5, and the plan costs less than a chain
    /// per query, since each arriving row updates one pane instead of two.
    #[test]
    fn a_merge_of_the_newest_panes_is_priced_by_the_panes_it_reads() {
        let rows_per_s = 10_000.0;
        let shared = nested_windows_at_ingestion(true, rows_per_s);
        let chains: Vec<_> = shared
            .dag
            .nodes
            .iter()
            .filter(|n| is_chain(&n.payload))
            .collect();
        assert_eq!(chains.len(), 1);
        let recurrence = [repeating(MINUTE), repeating(MINUTE)];
        let settings = CostInputs::default();
        let cost = price(&shared.dag, &recurrence, &arriving(rows_per_s), &settings).unwrap();
        let states_read = |newest: Option<u32>| -> u64 {
            let merge = shared
                .dag
                .nodes
                .iter()
                .find(|n| {
                    matches!(&n.payload, Payload::ASAP(ASAPOp::SummaryMerge { newest_panes, .. })
                        if *newest_panes == newest)
                })
                .unwrap();
            let detail = &cost.per_node[&merge.id].detail;
            detail
                .strip_prefix("merge ")
                .and_then(|rest| rest.split(' ').next())
                .and_then(|states| states.parse().ok())
                .unwrap_or_else(|| panic!("not a merge: {detail}"))
        };
        let (newest_two, all_five) = (states_read(Some(2)), states_read(None));
        assert!(all_five > 0);
        assert_eq!(newest_two * 5, all_five * 2);
        let separate = nested_windows_at_ingestion(false, rows_per_s);
        let separate_cost =
            price(&separate.dag, &recurrence, &arriving(rows_per_s), &settings).unwrap();
        assert!(
            cost.cost_per_second.unwrap() < separate_cost.cost_per_second.unwrap(),
            "{:?} vs {:?}",
            cost.cost_per_second,
            separate_cost.cost_per_second
        );
    }

    /// A 10-min quantile every minute (10 × 1-min panes) and one every 5
    /// min (2 × 5-min panes), maintained at ingestion time: one chain of
    /// 1-min panes serves both, the 5-min query merging all 10 panes once
    /// per 5 min, and costs less than a chain per query.
    #[test]
    fn queries_at_different_intervals_share_one_chain_at_ingestion_time() {
        let rows_per_s = 10_000.0;
        let queries = [
            ("quantile_over_time(0.5, m[10m])", MINUTE),
            ("quantile_over_time(0.99, m[10m])", 5 * MINUTE),
        ];
        let shared = shared_windows_at_ingestion(queries, true, rows_per_s);
        let chains: Vec<_> = shared
            .dag
            .nodes
            .iter()
            .filter(|n| is_chain(&n.payload))
            .collect();
        assert_eq!(chains.len(), 1);
        assert!(matches!(
            chains[0].payload,
            Payload::ASAP(ASAPOp::PaneChain {
                pane_width_ms: 60_000,
                pane_count: 10,
                ..
            })
        ));
        let recurrence = [repeating(MINUTE), repeating(5 * MINUTE)];
        let settings = CostInputs::default();
        let cost = price(&shared.dag, &recurrence, &arriving(rows_per_s), &settings).unwrap();
        let separate = shared_windows_at_ingestion(queries, false, rows_per_s);
        let separate_cost =
            price(&separate.dag, &recurrence, &arriving(rows_per_s), &settings).unwrap();
        assert!(
            cost.cost_per_second.unwrap() < separate_cost.cost_per_second.unwrap(),
            "{:?} vs {:?}",
            cost.cost_per_second,
            separate_cost.cost_per_second
        );
        assert!(cost.usage.average_memory_bytes < separate_cost.usage.average_memory_bytes);
        // Each query merges all 10 panes per series at its own rate: once
        // a minute and once per 5 min.
        let mut merges: Vec<(u64, String)> = shared
            .dag
            .nodes
            .iter()
            .filter(|n| matches!(n.payload, Payload::ASAP(ASAPOp::SummaryMerge { .. })))
            .map(|n| {
                let detail = &cost.per_node[&n.id].detail;
                let states = detail
                    .strip_prefix("merge ")
                    .and_then(|rest| rest.split(' ').next())
                    .and_then(|states| states.parse().ok())
                    .unwrap_or_else(|| panic!("not a merge: {detail}"));
                let rate = detail.split("; x ").nth(1).unwrap_or_default().to_string();
                (states, rate)
            })
            .collect();
        merges.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(merges.len(), 2);
        assert_eq!(merges[0].0, merges[1].0);
        assert!(merges[0].1.starts_with("0.0033/s"), "{merges:?}");
        assert!(merges[1].1.starts_with("0.0167/s"), "{merges:?}");
    }

    /// A 1-hour quantile every minute (60 × 1-min panes) and every 45 s
    /// (80 × 45-s panes) can share only a chain at their gcd, 240 × 15-s
    /// panes. Maintained at ingestion time it keeps more states than both
    /// per-query chains together, so it is priced higher than the
    /// per-query chains maintained at ingestion time.
    #[test]
    fn a_shared_chain_at_a_fine_gcd_is_offered_but_costs_more() {
        let rows_per_s = 10_000.0;
        let queries = [
            ("quantile_over_time(0.5, m[1h])", MINUTE),
            ("quantile_over_time(0.99, m[1h])", 45_000),
        ];
        let shared = shared_windows_at_ingestion(queries, true, rows_per_s);
        let chain = node_of(&shared.dag, is_chain);
        let chain = shared.dag.nodes.iter().find(|n| n.id == chain).unwrap();
        assert!(matches!(
            chain.payload,
            Payload::ASAP(ASAPOp::PaneChain {
                pane_width_ms: 15_000,
                pane_count: 240,
                ..
            })
        ));
        let separate = shared_windows_at_ingestion(queries, false, rows_per_s);
        let recurrence = [repeating(MINUTE), repeating(45_000)];
        let settings = CostInputs::default();
        let price_of = |candidate: &PhysicalWorkloadPlan| {
            price(
                &candidate.dag,
                &recurrence,
                &arriving(rows_per_s),
                &settings,
            )
            .unwrap()
            .cost_per_second
            .unwrap()
        };
        assert!(
            price_of(&shared) > price_of(&separate),
            "{} vs {}",
            price_of(&shared),
            price_of(&separate)
        );
    }

    /// With more than one ingest worker, each worker beyond the first holds
    /// its own partial copy of a maintained state's newest window: B1's
    /// peak memory grows by (k − 1) panes of states, its average memory
    /// (and so cost model 1's price) does not.
    #[test]
    fn partial_copies_of_the_newest_window_add_to_peak_memory() {
        let rows_per_s = 1e9;
        let b1 = pane_candidates(rows_per_s).remove(1);
        let chain = node_of(&b1.dag, is_chain);
        let cost = price(
            &b1.dag,
            &[repeating(MINUTE)],
            &arriving(rows_per_s),
            &CostInputs::default(),
        )
        .unwrap();
        let detail = &cost.per_node[&chain].detail;
        let workers = cost.usage.peak_cpu_cores - 1;
        assert!(workers > 1, "{workers}");
        // The chain keeps 6 panes (5 read and the one being built).
        let kept: u64 = detail
            .split("; keeps ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|bytes| bytes.parse().ok())
            .unwrap_or_else(|| panic!("{detail}"));
        let copies = workers - 1;
        let bytes = u64::from(copies) * (kept / 6);
        assert!(
            detail.contains(&format!(
                "; {copies} partial copies of its newest window, {bytes} bytes"
            )),
            "{detail}"
        );
        // At the peak, not on average: the kept bytes are B1's only
        // averaged memory beside negligible query-time holds.
        assert!(cost.usage.peak_memory_bytes >= kept + bytes);
        assert!(cost.usage.average_memory_bytes < (kept + bytes) as f64);
    }

    /// The KLL build of a `quantile_over_time` candidate, with its sketch
    /// parameters, and the candidate.
    fn kll_build() -> (PhysicalWorkloadPlan, PhysicalASAPNodeId, SketchParams) {
        let (candidates, _) = per_series_kll(AccuracyScope::PerValue);
        let candidate = candidates.into_iter().next().unwrap();
        let (build, sketch) = candidate
            .dag
            .nodes
            .iter()
            .find_map(|n| match &n.payload {
                Payload::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) => Some((n.id, kind.params().clone())),
                _ => None,
            })
            .unwrap();
        (candidate, build, sketch)
    }

    /// An entry pricing `operator` at both phases on `sketch`.
    fn sketch_entry(
        operator: OperatorKind,
        sketch: SketchParams,
        ns_per_unit: f64,
    ) -> OperatorCost {
        OperatorCost {
            operator,
            phase: None,
            sketch: Some(sketch),
            exact: None,
            measure: None,
            ns_per_unit,
            ns_per_invocation: 0.0,
            memory_bytes_per_state: None,
        }
    }

    /// An entry pricing `operator` at `phase` with no family.
    fn plain_entry(
        operator: OperatorKind,
        phase: Option<ExecutionTiming>,
        ns_per_unit: f64,
    ) -> OperatorCost {
        OperatorCost {
            operator,
            phase,
            sketch: None,
            exact: None,
            measure: None,
            ns_per_unit,
            ns_per_invocation: 0.0,
            memory_bytes_per_state: None,
        }
    }

    /// Settings pricing from `operators`.
    fn measured(operators: Vec<OperatorCost>) -> CostInputs {
        CostInputs {
            measured_operation_costs: MeasuredOperationCosts {
                operators,
                ..MeasuredOperationCosts::default()
            },
            ..CostInputs::default()
        }
    }

    /// A measured update time for the candidate's KLL prices that build
    /// from it (rows × measured ns, counted in operations of
    /// `ms_per_cpu_op`) and reports `Measured`; every other node keeps its
    /// formulas, reports `Analytical`, and has its CPU scaled by the
    /// calibration factor, measured ÷ formula time of the build: 250 ns
    /// against the formula's one 1-ns operation per row.
    #[test]
    fn a_measured_entry_prices_its_node_and_calibrates_the_others() {
        let (candidate, build, sketch) = kll_build();
        // Scanning is free here, so each node's time is its CPU, which the
        // factor scales (it leaves scanned bytes unscaled).
        let cpu_time_only = |mut settings: CostInputs| {
            settings.resource_speed.ms_per_scanned_byte = 0.0;
            settings
        };
        let before = price(
            &candidate.dag,
            &recurrences(1),
            &data(),
            &cpu_time_only(CostInputs::default()),
        )
        .unwrap();
        let settings = cpu_time_only(measured(vec![sketch_entry(
            OperatorKind::SummaryAgg,
            sketch.clone(),
            250.0,
        )]));
        let after = price(&candidate.dag, &recurrences(1), &data(), &settings).unwrap();
        for (id, node) in &after.per_node {
            let was = before.per_node[id].cpu_core_seconds_per_second;
            if *id == build {
                assert_eq!(node.price_source, NodePriceSource::Measured);
                assert!(node.detail.contains("x 250.000 ns"), "{}", node.detail);
            } else {
                assert_eq!(node.price_source, NodePriceSource::Analytical, "node {id}");
            }
            assert!(
                close(node.cpu_core_seconds_per_second, 250.0 * was),
                "node {id}: {} vs {was}",
                node.cpu_core_seconds_per_second
            );
        }
        assert_eq!(before.source, COST_SOURCE);
        assert_eq!(after.source, MIXED_COST_SOURCE);
        // The candidate's CPU grows linearly in the measured time.
        let cpu_at = |ns: f64| {
            let settings = measured(vec![sketch_entry(
                OperatorKind::SummaryAgg,
                sketch.clone(),
                ns,
            )]);
            price(&candidate.dag, &recurrences(1), &data(), &settings)
                .unwrap()
                .usage
                .cpu_ops_per_second
        };
        assert!(close(
            (cpu_at(250.0) - cpu_at(100.0)) / (cpu_at(400.0) - cpu_at(100.0)),
            0.5
        ));
    }

    /// A KLL with `k` = 469, as Stage 3 sizes it from the data, is priced
    /// between the measured `k` = 200 and `k` = 1024, linearly in the log
    /// of the state's size; a measured `k` matches exactly whatever failure
    /// probability it was sized for, and a `k` beyond the measured ones
    /// takes the nearest one's price.
    #[test]
    fn a_kll_between_two_measured_sizes_is_interpolated() {
        let kll = |k, failure_probability| SketchParams::Kll {
            k,
            failure_probability,
        };
        let table = MeasuredOperationCosts {
            operators: vec![
                sketch_entry(OperatorKind::SummaryAgg, kll(200, None), 100.0),
                sketch_entry(OperatorKind::SummaryAgg, kll(1024, None), 200.0),
            ],
            ..MeasuredOperationCosts::default()
        };
        let size = |params: &SketchParams| sketch_shape(params, StateInputRows::UNKNOWN).1 as f64;
        let at = |k| {
            lookup(
                &table,
                OperatorKind::SummaryAgg,
                ExecutionTiming::IngestionTime,
                StateFamily::Sketch(&kll(k, Some(0.001))),
                size,
            )
            .unwrap()
            .ns_per_unit
        };
        let expected = 100.0
            + 100.0 * (size(&kll(469, None)).ln() - size(&kll(200, None)).ln())
                / (size(&kll(1024, None)).ln() - size(&kll(200, None)).ln());
        assert!(close(at(469), expected), "{} vs {expected}", at(469));
        assert!(at(469) > 100.0 && at(469) < 200.0);
        assert_eq!(at(200), 100.0);
        assert_eq!(at(1024), 200.0);
        assert_eq!(at(4096), 200.0);
        assert_eq!(at(50), 100.0);
        // Another sketch, operator or family has no entry.
        assert!(lookup(
            &table,
            OperatorKind::SummaryMerge,
            ExecutionTiming::QueryTime,
            StateFamily::Sketch(&kll(469, None)),
            size,
        )
        .is_none());
        assert!(lookup(
            &table,
            OperatorKind::SummaryAgg,
            ExecutionTiming::QueryTime,
            StateFamily::Sketch(&SketchParams::DDSketch { alpha: 0.01 }),
            size,
        )
        .is_none());
    }

    /// An entry for the node's own phase wins over one for both phases.
    #[test]
    fn a_phase_entry_wins_over_one_for_both_phases() {
        let table = MeasuredOperationCosts {
            operators: vec![
                plain_entry(OperatorKind::Scan, None, 20.0),
                plain_entry(OperatorKind::Scan, Some(ExecutionTiming::QueryTime), 150.0),
            ],
            ..MeasuredOperationCosts::default()
        };
        let at = |phase| {
            lookup(&table, OperatorKind::Scan, phase, StateFamily::None, |_| {
                1.0
            })
            .unwrap()
            .ns_per_unit
        };
        assert_eq!(at(ExecutionTiming::QueryTime), 150.0);
        assert_eq!(at(ExecutionTiming::IngestionTime), 20.0);
    }

    /// The physical candidates of a per-series p99 over 5 min, queried every
    /// minute over data arriving at 10,000 rows per second: each Stage 1
    /// choice (the exact rebuild from raw data samples, KLL, DDSketch, ...)
    /// with each Stage 2 materialization, ids `P<choice>-m<k>`.
    fn quantile_candidates() -> Vec<PhysicalWorkloadPlan> {
        let root = lower_promql("quantile_over_time(0.99, m[5m])", p99_target());
        let inventory = asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        let mut out = Vec::new();
        for choice in 0..inventory.targets[0].choice_count() {
            let roots: Vec<_> = asap_logical_asap_optimization::pass1::per_computation_candidates::compose_logical_candidate(
                &inventory,
                &[choice],
            )
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect();
            let candidates =
                optimize_physical_asap("L", &roots, &[every_minute()], &arriving(10_000.0), false)
                    .unwrap();
            for (k, mut candidate) in candidates.into_iter().enumerate() {
                candidate.id = format!("P{}-m{k}", choice + 1);
                out.push(candidate);
            }
        }
        out
    }

    /// Whether `candidate` builds no summary: it rebuilds the quantile
    /// exactly from raw data samples.
    fn exact_rebuild(candidate: &PhysicalWorkloadPlan) -> bool {
        !candidate.dag.nodes.iter().any(|n| {
            matches!(
                n.payload,
                Payload::ASAP(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
            )
        })
    }

    /// The exact rebuild of [`quantile_candidates`] and the KLL pane
    /// candidates ([`pane_candidates`]: B2 rebuilt at query time, B1
    /// maintained at ingestion time), with the KLL's parameters.
    fn exact_and_kll() -> (Vec<PhysicalWorkloadPlan>, SketchParams) {
        let mut candidates: Vec<_> = quantile_candidates()
            .into_iter()
            .filter(exact_rebuild)
            .collect();
        assert_eq!(candidates.len(), 1);
        candidates[0].id = "EXACT".to_string();
        candidates.extend(pane_candidates(10_000.0));
        let kll = candidates[2]
            .dag
            .nodes
            .iter()
            .find_map(|n| match &n.payload {
                Payload::ASAP(ASAPOp::PaneChain {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) => Some(kind.params().clone()),
                _ => None,
            })
            .expect("a KLL pane chain");
        (candidates, kll)
    }

    /// Cost model 1 pricing CPU only (1 per ms of one core): the tests
    /// below compare plans by the CPU their tables price.
    fn cpu_only(operators: Vec<OperatorCost>) -> CostInputs {
        CostInputs {
            cost_model: CostModel::UsageOverTime(ResourceUnitCost {
                cost_per_cpu_op: 1e-6,
                cost_per_scanned_byte: 1e-7,
                cost_per_memory_byte_second: 0.0,
                cost_per_provisioned_memory_byte_second: 0.0,
                cost_per_provisioned_cpu_core_second: 0.0,
                cost_per_disk_byte_second: 0.0,
            }),
            ..measured(operators)
        }
    }

    /// The candidate Stage 3 selects among `candidates` for one p99 every
    /// minute over 10,000 rows per second.
    fn selected(candidates: &[PhysicalWorkloadPlan], settings: &CostInputs) -> Selection {
        select_plan(
            candidates,
            &[every_minute()],
            &arriving(10_000.0),
            PlanningModels::builtin().with_plan_selection(settings),
        )
        .unwrap()
    }

    /// The candidate cheapest when every node without an entry keeps its
    /// formulas' price (no calibration), as Stage 3 priced before.
    fn cheapest_uncalibrated(candidates: &[PhysicalWorkloadPlan], settings: &CostInputs) -> String {
        let cost = |candidate: &PhysicalWorkloadPlan| {
            price_at_scale(
                &candidate.dag,
                &node_map(&candidate.dag),
                &kept_pane_chains(&candidate.dag),
                &[repeating(MINUTE)],
                &[],
                &arriving(10_000.0),
                settings,
                1.0,
            )
            .unwrap()
            .0
            .cost_per_second
            .unwrap()
        };
        candidates
            .iter()
            .min_by(|a, b| cost(a).total_cmp(&cost(b)))
            .unwrap()
            .id
            .clone()
    }

    /// Calibration keeps the unmeasured nodes on the measured scale. A
    /// table measuring the KLL as expensive (150 ns per row) and one exact
    /// operator as cheap (the time range, 2 ns per row), but not the exact
    /// plan's scan and aggregate, made the exact rebuild from raw data samples
    /// look cheapest while the formulas kept its scan and aggregate at
    /// their 1-ns scale; with the calibration factor the KLL maintained at
    /// ingestion time (B1) is selected. With the full table, the exact
    /// rebuild is selected when it really is cheaper (1 ns per scanned
    /// sample and aggregated row), and B1 when raw reads and the exact
    /// quantile cost 100 and 50 ns.
    #[test]
    fn calibration_keeps_unmeasured_nodes_on_the_measured_scale() {
        let (candidates, kll) = exact_and_kll();
        let quantile = |ns_per_unit| OperatorCost {
            measure: Some("quantile".to_string()),
            ..plain_entry(OperatorKind::Aggregate, None, ns_per_unit)
        };
        let kll_entries = || {
            vec![
                sketch_entry(OperatorKind::PaneChain, kll.clone(), 150.0),
                sketch_entry(OperatorKind::SummaryMerge, kll.clone(), 2_000.0),
                sketch_entry(OperatorKind::SummaryEstimate, kll.clone(), 5_000.0),
                plain_entry(OperatorKind::TimeRange, None, 2.0),
                plain_entry(OperatorKind::Project, None, 1.0),
            ]
        };
        let partial = cpu_only(kll_entries());
        assert_eq!(cheapest_uncalibrated(&candidates, &partial), "EXACT");
        let selection = selected(&candidates, &partial);
        assert_eq!(selection.selected, "P1-m1", "{:?}", selection.rejected);
        let full = |scan_ns, aggregate_ns| {
            let mut entries = kll_entries();
            entries.push(plain_entry(OperatorKind::Scan, None, scan_ns));
            entries.push(quantile(aggregate_ns));
            cpu_only(entries)
        };
        assert_eq!(selected(&candidates, &full(1.0, 1.0)).selected, "EXACT");
        assert_eq!(selected(&candidates, &full(100.0, 50.0)).selected, "P1-m1");
    }

    /// The calibration factor is one heuristic, so a partial table can
    /// still mis-rank plans: if the exact plan's scan and quantile really
    /// cost 40 and 20 ns, the full table selects the exact rebuild, while
    /// the partial table of
    /// [`calibration_keeps_unmeasured_nodes_on_the_measured_scale`] prices
    /// them at the KLL's ratio and selects B1, which then costs more than
    /// the exact rebuild under the full table.
    #[test]
    fn a_partial_table_can_still_mis_rank_plans() {
        let (candidates, kll) = exact_and_kll();
        let partial = vec![
            sketch_entry(OperatorKind::PaneChain, kll.clone(), 150.0),
            sketch_entry(OperatorKind::SummaryMerge, kll.clone(), 2_000.0),
            sketch_entry(OperatorKind::SummaryEstimate, kll.clone(), 5_000.0),
            plain_entry(OperatorKind::TimeRange, None, 2.0),
            plain_entry(OperatorKind::Project, None, 1.0),
        ];
        let mut full = partial.clone();
        full.push(plain_entry(OperatorKind::Scan, None, 40.0));
        full.push(OperatorCost {
            measure: Some("quantile".to_string()),
            ..plain_entry(OperatorKind::Aggregate, None, 20.0)
        });
        assert_eq!(selected(&candidates, &cpu_only(partial)).selected, "P1-m1");
        let full = selected(&candidates, &cpu_only(full));
        assert_eq!(full.selected, "EXACT");
        let cost = |id: &str| full.costs[id].cost_per_second.unwrap();
        assert!(cost("P1-m1") > cost("EXACT"));
    }

    /// Reading raw data samples from the database holding them is the
    /// deployment's cost. With every operator measured, the exact rebuild
    /// from raw data samples at query time is the cheapest plan for a p99 every
    /// minute while its raw reads are free (0 ns per sample fetched and
    /// decoded at query time); priced at 200 ns per sample, it costs more
    /// than the KLL maintained at ingestion time (B1), which is selected.
    #[test]
    fn external_raw_reads_make_the_query_time_raw_plan_costlier() {
        let (candidates, kll) = exact_and_kll();
        let table = |raw_read_ns| {
            cpu_only(vec![
                plain_entry(
                    OperatorKind::Scan,
                    Some(ExecutionTiming::IngestionTime),
                    20.0,
                ),
                plain_entry(
                    OperatorKind::Scan,
                    Some(ExecutionTiming::QueryTime),
                    raw_read_ns,
                ),
                plain_entry(OperatorKind::TimeRange, None, 1.0),
                plain_entry(OperatorKind::Project, None, 1.0),
                OperatorCost {
                    measure: Some("quantile".to_string()),
                    ..plain_entry(OperatorKind::Aggregate, None, 10.0)
                },
                sketch_entry(OperatorKind::PaneChain, kll.clone(), 30.0),
                sketch_entry(OperatorKind::SummaryMerge, kll.clone(), 500.0),
                sketch_entry(OperatorKind::SummaryEstimate, kll.clone(), 1_000.0),
            ])
        };
        let free = selected(&candidates, &table(0.0));
        assert_eq!(free.selected, "EXACT", "{:?}", free.rejected);
        let priced = selected(&candidates, &table(200.0));
        assert_eq!(priced.selected, "P1-m1", "{:?}", priced.rejected);
        let cost = |selection: &Selection, id: &str| selection.costs[id].cost_per_second.unwrap();
        assert!(cost(&priced, "EXACT") > cost(&priced, "P1-m1"));
        assert!(cost(&priced, "EXACT") > cost(&free, "EXACT"));
    }

    /// A calibration factor below 1 (here a KLL build measured at 0.01 ns
    /// per row against the formula's 1 ns) scales only the CPU of the
    /// nodes without an entry, never their scanned bytes, so no node's CPU
    /// goes negative: the query-time scan keeps its scanned bytes and
    /// takes at least their scan time.
    #[test]
    fn a_calibration_factor_below_one_never_makes_cpu_negative() {
        let (candidate, _, sketch) = kll_build();
        let before = price(
            &candidate.dag,
            &recurrences(1),
            &data(),
            &CostInputs::default(),
        )
        .unwrap();
        let settings = measured(vec![sketch_entry(OperatorKind::SummaryAgg, sketch, 0.01)]);
        let after = price(&candidate.dag, &recurrences(1), &data(), &settings).unwrap();
        assert!(after.usage.cpu_ops_per_second >= 0.0, "{:?}", after.usage);
        for (id, node) in &after.per_node {
            assert!(
                node.cpu_core_seconds_per_second >= 0.0,
                "node {id}: {}",
                node.detail
            );
            assert!(node.cost_per_second.unwrap() >= 0.0, "node {id}");
        }
        assert_eq!(
            after.usage.scanned_bytes_per_second,
            before.usage.scanned_bytes_per_second
        );
    }

    /// Each query request pays the table's `ns_per_evaluation` once: on
    /// the node the query reads first, in its CPU and its latency. (Memory
    /// is free here: the longer evaluation would hold its memory longer.)
    #[test]
    fn each_request_pays_the_per_evaluation_cost() {
        let (candidate, _, _) = kll_build();
        let free_memory = unit_costs(|c| c.cost_per_memory_byte_second = 0.0);
        let before = price(&candidate.dag, &recurrences(1), &data(), &free_memory).unwrap();
        let settings = CostInputs {
            measured_operation_costs: MeasuredOperationCosts {
                ns_per_evaluation: 1e6,
                ..MeasuredOperationCosts::default()
            },
            ..free_memory
        };
        let after = price(&candidate.dag, &recurrences(1), &data(), &settings).unwrap();
        // 1 ms per request, a request every 10 s.
        assert!(close(
            after.usage.latency_per_query[0].query_ms - before.usage.latency_per_query[0].query_ms,
            1.0
        ));
        let core_seconds = |cost: &CandidateCost| {
            cost.per_node
                .values()
                .map(|n| n.cpu_core_seconds_per_second)
                .sum::<f64>()
        };
        assert!(close(
            core_seconds(&after) - core_seconds(&before),
            1e-3 / 10.0
        ));
        assert!(close(
            after.cost_per_second.unwrap() - before.cost_per_second.unwrap(),
            0.1
        ));
    }

    // A literal query has no operator to own its fee, but still incurs one
    // request's CPU, latency and price; range steps do not multiply that fee.
    #[test]
    fn literal_requests_pay_overhead_without_operator_nodes() {
        use asap_types::workload::{DemandEstimate, Rate};
        let roots = [QueryRoot::Scalar(ScalarExpr::Literal(
            ScalarValue::Float64(1.0),
        ))];
        let recurrence = QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(DemandEstimate {
            expected_rate: Rate(2000.0),
            observed_at: None,
            valid_for: None,
        }));
        let entries = [entry_with(AccuracyTarget::exact(), recurrence.clone())];
        let candidate = optimize_physical_asap("literal", &roots, &entries, &data(), false)
            .unwrap()
            .remove(0);
        assert!(candidate.dag.nodes.is_empty());
        let mut settings = CostInputs::default();
        settings.measured_operation_costs.ns_per_evaluation = 2_000_000.0;
        for range in [
            None,
            Some(RangeEvaluation {
                step_ms: 1000,
                span_ms: 3000,
            }),
        ] {
            let cost = crate::pricing::price(
                &candidate.dag,
                &node_map(&candidate.dag),
                &kept_pane_chains(&candidate.dag),
                std::slice::from_ref(&recurrence),
                &[range],
                &data(),
                &settings,
            )
            .unwrap();
            let CostModel::UsageOverTime(prices) = &settings.cost_model else {
                unreachable!()
            };
            assert!(close(
                cost.usage.cpu_ops_per_second,
                2.0 / settings.resource_speed.ms_per_cpu_op * 2000.0
            ));
            assert!(close(
                cost.cost_per_second.unwrap(),
                prices.cost_per_cpu_op * cost.usage.cpu_ops_per_second
            ));
            assert!(cost.cost_per_second.unwrap() > 0.0);
            assert_eq!(cost.usage.latency_per_query[0].query_ms, 2.0);
            assert_eq!(cost.usage.peak_cpu_cores, 4);
            assert!(cost.per_node.is_empty());
            assert_eq!(cost.source, MEASURED_COST_SOURCE);
        }
        // Cluster capacity must enforce overhead's throughput requirement too.
        settings.cost_model = CostModel::ClusterCapacity(ClusterLimits {
            memory_bytes: u64::MAX,
            disk_bytes: u64::MAX,
            cpu_cores: Some(3),
            objective: ClusterObjective::LowestLatency,
        });
        let facts = data();
        let models = PlanningModels {
            plan_selection: &settings,
            ..PlanningModels::builtin()
        };
        let mut selector = PlanSelector::new(&entries, &facts, models, false).unwrap();
        selector.set_calibration(CalibrationFit::default());
        selector.offer(&candidate).unwrap();
        assert!(matches!(
            selector.finish(),
            Err(SelectionError::NoValidCandidate(_))
        ));
        // Finite input coefficients can still overflow when multiplied by demand.
        settings.measured_operation_costs.ns_per_evaluation = f64::MAX;
        assert!(matches!(
            price(&candidate.dag, &[recurrence], &facts, &settings),
            Err((_, AnalyticalCostError::Overflow))
        ));
    }

    /// A measured `memory_bytes_per_state` sizes the state a build keeps,
    /// in place of the bytes the formulas derive from its parameters.
    #[test]
    fn measured_state_bytes_size_the_kept_state() {
        let candidates = pane_candidates(10_000.0);
        let maintained = &candidates[1];
        let kll = maintained
            .dag
            .nodes
            .iter()
            .find_map(|n| match &n.payload {
                Payload::ASAP(ASAPOp::PaneChain {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) => Some(kind.params().clone()),
                _ => None,
            })
            .unwrap();
        let memory = |memory_bytes_per_state| {
            let settings = measured(vec![OperatorCost {
                memory_bytes_per_state,
                ..sketch_entry(OperatorKind::PaneChain, kll.clone(), 1.0)
            }]);
            price(
                &maintained.dag,
                &[repeating(MINUTE)],
                &arriving(10_000.0),
                &settings,
            )
            .unwrap()
            .usage
            .average_memory_bytes
        };
        let formula = memory(None);
        // The formula's state: each pane sees its series' 60 samples (one
        // minute of 10 000 rows per second over 10 000 series).
        let samples = StateInputRows {
            rows_per_state: Some(60),
            non_negative: false,
        };
        let half = memory(Some(sketch_shape(&kll, samples).1 as f64 / 2.0));
        assert!(half < formula && half > 0.0, "{half} vs {formula}");
    }

    // Oversized state configurations must be rejected as unrepresentable,
    // never wrap to a cheap candidate or panic while pricing pane choices.
    #[test]
    fn pane_state_byte_overflow_returns_a_cost_error() {
        for candidate in pane_candidates(10_000.0) {
            let kll = candidate
                .dag
                .nodes
                .iter()
                .find_map(|node| match &node.payload {
                    Payload::ASAP(ASAPOp::PaneChain {
                        family: FieldDataType::Sketch(kind, _),
                        ..
                    }) => Some(kind.params().clone()),
                    _ => None,
                })
                .unwrap();
            let settings = measured(vec![OperatorCost {
                memory_bytes_per_state: Some(u64::MAX as f64),
                ..sketch_entry(OperatorKind::PaneChain, kll, 1.0)
            }]);
            assert!(matches!(
                price(
                    &candidate.dag,
                    &[repeating(MINUTE)],
                    &arriving(10_000.0),
                    &settings
                ),
                Err((_, AnalyticalCostError::Overflow))
            ));
        }
    }

    /// The table of earlier versions, a JSON array of sketch operations,
    /// still loads: each entry becomes its operator's entry at both phases,
    /// an update both a `SummaryAgg` and a `PaneChain` entry.
    #[test]
    fn the_earlier_table_format_still_loads() {
        let settings: CostInputs = serde_json::from_str(
            r#"{"measured_operation_costs": [
                 {"sketch": {"Kll": {"k": 200}}, "operation": "update", "cpu_ns_per_operation": 120.0},
                 {"sketch": {"Kll": {"k": 200}}, "operation": "merge", "cpu_ns_per_operation": 900.0}]}"#,
        )
        .unwrap();
        let kll = SketchParams::Kll {
            k: 200,
            failure_probability: None,
        };
        assert_eq!(
            settings.measured_operation_costs.operators,
            vec![
                sketch_entry(OperatorKind::SummaryAgg, kll.clone(), 120.0),
                sketch_entry(OperatorKind::PaneChain, kll.clone(), 120.0),
                sketch_entry(OperatorKind::SummaryMerge, kll, 900.0),
            ]
        );
        let table: MeasuredOperationCosts = serde_json::from_str(
            r#"{"operators": [{"operator": "Scan", "phase": "query_time", "ns_per_unit": 150.0}],
                "ns_per_evaluation": 20000.0,
                "provenance": {"executor_version": "0.1.0", "machine": "x86_64, 16 cores"}}"#,
        )
        .unwrap();
        assert_eq!(
            table.operators,
            [plain_entry(
                OperatorKind::Scan,
                Some(ExecutionTiming::QueryTime),
                150.0
            )]
        );
        let round_trip: MeasuredOperationCosts =
            serde_json::from_str(&serde_json::to_string(&table).unwrap()).unwrap();
        assert_eq!(round_trip, table);
        assert!(serde_json::from_str::<MeasuredOperationCosts>(
            r#"{"operators": [{"operator": "Scan", "ns": 1.0}]}"#
        )
        .is_err());
    }

    /// With no entry for any node's operator and family (a CMS that no
    /// node builds, a KLL merge in a plan without merges), every node is
    /// priced from the formulas, as without a table.
    #[test]
    fn entries_for_other_operators_or_families_change_nothing() {
        let (candidate, _, sketch) = kll_build();
        let before = price(
            &candidate.dag,
            &recurrences(1),
            &data(),
            &CostInputs::default(),
        )
        .unwrap();
        let other = measured(vec![
            sketch_entry(
                OperatorKind::SummaryAgg,
                SketchParams::Cms {
                    width: 1024,
                    depth: 3,
                },
                50.0,
            ),
            sketch_entry(OperatorKind::SummaryMerge, sketch, 50.0),
        ]);
        let after = price(&candidate.dag, &recurrences(1), &data(), &other).unwrap();
        assert_eq!(after, before);
    }

    /// Bad input: a negative or non-finite time, an entry naming two
    /// families, two entries for one operator, phase and family, or a
    /// table with `ms_per_cpu_op` 0.
    #[test]
    fn invalid_measured_tables_are_rejected() {
        let cms = SketchParams::Cms {
            width: 1024,
            depth: 3,
        };
        let entry = |ns| sketch_entry(OperatorKind::SummaryAgg, cms.clone(), ns);
        let validate = |operators, ms_per_cpu_op| {
            let mut settings = measured(operators);
            settings.resource_speed.ms_per_cpu_op = ms_per_cpu_op;
            settings.validate()
        };
        for ns in [-1.0, f64::NAN] {
            assert!(matches!(
                validate(vec![entry(ns)], 1e-6),
                Err(AnalyticalCostError::InvalidCalibration(..))
            ));
        }
        let duplicate = validate(vec![entry(1.0), entry(2.0)], 1e-6).unwrap_err();
        assert!(
            matches!(duplicate, AnalyticalCostError::DuplicateOperatorCost(_)),
            "{duplicate}"
        );
        assert!(
            duplicate
                .to_string()
                .contains("SummaryAgg at both phases on Cms"),
            "{duplicate}"
        );
        let two_families = OperatorCost {
            exact: Some(asap_types::ir::schema::ExactKind::Sum),
            ..entry(1.0)
        };
        assert!(matches!(
            validate(vec![two_families], 1e-6),
            Err(AnalyticalCostError::AmbiguousOperatorCost(_))
        ));
        assert_eq!(
            validate(vec![entry(1.0)], 0.0),
            Err(AnalyticalCostError::MeasuredCostsWithoutCpuSpeed(0.0))
        );
        let mut empty = CostInputs::default();
        empty.resource_speed.ms_per_cpu_op = 0.0;
        assert!(
            empty.validate().is_ok(),
            "no table, no positive speed needed"
        );
    }
}
