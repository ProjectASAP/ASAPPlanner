//! #509 Stage 2, physical ASAP-aware optimization: turns one
//! CandidateLogicalASAPDAG into CandidatePhysicalASAPDAGs by materialization
//! and physical operator implementation.
//!
//! ## Materialization
//!
//! Each summary build (a `SummaryAgg` or a `PaneChain`) is one
//! materialization candidate ([`MaterializationCandidate`]) whose
//! output materialization may store (#509 §Materialization, Algorithm 3).
//! Each has up to four options:
//!
//! * `NotMaterialized`: rebuilt at query time by every query that reads it;
//! * `MatAtIngestion(Memory)`: maintained as data arrives and kept for the
//!   queries, when the build is eligible ([`IngestionPruneReason`]);
//! * `MatAtQuery(Memory)`: built once at query time for every query of one
//!   evaluation that reads it and reused only within that evaluation,
//!   offered for a build two or more queries read;
//! * `MatAtQueryKeptAcrossEvaluations(Memory)`: built at query time and
//!   kept across evaluations (#509 Example 4's B3), offered when the
//!   deployment keeps query-time results
//!   ([`DeploymentCapabilities::query_time_results_retained`]) for a
//!   tumbling-pane chain that builds one new pane per evaluation
//!   ([`builds_one_pane_per_evaluation`]): each evaluation builds the
//!   newest pane and reuses the others. Both query-time options are offered
//!   when both apply, and Stage 3 prices each.
//!
//! A build at ingestion time needs every build below it at ingestion time too
//! (#509 Algorithm 3, line 13); such assignments are not generated. Every
//! other combination of options is one physical candidate, the
//! all-`NotMaterialized` one first: nothing is capped. A node that one
//! query reads at ingestion time and another at query time is copied, one
//! copy per timing ([`split_shared_by_timing`]).
//!
//! A build may run at ingestion time (S4) only when:
//!
//! * the data workload is continuously ingesting or mixed (unknown arrival
//!   counts as not ingesting);
//! * every query reading it repeats and is predictable;
//! * it has no filter: an ingestion-time build reads raw samples, which
//!   carry none of the columns a filter reads;
//! * if it is a counter state (`Rate`, `Increase`, `IRate`), whose update
//!   needs each series' samples in time order, the data is continuously
//!   ingesting and every source below it is a time series. Assumption:
//!   continuously ingesting PromQL data arrives in time order per series,
//!   as Prometheus assumes (it rejects out-of-order samples). Table (SQL)
//!   sources and mixed data, whose at-rest part may load in any order, are
//!   not taken to be ordered;
//! * it is a tumbling-pane chain, or a whole-window build whose readers
//!   all run at one fixed interval with a known phase, over a window no
//!   longer than that interval and not shifted;
//! * everything below it can run at ingestion time.
//!
//! ## Physical operator implementation
//!
//! The runtime has one implementation per logical operator except exact
//! top-k, which it cannot run as `Aggregate{[TopK]}`. Stage 2 records that
//! choice in the DAG by rewriting it to a per-group sort followed by a
//! per-group limit, the shape the PromQL frontend uses for generic `topk`.
//! A `Project` over the limited rows restores the logical `[keys.., topk_k]`
//! columns, so an exact root has the same column names as a summary
//! candidate's and as the frontend declares, and no input column leaks.
//! `topk_k` carries the ranked value's type and nullability (the frontend
//! declares a non-null `Utf8` placeholder); casting the value to `Utf8` would
//! not make it the same answer.
//! An `Aggregate{[TopK]}` over a per-entity reduction, or with other
//! measures, a filter or `HAVING`, has no such rewrite, and its candidate
//! fails with a [`PhysicalASAPOptimizationError`]; the caller prunes that
//! candidate alone, with the error as its reason.
//! A summary needs no rewrite: `SummaryAgg` → `SummaryEstimate` already is
//! build → estimate. Every node keeps the guarantee Stage 1 composed for it.
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use asap_types::ir::operator::operator_properties::Source;
use asap_types::ir::operator::{AggIntent, Reduction};
use asap_types::ir::physical_export::{
    compile_physical_asap_workload_with_node_ids, PhysicalASAPDAG, PhysicalASAPNodeId,
};
use asap_types::ir::properties::{ExecutionDataStateError, ExecutionTiming};
use asap_types::ir::scalar::column_resolution::resolve_column_ref;
use asap_types::ir::scalar::expr_ir::ColumnRef;
use asap_types::ir::schema::{ExactKind, FieldDataType};
use asap_types::ir::{
    apply_materialization_timings, is_filtered_build, split_shared_by_timing, ASAPOp,
    MaterializationAssignment, NonASAPOp, Operator, OperatorNode, ProjectItem, QueryRoot,
    ScalarExpr, SchemaDerivationError, SortKey, TimingMemo,
};
use asap_types::physical::Materialization;
use asap_types::physical::Medium;
use asap_types::workload::{
    DataArrival, DataWorkload, Predictability, QueryRecurrence, QueryWorkloadEntry, RepeatedDemand,
};
use thiserror::Error;

use asap_logical_asap_optimization::deployment_capabilities::DeploymentCapabilities;
use asap_logical_asap_optimization::workload_candidates::fixed_interval_ms;

/// One CandidatePhysicalASAPDAG (Stage 2 output), derived from exactly one
/// CandidateLogicalASAPDAG (Stage 1 output). It holds planning data only;
/// display labels are derived from it by the devtools.
#[derive(Debug, Clone)]
pub struct PhysicalCandidate {
    /// `P<n>` (nothing materialized) or `P<n>-m<k>`, set by the caller.
    pub id: String,
    /// The id of the logical candidate it implements.
    pub from_logical: String,
    /// The timed query roots, one per query in workload order, that `dag`
    /// exports.
    pub roots: Vec<QueryRoot>,
    pub dag: PhysicalASAPDAG,
    /// Its materialization assignment: the option chosen for each summary
    /// build (`SummaryAgg` or `PaneChain`) of `dag`, by node id, including
    /// the builds left `NotMaterialized`. A build one query reads at
    /// ingestion time and another at query time appears once per copy.
    pub materialization: BTreeMap<PhysicalASAPNodeId, Materialization>,
}

impl PhysicalCandidate {
    /// Whether any summary build is materialized.
    pub fn materializes_any(&self) -> bool {
        self.materialization
            .values()
            .any(|m| *m != Materialization::NotMaterialized)
    }
}

#[derive(Debug, Error)]
pub enum PhysicalASAPOptimizationError {
    #[error(transparent)]
    Structure(#[from] SchemaDerivationError),
    #[error(transparent)]
    Timing(#[from] ExecutionDataStateError),
    #[error("exact top-k has no Sort + Limit implementation: {0}")]
    ExactTopK(String),
}

/// Why a summary build cannot be materialized at ingestion time (S4).
/// Stage 2 bookkeeping, like [`MaterializationCandidate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IngestionPruneReason {
    /// The data workload is at rest, or its arrival is unknown.
    NotIngesting,
    /// A query reading it is not repeated.
    NotRepeated,
    /// A query reading it is not predictable.
    NotPredictable,
    /// It has a filter, which only a query-time build can apply.
    Filtered,
    /// It is a counter state, whose update needs each series' samples in
    /// time order, over data not taken to arrive in that order: only
    /// continuously ingesting time-series (PromQL) sources are.
    CounterNeedsTimeOrder,
    /// A whole-window build whose readers do not all run at one fixed
    /// interval with a known phase, or whose window is longer than that
    /// interval, shifted, or not a range.
    WindowNotFixed,
    /// Something below it can only run at query time.
    UpstreamAtQueryTime(String),
}

/// One summary build Stage 2 may store, with its materialization options.
/// It is not a whole-workload plan like [`PhysicalCandidate`]: it is Stage
/// 2's enumeration bookkeeping, and the physical candidates are the
/// combinations of the materialization candidates' options. For example,
/// over one metric:
///
/// ```text
/// q1: quantile_over_time(0.99, latency[5m])   every 1m
/// q2: max_over_time(latency[5m])              every 1m
/// ```
///
/// If Stage 1 chose KLL panes for q1 and exact max panes for q2, Stage 2
/// finds two materialization candidates:
///
/// | materialization candidate | readers | options                         |
/// |---------------------------|---------|---------------------------------|
/// | Kll ×5 panes (q1)         | q1      | NotMaterialized, MatAtIngestion |
/// | Max acc ×5 panes (q2)     | q2      | NotMaterialized, MatAtIngestion |
///
/// One read by two or more queries also gets `MatAtQuery` (built once per
/// evaluation and reused). The physical candidates are the product of the
/// options, here 2 × 2 = 4, less the assignments that put a build at
/// ingestion time over one that is not; Stage 3 prices each.
/// `at_ingestion` records why ingestion is ruled out when it is (e.g.
/// `Filtered`, `NotRepeated`). Each physical candidate carries the
/// resulting assignment ([`PhysicalCandidate::materialization`]).
#[derive(Debug, Clone)]
pub(crate) struct MaterializationCandidate {
    /// The build in the implemented DAG.
    pub(crate) node: Rc<OperatorNode>,
    /// The queries (indices into the roots) that read it.
    pub(crate) readers: Vec<usize>,
    /// `Ok` when `MatAtIngestion` is an option.
    pub(crate) at_ingestion: Result<(), IngestionPruneReason>,
    /// Whether `MatAtQueryKeptAcrossEvaluations` is an option: the
    /// deployment keeps query-time results and it is a tumbling-pane chain
    /// that builds one new pane per evaluation.
    pub(crate) may_keep_across_evaluations: bool,
}

impl MaterializationCandidate {
    /// Its options, `NotMaterialized` first: `MatAtIngestion` when eligible,
    /// `MatAtQuery` when two or more queries read it, and
    /// `MatAtQueryKeptAcrossEvaluations` when it may be kept across
    /// evaluations.
    pub(crate) fn options(&self) -> Vec<Materialization> {
        let mut options = vec![Materialization::NotMaterialized];
        if self.at_ingestion.is_ok() {
            options.push(Materialization::MatAtIngestion(Medium::Memory));
        }
        if self.readers.len() > 1 {
            options.push(Materialization::MatAtQuery(Medium::Memory));
        }
        if self.may_keep_across_evaluations {
            options.push(Materialization::MatAtQueryKeptAcrossEvaluations(
                Medium::Memory,
            ));
        }
        options
    }
}

/// Whether a tumbling-pane chain of `pane_width_ms`-wide panes read by
/// queries repeating as `readers` builds exactly one new pane per
/// evaluation: there is a reader, every reader repeats at a fixed interval
/// equal to the pane width, and the readers whose evaluation phase is known
/// share one phase (modulo the interval). Its other panes are the previous
/// evaluations' newest, so a deployment that keeps query-time results
/// reuses them (#509 Example 4's B3).
///
/// Co-scheduling assumption: readers with the same interval and no known
/// phase run at the same instants as each other and as the readers with a
/// known phase, as plan selection (`asap-plan-selection`) assumes when it counts
/// one evaluation per interval. Readers at different known phases would
/// each need their own newest pane, so they do not share one kept chain.
pub(crate) fn builds_one_pane_per_evaluation<'a>(
    pane_width_ms: u64,
    readers: impl IntoIterator<Item = &'a QueryRecurrence>,
) -> bool {
    let mut phase_ms = None;
    let mut any_reader = false;
    for recurrence in readers {
        any_reader = true;
        if fixed_interval_ms(recurrence) != Some(pane_width_ms) {
            return false;
        }
        if let QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
            evaluation_phase, ..
        }) = recurrence
        {
            let phase = evaluation_phase.0 % pane_width_ms;
            if *phase_ms.get_or_insert(phase) != phase {
                return false;
            }
        }
    }
    any_reader
}

/// Stage 2 for one CandidateLogicalASAPDAG: implement every operator
/// (physical operator implementation), then one physical candidate per
/// materialization assignment of its mat_candidates, the all-`NotMaterialized` one
/// first. `entries[i]` is the workload entry of query `i` (`roots[i]`):
/// its recurrence and predictability decide whether state may be kept from
/// ingestion time; a missing entry is unknown. `id` is left empty for the
/// caller to name. Sharing between `roots` is preserved: one memo serves the
/// whole workload. `deployment` says whether a query-time result may be
/// kept across evaluations.
pub fn optimize_physical_asap(
    from_logical: &str,
    roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    deployment: &DeploymentCapabilities,
) -> Result<Vec<PhysicalCandidate>, PhysicalASAPOptimizationError> {
    let mut memo = HashMap::new();
    let implemented = map_roots(roots, &mut |node| implement(node, &mut memo))?;
    let mat_candidates = materialization_candidates(&implemented, entries, data, deployment);
    let mut candidates = Vec::new();
    for assignment in materialization_assignments(&mat_candidates) {
        let materialized: Vec<_> = mat_candidates
            .iter()
            .zip(&assignment)
            .filter(|(_, m)| **m != Materialization::NotMaterialized)
            .map(|(mat_candidate, m)| (mat_candidate, *m))
            .collect();
        let mut options = MaterializationAssignment::with_default(Materialization::NotMaterialized);
        for (mat_candidate, materialization) in &materialized {
            options.set(&mat_candidate.node, *materialization);
        }
        let operators: Vec<Rc<OperatorNode>> = implemented
            .iter()
            .flat_map(|root| root_operators(root).into_iter().cloned())
            .collect();
        let (split, options) = split_shared_by_timing(&operators, &options);
        let replaced: HashMap<_, _> = operators
            .iter()
            .map(Rc::as_ptr)
            .zip(split.iter().cloned())
            .collect();
        let split_roots = map_roots(&implemented, &mut |node| {
            Ok(replaced[&Rc::as_ptr(node)].clone())
        })?;
        let mut timing = TimingMemo::new();
        let timed = map_roots(&split_roots, &mut |node| {
            Ok(apply_materialization_timings(node, &options, &mut timing)?)
        })?;
        let dag = compile_physical_asap_workload_with_node_ids(&timed)?.dag;
        let materialization = dag
            .nodes
            .iter()
            .filter(|n| {
                matches!(
                    n.payload,
                    Operator::ASAP(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
                )
            })
            .map(|n| (n.id, n.materialization))
            .collect();
        candidates.push(PhysicalCandidate {
            id: String::new(),
            from_logical: from_logical.to_string(),
            roots: timed,
            dag,
            materialization,
        });
    }
    Ok(candidates)
}

/// The number of physical candidates [`optimize_physical_asap`] returns for
/// `roots`, without building them; `Err` when a candidate cannot be
/// implemented.
pub fn physical_candidate_count(
    roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    deployment: &DeploymentCapabilities,
) -> Result<usize, PhysicalASAPOptimizationError> {
    let mut memo = HashMap::new();
    let implemented = map_roots(roots, &mut |node| implement(node, &mut memo))?;
    let mat_candidates = materialization_candidates(&implemented, entries, data, deployment);
    Ok(materialization_assignments(&mat_candidates).count())
}

/// The operator nodes `root` reads.
fn root_operators(root: &QueryRoot) -> Vec<&Rc<OperatorNode>> {
    match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    }
}

/// Every summary build of `roots` (implemented), in discovery order, with
/// its readers, whether it may run at ingestion time and whether it may be
/// kept from query time across evaluations.
pub(crate) fn materialization_candidates(
    roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    deployment: &DeploymentCapabilities,
) -> Vec<MaterializationCandidate> {
    let mut nodes: Vec<Rc<OperatorNode>> = Vec::new();
    let mut readers: HashMap<*const OperatorNode, Vec<usize>> = HashMap::new();
    for (query, root) in roots.iter().enumerate() {
        for operator in root_operators(root) {
            for node in OperatorNode::reachable(operator) {
                if !matches!(
                    node.asap(),
                    Some(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
                ) {
                    continue;
                }
                let entry = readers.entry(Rc::as_ptr(&node)).or_default();
                if entry.is_empty() {
                    nodes.push(Rc::clone(&node));
                }
                if !entry.contains(&query) {
                    entry.push(query);
                }
            }
        }
    }
    let below = candidates_below(&nodes);
    nodes
        .iter()
        .enumerate()
        .map(|(c, node)| {
            let readers = readers[&Rc::as_ptr(node)].clone();
            let at_ingestion =
                ingestion_eligibility(node, &readers, entries, data).and_then(|()| {
                    // Only this build and the builds below it at ingestion time.
                    let mut trial =
                        MaterializationAssignment::with_default(Materialization::NotMaterialized);
                    for &v in below[c].iter().chain([&c]) {
                        trial.set(&nodes[v], Materialization::MatAtIngestion(Medium::Memory));
                    }
                    apply_materialization_timings(node, &trial, &mut TimingMemo::new())
                        .map(|_| ())
                        .map_err(|e| IngestionPruneReason::UpstreamAtQueryTime(e.to_string()))
                });
            // A reader without a workload entry has an unknown recurrence.
            let recurrences: Option<Vec<&QueryRecurrence>> = readers
                .iter()
                .map(|&q| entries.get(q).map(|e| &e.recurrence))
                .collect();
            let may_keep_across_evaluations = deployment.query_time_results_retained
                && match (node.asap(), recurrences) {
                    (Some(ASAPOp::PaneChain { pane_width_ms, .. }), Some(recurrences)) => {
                        builds_one_pane_per_evaluation(*pane_width_ms, recurrences)
                    }
                    _ => false,
                };
            MaterializationCandidate {
                node: Rc::clone(node),
                readers,
                at_ingestion,
                may_keep_across_evaluations,
            }
        })
        .collect()
}

/// Whether every scan below `node` reads a time series (a PromQL source).
fn reads_only_time_series(node: &OperatorNode) -> bool {
    match node.non_asap() {
        Some(NonASAPOp::Scan { source, .. }) => matches!(source, Source::TimeSeries { .. }),
        _ => node.children().iter().all(|c| reads_only_time_series(c)),
    }
}

/// The workload rules of S4 for `node` (module doc).
fn ingestion_eligibility(
    node: &OperatorNode,
    readers: &[usize],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
) -> Result<(), IngestionPruneReason> {
    use IngestionPruneReason as Why;
    if !matches!(
        data.arrival,
        DataArrival::Ingesting { .. } | DataArrival::Mixed { .. }
    ) {
        return Err(Why::NotIngesting);
    }
    let demands: Vec<Option<&QueryWorkloadEntry>> =
        readers.iter().map(|&q| entries.get(q)).collect();
    if demands
        .iter()
        .any(|d| !d.is_some_and(|d| matches!(d.recurrence, QueryRecurrence::Repeated(_))))
    {
        return Err(Why::NotRepeated);
    }
    if demands
        .iter()
        .any(|d| !d.is_some_and(|d| matches!(d.predictability, Predictability::Predictable { .. })))
    {
        return Err(Why::NotPredictable);
    }
    if is_filtered_build(node) {
        return Err(Why::Filtered);
    }
    let (family, chain) = match node.asap() {
        Some(ASAPOp::SummaryAgg { family, .. }) => (family, false),
        Some(ASAPOp::PaneChain { family, .. }) => (family, true),
        _ => unreachable!("a materialization candidate is a summary build"),
    };
    if matches!(
        family,
        FieldDataType::ExactAggregate(ExactKind::Rate | ExactKind::Increase | ExactKind::IRate, _)
    ) {
        // Assumption (module doc): continuously ingesting PromQL data
        // arrives in time order per series, as Prometheus assumes.
        let ordered =
            matches!(data.arrival, DataArrival::Ingesting { .. }) && reads_only_time_series(node);
        if !ordered {
            return Err(Why::CounterNeedsTimeOrder);
        }
    }
    if chain {
        return Ok(());
    }
    // A whole-window build: one window per evaluation, at one known phase,
    // that ends at the evaluation and fits in the interval.
    let mut cadence = None;
    for d in demands.into_iter().flatten() {
        let QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
            interval,
            evaluation_phase,
        }) = &d.recurrence
        else {
            return Err(Why::WindowNotFixed);
        };
        if cadence.is_some_and(|c| c != (interval.0, evaluation_phase.0)) {
            return Err(Why::WindowNotFixed);
        }
        cadence = Some((interval.0, evaluation_phase.0));
    }
    let Some((interval_ms, _)) = cadence else {
        return Err(Why::WindowNotFixed);
    };
    match node.children()[0].non_asap() {
        Some(NonASAPOp::TimeRange {
            range,
            kind: asap_types::ir::TimeRangeKind::Range,
            child,
        }) if range.as_millis() <= u128::from(interval_ms)
            && !matches!(child.non_asap(), Some(NonASAPOp::TimeShift { .. })) =>
        {
            Ok(())
        }
        _ => Err(Why::WindowNotFixed),
    }
}

/// For each of `nodes`, the indices of the others below it.
fn candidates_below(nodes: &[Rc<OperatorNode>]) -> Vec<Vec<usize>> {
    nodes
        .iter()
        .map(|node| {
            let reachable = OperatorNode::reachable(node);
            (0..nodes.len())
                .filter(|&v| {
                    !Rc::ptr_eq(&nodes[v], node)
                        && reachable.iter().any(|n| Rc::ptr_eq(n, &nodes[v]))
                })
                .collect()
        })
        .collect()
}

/// Every assignment of an option to each materialization candidate (mixed
/// radix, the last varying fastest, all `NotMaterialized` first) in which
/// every build below an ingestion-time build is at ingestion time too.
fn materialization_assignments(
    mat_candidates: &[MaterializationCandidate],
) -> impl Iterator<Item = Vec<Materialization>> + '_ {
    let options: Vec<Vec<Materialization>> = mat_candidates.iter().map(|c| c.options()).collect();
    let nodes: Vec<_> = mat_candidates.iter().map(|c| Rc::clone(&c.node)).collect();
    let below = candidates_below(&nodes);
    let mut digits = Some(vec![0usize; mat_candidates.len()]);
    std::iter::from_fn(move || loop {
        let current = digits.as_mut()?;
        let assignment: Vec<Materialization> = current
            .iter()
            .zip(&options)
            .map(|(&d, options)| options[d])
            .collect();
        // Advance (mixed radix); `None` after the last assignment.
        let mut carried = true;
        for (digit, options) in current.iter_mut().zip(&options).rev() {
            *digit += 1;
            if *digit < options.len() {
                carried = false;
                break;
            }
            *digit = 0;
        }
        if carried {
            digits = None;
        }
        let at_ingestion = |m: &Materialization| m.timing() == ExecutionTiming::IngestionTime;
        let down_closed = assignment.iter().enumerate().all(|(c, m)| {
            !at_ingestion(m) || below[c].iter().all(|&v| at_ingestion(&assignment[v]))
        });
        if down_closed {
            return Some(assignment);
        }
        digits.as_ref()?;
    })
}

/// `roots` with every operator node they read replaced by `f`'s result.
fn map_roots(
    roots: &[QueryRoot],
    f: &mut impl FnMut(&Rc<OperatorNode>) -> Result<Rc<OperatorNode>, PhysicalASAPOptimizationError>,
) -> Result<Vec<QueryRoot>, PhysicalASAPOptimizationError> {
    roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => Ok(QueryRoot::Operator(f(node)?)),
            QueryRoot::Scalar(expr) => {
                let mut mapped = HashMap::new();
                for node in expr.operator_refs() {
                    mapped.insert(Rc::as_ptr(node), f(node)?);
                }
                Ok(QueryRoot::Scalar(expr.map_operator_refs(&mut |node| {
                    mapped[&Rc::as_ptr(node)].clone()
                })))
            }
        })
        .collect()
}

/// Each node's implementation.
type Memo = HashMap<*const OperatorNode, Rc<OperatorNode>>;

fn implement(
    node: &Rc<OperatorNode>,
    memo: &mut Memo,
) -> Result<Rc<OperatorNode>, PhysicalASAPOptimizationError> {
    if let Some(done) = memo.get(&Rc::as_ptr(node)) {
        return Ok(done.clone());
    }
    for child in node.children() {
        implement(child, memo)?;
    }
    let implemented = match node.non_asap() {
        Some(NonASAPOp::Aggregate { measures, .. })
            if measures.iter().any(|m| matches!(m, AggIntent::TopK { .. })) =>
        {
            implement_topk(node, memo)?
        }
        _ => rebuilt(node, memo)?,
    };
    memo.insert(Rc::as_ptr(node), implemented.clone());
    Ok(implemented)
}

/// Exact top-k as `Limit(Sort)` per group, which outputs the ranked rows of
/// the child, under a `Project` restoring the logical `[keys.., topk_k]`
/// output (the ranked value under `topk_k`): a root returns those columns,
/// and a node reading the top-k resolved its columns against them.
fn implement_topk(
    node: &Rc<OperatorNode>,
    memo: &Memo,
) -> Result<Rc<OperatorNode>, PhysicalASAPOptimizationError> {
    let unsupported = |why: &str| PhysicalASAPOptimizationError::ExactTopK(why.to_string());
    let Some(NonASAPOp::Aggregate {
        reduction,
        measures,
        filters,
        having,
        child,
        ..
    }) = node.non_asap()
    else {
        unreachable!("implement_topk is called on an Aggregate")
    };
    let Reduction::Reduce(keys) = reduction else {
        return Err(unsupported("per-entity reduction"));
    };
    let [AggIntent::TopK { k, .. }] = measures.as_slice() else {
        return Err(unsupported("measures besides the top-k"));
    };
    if filters.iter().any(Option::is_some) {
        return Err(unsupported("filtered measure"));
    }
    if having.is_some() {
        return Err(unsupported("HAVING"));
    }
    let child = memo[&Rc::as_ptr(child)].clone();
    let value = asap_logical_asap_optimization::realization::ranked_column(&child)
        .map_err(|e| unsupported(&format!("no ranked column: {e}")))?;
    let sorted = Rc::new(
        OperatorNode::new(Operator::NonASAP(NonASAPOp::Sort {
            keys: vec![SortKey {
                expr: ScalarExpr::Column(value),
                ascending: false,
                nulls_first: false,
            }],
            partition_by: keys.clone(),
            child: child.clone(),
        }))?
        .with_guarantee(child.guarantee.clone()),
    );
    let limited = Rc::new(
        OperatorNode::new(Operator::NonASAP(NonASAPOp::Limit {
            n: Some(*k),
            offset: 0,
            partition_by: keys.clone(),
            child: sorted,
        }))?
        .with_guarantee(node.guarantee.clone()),
    );
    let (ranked, key_fields) = node
        .schema
        .fields
        .split_last()
        .expect("an aggregate outputs its measure");
    let mut cols = key_fields
        .iter()
        .map(|field| {
            let column = resolve_column_ref(&ColumnRef::Named(field.name.clone()), &child.schema)
                .map_err(|e| unsupported(&format!("group key {}: {e}", field.name)))?;
            Ok(ProjectItem {
                alias: Some(field.name.clone()),
                expr: ScalarExpr::Column(column),
            })
        })
        .collect::<Result<Vec<_>, PhysicalASAPOptimizationError>>()?;
    cols.push(ProjectItem {
        alias: Some(ranked.name.clone()),
        expr: ScalarExpr::Column(value),
    });
    Ok(Rc::new(
        OperatorNode::new(Operator::NonASAP(NonASAPOp::Project {
            cols,
            qualifier: None,
            child: limited,
        }))?
        .with_guarantee(node.guarantee.clone()),
    ))
}

/// `node` over its implemented children; the same `Rc` when none changed.
fn rebuilt(
    node: &Rc<OperatorNode>,
    memo: &Memo,
) -> Result<Rc<OperatorNode>, PhysicalASAPOptimizationError> {
    let input = |child: &Rc<OperatorNode>| memo[&Rc::as_ptr(child)].clone();
    let changed = node
        .children()
        .iter()
        .any(|child| !Rc::ptr_eq(child, &input(child)));
    Ok(if changed {
        Rc::new(
            node.with_new_children(|child| input(child))?
                .with_guarantee(node.guarantee.clone()),
        )
    } else {
        node.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_types::ir::physical_export::PhysicalASAPNodeId;
    use asap_types::ir::physical_export::PhysicalASAPOperatorPayload as Payload;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::properties::ExecutionTiming;
    use asap_types::ir::ASAPOp;

    /// The all-`NotMaterialized` candidate, with no workload declared.
    fn first_candidate(roots: &[QueryRoot]) -> PhysicalCandidate {
        optimize_physical_asap(
            "L1",
            roots,
            &[],
            &DataWorkload::default(),
            &DeploymentCapabilities::default(),
        )
        .unwrap()
        .remove(0)
    }

    fn operator_root(candidate: &PhysicalCandidate, i: usize) -> Rc<OperatorNode> {
        match &candidate.roots[i] {
            QueryRoot::Operator(node) => node.clone(),
            QueryRoot::Scalar(_) => panic!("operator root"),
        }
    }

    fn names(node: &OperatorNode) -> Vec<String> {
        node.schema.fields.iter().map(|f| f.name.clone()).collect()
    }

    /// The `Limit` under an exact top-k's restoring `Project`.
    fn topk_limit(project: &OperatorNode) -> Rc<OperatorNode> {
        let NonASAPOp::Project { child, .. } = project.expect_non_asap() else {
            panic!("project, found {}", project.operator.kind_name())
        };
        child.clone()
    }

    /// The single sort key of the `Sort` under the `Limit` at `limit`.
    fn topk_sort_key(limit: &OperatorNode) -> (ScalarExpr, bool) {
        let NonASAPOp::Limit { child: sort, .. } = limit.expect_non_asap() else {
            panic!("limit, found {}", limit.operator.kind_name())
        };
        let NonASAPOp::Sort { keys, .. } = sort.expect_non_asap() else {
            panic!("sort below limit")
        };
        let [key] = keys.as_slice() else {
            panic!("one sort key")
        };
        (key.expr.clone(), key.ascending)
    }

    /// Example 1's shape, `topk by (job) (k, per-series rows)`, sorts each
    /// group by the sample value, descending.
    #[test]
    fn promql_topk_over_series_sorts_by_value_descending() {
        let root = lower_promql(
            "topk by (job) (10, sum_over_time(m[1m]))",
            AccuracyTarget::exact(),
        );
        let candidate = first_candidate(&[root.clone().into()]);
        let physical = operator_root(&candidate, 0);
        assert_eq!(names(&physical), names(&root));
        let limit = topk_limit(&physical);
        assert_eq!(limit.schema.fields[1].name, "value");
        assert_eq!(topk_sort_key(&limit), (ScalarExpr::Column(1), false));
    }

    /// `topk(k, sum by (job) (..))` sorts by the summed measure.
    #[test]
    fn promql_topk_over_grouped_aggregate_sorts_by_its_measure() {
        let root = lower_promql("topk(5, sum by (job) (m))", AccuracyTarget::exact());
        let candidate = first_candidate(&[root.into()]);
        let physical = operator_root(&candidate, 0);
        assert_eq!(names(&physical), ["topk_5"]);
        let limit = topk_limit(&physical);
        assert_eq!(names(&limit), ["job", "sum"]);
        assert_eq!(topk_sort_key(&limit), (ScalarExpr::Column(1), false));
    }

    /// A node reading an exact top-k keeps resolving the columns it bound
    /// against the logical `[keys.., topk_k]` output: `sum by (job)` still
    /// groups by `job`, not by whatever column now sits at its index.
    #[test]
    fn consumer_of_exact_topk_reads_its_logical_columns() {
        let root = lower_promql(
            "sum by (job) (topk by (job) (10, sum_over_time(m[1m])))",
            AccuracyTarget::exact(),
        );
        let candidate = first_candidate(&[root.clone().into()]);
        let physical = operator_root(&candidate, 0);
        assert_eq!(names(&physical), names(&root));
        let NonASAPOp::Aggregate { child: input, .. } = physical.expect_non_asap() else {
            panic!("sum root")
        };
        let NonASAPOp::Project {
            cols, child: limit, ..
        } = input.expect_non_asap()
        else {
            panic!("project over the top-k rows")
        };
        let read: Vec<_> = cols
            .iter()
            .map(|item| {
                let ScalarExpr::Column(c) = item.expr else {
                    panic!("column")
                };
                (
                    item.alias.clone().unwrap(),
                    limit.schema.fields[c].name.clone(),
                )
            })
            .collect();
        assert_eq!(
            read,
            [
                ("job".to_string(), "job".to_string()),
                ("topk_10".to_string(), "value".to_string())
            ]
        );
        assert_eq!(topk_sort_key(limit), (ScalarExpr::Column(1), false));
    }

    /// An `Aggregate{[TopK]}` that Sort + Limit cannot express (another
    /// measure, `HAVING`) fails, so its candidate is pruned with the reason.
    #[test]
    fn topk_with_extra_measure_or_having_fails() {
        use asap_logical_asap_optimization::test_support::{aggregate, metric_scan};
        use asap_types::ir::scalar::expr_ir::ScalarValue;
        use asap_types::ir::Predicate;
        let topk = AggIntent::TopK {
            k: 3,
            accuracy: AccuracyTarget::exact(),
        };
        let count = AggIntent::Count {
            accuracy: AccuracyTarget::exact(),
        };
        let by_job = || Reduction::by(vec![2]);
        let cases = [
            aggregate(
                by_job(),
                vec![topk.clone(), count],
                vec![],
                None,
                metric_scan(&["job"]),
            ),
            aggregate(
                by_job(),
                vec![topk],
                vec![],
                Some(Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true)))),
                metric_scan(&["job"]),
            ),
        ];
        for root in cases {
            let error = optimize_physical_asap(
                "L1",
                &[root.into()],
                &[],
                &DataWorkload::default(),
                &DeploymentCapabilities::default(),
            )
            .unwrap_err();
            assert!(
                matches!(error, PhysicalASAPOptimizationError::ExactTopK(_)),
                "{error}"
            );
        }
    }

    /// Exact `topk by (job)` becomes Limit(Sort) partitioned by `job`, and a
    /// child shared by two roots stays one node (one `Rc`, one DAG node).
    #[test]
    fn exact_topk_becomes_sort_then_limit_and_keeps_shared_child() {
        let exact = AccuracyTarget::exact();
        let topk = lower_promql("topk by (job) (10, sum_over_time(m[1m]))", exact.clone());
        let sum = lower_promql("sum by (job) (sum_over_time(m[1m]))", exact);
        // Share Q2's per-series child with Q1, as Stage 1 sharing would.
        let NonASAPOp::Aggregate { child: shared, .. } = topk.expect_non_asap() else {
            panic!("topk aggregate")
        };
        let sum = Rc::new(sum.with_new_children(|_| shared.clone()).unwrap());
        let candidate = first_candidate(&[topk.into(), sum.into()]);
        let root = |i: usize| match &candidate.roots[i] {
            QueryRoot::Operator(node) => node.clone(),
            QueryRoot::Scalar(_) => panic!("operator root"),
        };

        let NonASAPOp::Limit {
            n: Some(10),
            partition_by,
            child: sort,
            ..
        } = topk_limit(&root(0)).expect_non_asap().clone()
        else {
            panic!("limit under the root")
        };
        assert!(!partition_by.keys().is_empty());
        let NonASAPOp::Sort {
            child: below_sort,
            partition_by: sort_partition,
            ..
        } = sort.expect_non_asap()
        else {
            panic!("sort below limit")
        };
        assert_eq!(*sort_partition, partition_by);
        let NonASAPOp::Aggregate {
            child: below_sum, ..
        } = root(1).expect_non_asap().clone()
        else {
            panic!("sum root")
        };
        assert!(Rc::ptr_eq(below_sort, &below_sum));

        let dag = &candidate.dag;
        let kind = |id: PhysicalASAPNodeId| match &dag
            .nodes
            .iter()
            .find(|n| n.id == id)
            .unwrap()
            .payload
        {
            Payload::NonASAP(NonASAPOp::Sort { .. }) => "sort",
            Payload::NonASAP(NonASAPOp::Limit { .. }) => "limit",
            _ => "other",
        };
        let producer_of = |consumer| {
            dag.edges
                .iter()
                .find(|e| e.consumer == consumer)
                .unwrap()
                .producer
        };
        let limit_id = producer_of(dag.roots[0]);
        assert_eq!(kind(limit_id), "limit");
        let sort_id = producer_of(limit_id);
        assert_eq!(kind(sort_id), "sort");
        assert_eq!(producer_of(sort_id), producer_of(dag.roots[1]));
    }

    /// With summaries chosen, every node, the summary build included, runs at
    /// query time.
    #[test]
    fn everything_runs_at_query_time() {
        let root = lower_promql(
            "topk by (job) (10, sum_over_time(m[1m]))",
            AccuracyTarget::max_error(0.01).failure_probability(0.001),
        );
        let inventory = asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        // A summary (the last candidate) for every target.
        let choice: Vec<_> = inventory
            .targets
            .iter()
            .map(|t| t.choice_count() - 1)
            .collect();
        let roots: Vec<_> =
            asap_logical_asap_optimization::per_computation_candidates::compose_logical_candidate(
                &inventory, &choice,
            )
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        let candidate = first_candidate(&roots);
        assert!(candidate
            .dag
            .nodes
            .iter()
            .any(|n| matches!(n.payload, Payload::ASAP(ASAPOp::SummaryAgg { .. }))));
        assert!(candidate
            .dag
            .nodes
            .iter()
            .all(|n| n.output_state.timing == ExecutionTiming::QueryTime));
    }

    // ── Materialization ──────────────────────────────────────────────────

    use asap_types::workload::{DataArrival, RepetitionInterval, TimestampMs};

    const MINUTE: u64 = 60_000;

    /// The composed logical candidate of `query` that builds its last
    /// summary option (DDSketch for a quantile) for every target.
    fn summary_root(query: &str) -> QueryRoot {
        let root = lower_promql(
            query,
            AccuracyTarget::max_error(0.01)
                .failure_probability(0.001)
                .accuracy_metric(asap_types::ir::properties::AccuracyMetric::Rank),
        );
        let inventory = asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        let choice: Vec<_> = inventory
            .targets
            .iter()
            .map(|t| t.choice_count() - 1)
            .collect();
        asap_logical_asap_optimization::per_computation_candidates::compose_logical_candidate(
            &inventory, &choice,
        )
        .unwrap()
        .remove(0)
        .1
    }

    /// `root` with its summary build in 1-min tumbling panes.
    fn in_panes(root: QueryRoot) -> QueryRoot {
        let roots = [root];
        let rule_candidates = asap_logical_asap_optimization::asap_aware_cse::pane_candidates(
            &roots,
            &[Some(MINUTE)],
        );
        assert_eq!(rule_candidates.len(), 1, "{rule_candidates:?}");
        let mut pass2_candidates =
            asap_logical_asap_optimization::asap_aware_cse::window_composition_pass2_candidates(
                &roots,
                &rule_candidates,
            )
            .unwrap();
        pass2_candidates.pop().unwrap().roots.unwrap().remove(0)
    }

    const AT_INGESTION: Materialization = Materialization::MatAtIngestion(Medium::Memory);

    /// The options `candidate` chose other than `NotMaterialized`, by node.
    fn materialized(candidate: &PhysicalCandidate) -> Vec<Materialization> {
        candidate
            .materialization
            .values()
            .copied()
            .filter(|m| *m != Materialization::NotMaterialized)
            .collect()
    }

    /// A predictable query evaluated every minute.
    fn every_minute() -> QueryWorkloadEntry {
        QueryWorkloadEntry {
            predictability: Predictability::Predictable { known_at: None },
            ..asap_logical_asap_optimization::test_support::entry(QueryRecurrence::Repeated(
                RepeatedDemand::FixedInterval(RepetitionInterval(MINUTE as u32)),
            ))
        }
    }

    fn arriving() -> DataWorkload {
        DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 1_000.0,
            },
            ..Default::default()
        }
    }

    /// The distinct nodes of `kind` the candidate's roots read.
    fn node_of(candidate: &PhysicalCandidate, kind: &str) -> Vec<Rc<OperatorNode>> {
        let mut nodes: Vec<Rc<OperatorNode>> = Vec::new();
        for root in candidate.roots.iter().flat_map(root_operators) {
            for node in OperatorNode::reachable(root) {
                if node.operator.kind_name() == kind && !nodes.iter().any(|n| Rc::ptr_eq(n, &node))
                {
                    nodes.push(node);
                }
            }
        }
        nodes
    }

    /// #509 Example 4, B1 and B2: a tumbling-pane chain read by a repeating,
    /// predictable query over arriving data is either rebuilt at query time
    /// or maintained at ingestion time. Maintained, the chain and the scan
    /// below it run at ingestion time, and the merge and estimate at query
    /// time.
    #[test]
    fn a_pane_chain_over_arriving_data_may_be_maintained_at_ingestion_time() {
        let root = in_panes(summary_root("quantile_over_time(0.99, m[5m])"));
        let candidates = optimize_physical_asap(
            "L1",
            &[root],
            &[every_minute()],
            &arriving(),
            &DeploymentCapabilities::default(),
        )
        .unwrap();
        assert_eq!(candidates.len(), 2);
        let [b2, b1] = &candidates[..] else {
            unreachable!()
        };
        assert!(!b2.materializes_any());
        assert!(b2
            .dag
            .nodes
            .iter()
            .all(|n| n.output_state.timing == ExecutionTiming::QueryTime));
        assert_eq!(materialized(b1), [AT_INGESTION]);
        let [chain] = &node_of(b1, "PaneChain")[..] else {
            panic!("one chain")
        };
        assert_eq!(
            chain.materialization,
            Materialization::MatAtIngestion(Medium::Memory)
        );
        for below in OperatorNode::reachable(chain) {
            assert_eq!(below.timing, Some(ExecutionTiming::IngestionTime));
        }
        for kind in ["SummaryMerge", "SummaryEstimate"] {
            for node in node_of(b1, kind) {
                assert_eq!(node.timing, Some(ExecutionTiming::QueryTime), "{kind}");
            }
        }
    }

    /// S4: maintaining state at ingestion time needs arriving data and
    /// queries that repeat and are predictable; otherwise the chain only
    /// runs at query time, and the materialization candidate says why.
    #[test]
    fn ingestion_time_needs_arriving_data_and_repeated_predictable_queries() {
        use IngestionPruneReason as Why;
        let root = in_panes(summary_root("quantile_over_time(0.99, m[5m])"));
        let at_rest = DataWorkload {
            arrival: DataArrival::AtRest,
            ..Default::default()
        };
        let unknown = QueryWorkloadEntry {
            predictability: Predictability::Unknown,
            ..every_minute()
        };
        let once = QueryWorkloadEntry {
            recurrence: QueryRecurrence::OneTime {
                invocations: 1,
                execute_at: None,
            },
            ..every_minute()
        };
        for (demand, data, why) in [
            (every_minute(), at_rest, Why::NotIngesting),
            (every_minute(), DataWorkload::default(), Why::NotIngesting),
            (unknown, arriving(), Why::NotPredictable),
            (once, arriving(), Why::NotRepeated),
        ] {
            let roots = [root.clone()];
            let candidates = optimize_physical_asap(
                "L1",
                &roots,
                std::slice::from_ref(&demand),
                &data,
                &DeploymentCapabilities::default(),
            )
            .unwrap();
            assert_eq!(candidates.len(), 1, "{why:?}");
            let mut memo = HashMap::new();
            let implemented = map_roots(&roots, &mut |n| implement(n, &mut memo)).unwrap();
            let mat_candidates = materialization_candidates(
                &implemented,
                &[demand],
                &data,
                &DeploymentCapabilities::default(),
            );
            assert_eq!(mat_candidates.len(), 1);
            assert_eq!(mat_candidates[0].at_ingestion, Err(why));
        }
    }

    /// A counter state (`rate`, `increase`) needs each series' samples in
    /// time order. Continuously ingesting time-series (PromQL) data is taken
    /// to arrive in that order, as Prometheus assumes, so its counter panes
    /// may be maintained at ingestion time.
    #[test]
    fn counter_panes_over_ingesting_promql_data_run_at_ingestion_time() {
        for query in ["rate(m[5m])", "increase(m[5m])"] {
            let roots = [in_panes(summary_root(query))];
            let mut memo = HashMap::new();
            let implemented = map_roots(&roots, &mut |n| implement(n, &mut memo)).unwrap();
            let mat_candidates = materialization_candidates(
                &implemented,
                &[every_minute()],
                &arriving(),
                &DeploymentCapabilities::default(),
            );
            assert_eq!(mat_candidates[0].at_ingestion, Ok(()), "{query}");
            let candidates = optimize_physical_asap(
                "L1",
                &roots,
                &[every_minute()],
                &arriving(),
                &DeploymentCapabilities::default(),
            )
            .unwrap();
            assert_eq!(candidates.len(), 2, "{query}");
        }
    }

    /// Only continuously ingesting time-series data is taken to arrive in
    /// time order: a counter over a table source, or over mixed data (whose
    /// at-rest part may be loaded in any order), stays at query time.
    #[test]
    fn counter_panes_over_other_sources_stay_at_query_time() {
        use IngestionPruneReason as Why;
        let root = in_panes(summary_root("rate(m[5m])"));
        let QueryRoot::Operator(node) = &root else {
            panic!("{root:?}")
        };
        let table = QueryRoot::Operator(to_table(node));
        let mixed = DataWorkload {
            arrival: DataArrival::Mixed {
                rows_per_second: 1_000.0,
            },
            ..Default::default()
        };
        for (root, data) in [(table, arriving()), (root.clone(), mixed)] {
            let roots = [root];
            let mut memo = HashMap::new();
            let implemented = map_roots(&roots, &mut |n| implement(n, &mut memo)).unwrap();
            let mat_candidates = materialization_candidates(
                &implemented,
                &[every_minute()],
                &data,
                &DeploymentCapabilities::default(),
            );
            assert_eq!(
                mat_candidates[0].at_ingestion,
                Err(Why::CounterNeedsTimeOrder)
            );
        }
    }

    /// `node` with every time-series scan turned into a table scan of the
    /// same name and schema.
    fn to_table(node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        let mut out = (**node).clone();
        out.operator = node.operator.map_children(to_table);
        if let Operator::NonASAP(NonASAPOp::Scan { source, .. }) = &mut out.operator {
            if let Source::TimeSeries { metric } = source {
                *source = Source::Table {
                    table_ref: metric.clone(),
                };
            }
        }
        Rc::new(out)
    }

    /// A whole-window build is maintained only for one window per
    /// evaluation at a known phase: a fixed interval with a phase, a window
    /// no longer than it, and no filter.
    #[test]
    fn a_whole_window_build_needs_a_known_phase_and_a_window_within_the_interval() {
        use IngestionPruneReason as Why;
        let root = summary_root("quantile_over_time(0.99, m[1m])");
        let at = |interval_ms: u32| QueryWorkloadEntry {
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
                interval: RepetitionInterval(interval_ms),
                evaluation_phase: TimestampMs(0),
            }),
            ..every_minute()
        };
        let mat_candidates = |root: &QueryRoot, demand: QueryWorkloadEntry| {
            let mut memo = HashMap::new();
            let implemented =
                map_roots(std::slice::from_ref(root), &mut |n| implement(n, &mut memo)).unwrap();
            materialization_candidates(
                &implemented,
                &[demand],
                &arriving(),
                &DeploymentCapabilities::default(),
            )
            .remove(0)
            .at_ingestion
        };
        assert_eq!(mat_candidates(&root, at(MINUTE as u32)), Ok(()));
        assert_eq!(
            mat_candidates(&root, every_minute()),
            Err(Why::WindowNotFixed)
        );
        assert_eq!(mat_candidates(&root, at(30_000)), Err(Why::WindowNotFixed));
        let candidates = optimize_physical_asap(
            "L1",
            std::slice::from_ref(&root),
            &[at(MINUTE as u32)],
            &arriving(),
            &DeploymentCapabilities::default(),
        )
        .unwrap();
        assert_eq!(materialized(&candidates[1]), [AT_INGESTION]);

        // The same build with a filter.
        let QueryRoot::Operator(node) = &root else {
            unreachable!()
        };
        let build = OperatorNode::reachable(node)
            .into_iter()
            .find(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
            .unwrap();
        let Operator::ASAP(ASAPOp::SummaryAgg {
            child,
            family,
            input,
            reduction,
            grouping,
            ..
        }) = build.operator.clone()
        else {
            unreachable!()
        };
        let filtered = Rc::new(
            OperatorNode::with_schema(
                Operator::ASAP(ASAPOp::SummaryAgg {
                    child,
                    family,
                    input,
                    reduction,
                    grouping,
                    filter: Some(asap_types::ir::Predicate(ScalarExpr::Literal(
                        asap_types::ir::scalar::expr_ir::ScalarValue::Boolean(true),
                    ))),
                }),
                build.schema.clone(),
            )
            .with_guarantee(build.guarantee.clone()),
        );
        assert_eq!(
            mat_candidates(&QueryRoot::Operator(filtered), at(MINUTE as u32)),
            Err(Why::Filtered)
        );
    }

    /// A build two queries read is also offered built once per evaluation
    /// for both (`MatAtQuery`); a build one query reads is not.
    #[test]
    fn a_build_two_queries_read_may_be_built_once_per_evaluation() {
        let QueryRoot::Operator(p99) = summary_root("quantile_over_time(0.99, m[1m])") else {
            unreachable!()
        };
        let build = OperatorNode::reachable(&p99)
            .into_iter()
            .find(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
            .unwrap();
        let p50 = Rc::new(
            OperatorNode::new(Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: Rc::clone(&build),
                query: asap_types::ir::schema::SketchStatistic::Quantile { q: 0.5 },
            }))
            .unwrap(),
        );
        let roots = [QueryRoot::Operator(p99), QueryRoot::Operator(p50)];
        let candidates = optimize_physical_asap(
            "L1",
            &roots,
            &[],
            &DataWorkload::default(),
            &DeploymentCapabilities::default(),
        )
        .unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            materialized(&candidates[1]),
            [Materialization::MatAtQuery(Medium::Memory)]
        );
        let [build] = &node_of(&candidates[1], "SummaryAgg")[..] else {
            panic!("one shared build")
        };
        assert_eq!(
            build.materialization,
            Materialization::MatAtQuery(Medium::Memory)
        );
    }

    /// Two queries share one scan: one reads it through panes maintained at
    /// ingestion time, the other at query time. The scan is split, one copy
    /// per timing; with nothing materialized it stays one node.
    #[test]
    fn a_scan_two_queries_read_at_two_timings_is_split() {
        let QueryRoot::Operator(panes) = in_panes(summary_root("quantile_over_time(0.99, m[5m])"))
        else {
            unreachable!()
        };
        let scan = OperatorNode::reachable(&panes)
            .into_iter()
            .find(|n| matches!(n.non_asap(), Some(NonASAPOp::Scan { .. })))
            .unwrap();
        let ranged = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
            range: std::time::Duration::from_millis(5 * MINUTE),
            kind: asap_types::ir::TimeRangeKind::Range,
            child: Rc::clone(&scan),
        }))
        .unwrap();
        let exact = asap_logical_asap_optimization::test_support::aggregate(
            Reduction::by(vec![]),
            vec![AggIntent::Max { col: None }],
            vec![],
            None,
            ranged,
        );
        let roots = [QueryRoot::Operator(panes), QueryRoot::Operator(exact)];
        let candidates = optimize_physical_asap(
            "L1",
            &roots,
            &[every_minute(), every_minute()],
            &arriving(),
            &DeploymentCapabilities::default(),
        )
        .unwrap();
        assert_eq!(candidates.len(), 2);
        let scans = |c: &PhysicalCandidate| {
            let mut timings: Vec<_> = c
                .dag
                .nodes
                .iter()
                .filter(|n| matches!(n.payload, Payload::NonASAP(NonASAPOp::Scan { .. })))
                .map(|n| n.output_state.timing)
                .collect();
            timings.sort_by_key(|t| *t == ExecutionTiming::QueryTime);
            timings
        };
        assert_eq!(scans(&candidates[0]), [ExecutionTiming::QueryTime]);
        assert_eq!(
            scans(&candidates[1]),
            [ExecutionTiming::IngestionTime, ExecutionTiming::QueryTime]
        );
    }

    /// Readers share one kept pane chain only when they evaluate at the
    /// same instants: two readers every minute at phases 0 and 2 min do,
    /// at phases 0 and 30 s they do not (each would need its own newest
    /// pane). A reader with no known phase is assumed to run with the
    /// others.
    #[test]
    fn readers_at_different_phases_do_not_build_one_pane_per_evaluation() {
        let at = |phase_ms: u64| {
            QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
                interval: RepetitionInterval(MINUTE as u32),
                evaluation_phase: TimestampMs(phase_ms),
            })
        };
        let unknown_phase = every_minute().recurrence;
        assert!(builds_one_pane_per_evaluation(
            MINUTE,
            [&at(0), &at(2 * MINUTE)]
        ));
        assert!(!builds_one_pane_per_evaluation(
            MINUTE,
            [&at(0), &at(30_000)]
        ));
        assert!(builds_one_pane_per_evaluation(
            MINUTE,
            [&at(30_000), &unknown_phase]
        ));
        assert!(!builds_one_pane_per_evaluation(
            MINUTE,
            [&at(0), &unknown_phase, &at(30_000)]
        ));
    }
}
