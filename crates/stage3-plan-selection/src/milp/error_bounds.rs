//! What constraint (K) reads (concepts doc, section 4): the error of
//! reading a summary state through each edge ($\varepsilon^{\mathrm{loc}}$,
//! $\delta^{\mathrm{loc}}$), each candidate's error contract
//! $\beta_{c,i}$, $b_{c,i}$ per metric
//! (`LogicalRequirements::error_propagation`), the largest error bound it
//! can carry (the big-M constants $M$), and each query's limits at its
//! root ($\varepsilon_q$, $\delta_q$).
//!
//! Errors are tracked per metric (absolute, absolute in either stream
//! norm, relative, rank): $E_{c,m}$. A consumer's contract in one metric
//! says nothing about another, so an input error in a metric its contract
//! does not carry must be zero when the edge is selected (the metric rule),
//! and a root accepts only the metrics its target accepts.
//!
//! An approximation enters where a state is read: a readout of a statistic,
//! or a merge, over a summary build with parameters $\boldsymbol\theta$.
//! Its bound depends on both the producer's $\boldsymbol\theta$ and the
//! statistic the consumer reads, so it is a constant per edge $(u, c)$
//! ([`CandidateErrorModel::read_error_on_edge`]): $E_c \ge \sum_u
//! \varepsilon^{\mathrm{loc}}_{u,c}\, y_{u,c,i} + \sum_i \beta_{c,i}
//! F_{c,i}$, linear since one $y$ per input is 1. A read whose guarantee
//! needs a fact the data workload does not declare (non-negative values
//! for a Count-Min count or a DDSketch interior quantile) has none.

use std::collections::HashMap;

use asap_accuracy::estimators::{distinct_count_guarantee_over_source, local_guarantee};
use asap_accuracy::BuiltinAccuracyModel;
use asap_accuracy::{AccuracyEvidenceProvider, AccuracyModel};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{
    FamilyCapability, FieldDataType, MergedError, SketchAlgorithm, SketchParams, SketchStatistic,
    SummaryInputExpr, SummaryUpdate, WeightDomain,
};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, QueryRoot, ScalarExpr};
use asap_types::stages::{
    DagOfSelectedCandidates, ErrorPropagation, LogicalRequirements, Memo, MemoGroupId,
    PhysicalASAPMemoCandidates, PhysicalCandidateId,
};
use asap_types::workload::bounds::{
    confirmed_weights, declared_non_negative, series_read, source_name, values_per_result,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

/// The metrics errors are tracked in, indexed by position.
pub(crate) const METRICS: [AccuracyMetric; METRIC_COUNT] = [
    AccuracyMetric::ABSOLUTE,
    AccuracyMetric::ABSOLUTE_STREAM_L1,
    AccuracyMetric::ABSOLUTE_STREAM_L2,
    AccuracyMetric::Relative,
    AccuracyMetric::Rank,
];
pub(crate) const METRIC_COUNT: usize = 5;

/// The relative tolerance of the built-in target check
/// (`asap_accuracy`'s `satisfies`): a $\boldsymbol\theta$ sized to meet
/// $\varepsilon$ exactly is not rejected by floating-point noise.
const TARGET_TOLERANCE: f64 = 1e-9;

/// The index of `metric` in [`METRICS`].
fn metric_index(metric: AccuracyMetric) -> usize {
    METRICS
        .iter()
        .position(|&known| known == metric)
        .expect("every metric a guarantee states is in METRICS")
}

/// An error bound per metric and a failure probability, or no guarantee:
/// the error of reading a state through one edge, or a selected node's
/// composed error.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ErrorBounds {
    /// The error bound in each metric, by index into [`METRICS`].
    pub(crate) error_bound: [f64; METRIC_COUNT],
    /// The failure probability; `None` when the guarantee leaves it
    /// unknown ((K) counts it as 1).
    pub(crate) failure_probability: Option<f64>,
    /// Part of the output is an approximation without a guarantee the
    /// planner can state (a family without an error model, a merge or
    /// tumbling windows that do not keep the guarantee, an unproven sign).
    pub(crate) without_guarantee: bool,
}

impl ErrorBounds {
    pub(crate) const EXACT: Self = Self {
        error_bound: [0.0; METRIC_COUNT],
        failure_probability: Some(0.0),
        without_guarantee: false,
    };

    const WITHOUT_GUARANTEE: Self = Self {
        without_guarantee: true,
        ..Self::EXACT
    };

    /// Adds `other`'s failure probability (a union bound; unknown stays
    /// unknown).
    fn add_failure_probability(&mut self, other: Option<f64>) {
        self.failure_probability = self.failure_probability.zip(other).map(|(a, b)| a + b);
    }
}

/// Each physical candidate's error model, by [`PhysicalCandidateId`]
/// ([`Self::new`]).
pub(crate) struct CandidateErrorModel {
    /// `error_propagation(c, m)` by metric index: $\beta_{c,i}$ and
    /// $b_{c,i}$, `None` without a contract in that metric.
    propagation: Vec<[Option<ErrorPropagation>; METRIC_COUNT]>,
    /// Carries every metric with factor 1 on every input: the only
    /// consumers that carry an output without a guarantee on.
    value_preserving: Vec<bool>,
    /// $|\mathcal I(c)|$.
    input_count: Vec<usize>,
    /// $\varepsilon^{\mathrm{loc}}_{u,c}$, $\delta^{\mathrm{loc}}_{u,c}$
    /// of each admissible edge on which consumer `c` reads a state that
    /// summary build `u` builds, by (producer, consumer) id; absent when
    /// the read is exact.
    read_error_on_edge: HashMap<(usize, usize), ErrorBounds>,
    /// The largest $E_{c,m}$ any selection gives; 0 when `c` never
    /// carries an `m` error.
    error_bound_ceiling: Vec<[f64; METRIC_COUNT]>,
    /// The largest $D_c$ any selection gives.
    failure_probability_ceiling: Vec<f64>,
    /// Whether some selection gives `c`'s output a part without a
    /// guarantee.
    may_lack_guarantee: Vec<bool>,
}

impl CandidateErrorModel {
    /// The error model of `physical`'s candidates, the data workload
    /// `data` answering the facts the guarantees need (values' signs, a
    /// source's distinct bound). `longest_path` bounds the edges of a
    /// selected path (the executions), for the ceilings.
    pub(crate) fn new(
        physical: &PhysicalASAPMemoCandidates,
        data: &DataWorkload,
        longest_path: usize,
    ) -> Self {
        let memo = &physical.logical_candidates.memo;
        let logical =
            |c: usize| memo.candidate(physical.physical_candidates[c].logical_candidate_id);
        let count = physical.physical_candidates.len();
        let mut propagation = Vec::with_capacity(count);
        let mut value_preserving = Vec::with_capacity(count);
        let mut input_count = Vec::with_capacity(count);
        for c in 0..count {
            let operator = &logical(c).expression.operator;
            let contracts: [Option<ErrorPropagation>; METRIC_COUNT] = std::array::from_fn(|m| {
                LogicalRequirements::error_propagation(operator, METRICS[m])
            });
            value_preserving.push(contracts.iter().all(|contract| {
                contract
                    .as_ref()
                    .is_some_and(|p| p.factor_per_input.iter().all(|&factor| factor == 1.0))
            }));
            propagation.push(contracts);
            input_count.push(operator.children().len());
        }
        // The reads, once per (producer, consumer) and statistics read.
        let mut statistics_of_group: HashMap<MemoGroupId, Vec<SketchStatistic>> = HashMap::new();
        let mut read_error_on_edge = HashMap::new();
        for (_, edge) in physical.edges() {
            if !(edge.satisfies_logical_requirement && edge.satisfies_physical_requirement) {
                continue;
            }
            let (u, c) = (edge.producer_candidate_id.0, edge.consumer_candidate_id.0);
            let statistics = match &logical(c).expression.operator {
                Operator::ASAP(ASAPOp::SummaryEstimate { query, .. }) => vec![query.clone()],
                Operator::ASAP(ASAPOp::SummaryMerge { .. }) => {
                    let group = physical.physical_candidates[c].logical_candidate_id.group;
                    statistics_of_group
                        .entry(group)
                        .or_insert_with(|| statistics_read(memo, group))
                        .clone()
                }
                _ => continue,
            };
            let producer = logical(u);
            let read = read_bounds(
                memo,
                &producer.expression.operator,
                producer.logical_properties.parameters(),
                producer.logical_properties.window_layout().is_some(),
                &statistics,
                matches!(
                    logical(c).expression.operator,
                    Operator::ASAP(ASAPOp::SummaryMerge { .. })
                ),
                data,
            );
            if read != ErrorBounds::EXACT {
                read_error_on_edge.insert((u, c), read);
            }
        }
        let mut model = Self {
            propagation,
            value_preserving,
            input_count,
            read_error_on_edge,
            error_bound_ceiling: Vec::new(),
            failure_probability_ceiling: Vec::new(),
            may_lack_guarantee: Vec::new(),
        };
        model.set_ceilings(physical, longest_path);
        model
    }

    /// The ceilings over `physical`'s admissible edges: rounds of
    /// $\bar E_{c,m} = \sum_i \max_{u \in P_{c,i}} (\beta_{c,i,m} \bar
    /// E_{u,m} + \varepsilon^{\mathrm{loc}}_{u,c,m})$ (an input whose
    /// contract does not carry $m$ passes nothing on: (K) makes it zero),
    /// likewise for $D$ (an unknown failure probability as 1) and the lack
    /// of a guarantee, until nothing changes. After $k$ rounds a ceiling
    /// bounds every selected path of at most $k$ edges, so stopping after
    /// `longest_path` rounds bounds every acyclic selection: each is a valid
    /// $M$. (On a cycle that carries error the ceilings grow with the
    /// rounds; none occurs in the workloads tested.)
    fn set_ceilings(&mut self, physical: &PhysicalASAPMemoCandidates, longest_path: usize) {
        let count = self.input_count.len();
        let edges: Vec<_> = physical.edges().map(|(_, edge)| edge).collect();
        let mut error = vec![[0.0; METRIC_COUNT]; count];
        let mut failure = vec![0.0; count];
        let mut lacks = vec![false; count];
        for _ in 0..longest_path.max(1) {
            let mut changed = false;
            for c in 0..count {
                let mut next_error = [0.0; METRIC_COUNT];
                let mut next_failure = 0.0;
                let mut next_lacks = false;
                for input in 0..self.input_count[c] {
                    let mut largest_error = [0.0_f64; METRIC_COUNT];
                    let mut largest_failure = 0.0_f64;
                    for edge in physical.admissible_edges_into(PhysicalCandidateId(c), input) {
                        let u = edges[edge.0].producer_candidate_id.0;
                        let read = self.read_error_on_edge(u, c);
                        for m in 0..METRIC_COUNT {
                            let carried = self.factor(c, input, m).unwrap_or(0.0) * error[u][m];
                            largest_error[m] = largest_error[m].max(carried + read.error_bound[m]);
                        }
                        let carried = if self.propagates_failure(c, input) {
                            failure[u]
                        } else {
                            0.0
                        };
                        largest_failure =
                            largest_failure.max(carried + read.failure_probability.unwrap_or(1.0));
                        next_lacks |=
                            read.without_guarantee || (lacks[u] && self.value_preserving[c]);
                    }
                    for m in 0..METRIC_COUNT {
                        next_error[m] += largest_error[m];
                    }
                    next_failure += largest_failure;
                }
                for m in 0..METRIC_COUNT {
                    if next_error[m] > error[c][m] {
                        error[c][m] = next_error[m];
                        changed = true;
                    }
                }
                if next_failure > failure[c] {
                    failure[c] = next_failure;
                    changed = true;
                }
                if next_lacks && !lacks[c] {
                    lacks[c] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        self.error_bound_ceiling = error;
        self.failure_probability_ceiling = failure;
        self.may_lack_guarantee = lacks;
    }

    /// $\varepsilon^{\mathrm{loc}}_{u,c}$, $\delta^{\mathrm{loc}}_{u,c}$ of
    /// consumer `c` reading producer `u`: exact unless `c` reads a state
    /// `u` builds.
    pub(crate) fn read_error_on_edge(&self, u: usize, c: usize) -> &ErrorBounds {
        self.read_error_on_edge
            .get(&(u, c))
            .unwrap_or(&ErrorBounds::EXACT)
    }

    /// $\beta_{c,i}$ in metric `m`, `None` without a contract in it.
    pub(crate) fn factor(&self, c: usize, input: usize, m: usize) -> Option<f64> {
        self.propagation[c][m]
            .as_ref()
            .map(|p| p.factor_per_input[input])
    }

    /// $b_{c,i}$ in metric `m`: the input error the linear bound holds
    /// under, `None` when it holds everywhere or there is no contract.
    pub(crate) fn validity_cap(&self, c: usize, input: usize, m: usize) -> Option<f64> {
        self.propagation[c][m]
            .as_ref()
            .and_then(|p| p.validity_cap_per_input[input])
    }

    /// Whether a failure of input `input`'s producer can fail `c`'s
    /// output: some metric reaches it with a positive factor, or `c`
    /// carries an output without a guarantee on.
    pub(crate) fn propagates_failure(&self, c: usize, input: usize) -> bool {
        self.value_preserving[c]
            || (0..METRIC_COUNT).any(|m| self.factor(c, input, m).is_some_and(|f| f > 0.0))
    }

    pub(crate) fn value_preserving(&self, c: usize) -> bool {
        self.value_preserving[c]
    }

    /// $M$ of $E_{c,m}$; 0 when `c` never carries an `m` error.
    pub(crate) fn error_bound_ceiling(&self, c: usize, m: usize) -> f64 {
        self.error_bound_ceiling[c][m]
    }

    /// $M$ of $D_c$.
    pub(crate) fn failure_probability_ceiling(&self, c: usize) -> f64 {
        self.failure_probability_ceiling[c]
    }

    pub(crate) fn may_lack_guarantee(&self, c: usize) -> bool {
        self.may_lack_guarantee[c]
    }

    /// The composed error of every node of `selected` (section 4: the
    /// root conditions over the selected edges, with no big-M), by
    /// `SelectedNodeId`: per input, its read error plus $\beta$ times the
    /// producer's error, failure probabilities by a union bound. An input
    /// error the consumer's contract does not carry (the metric rule), or
    /// one over its validity cap, leaves the output without a guarantee.
    pub(crate) fn composed_errors(&self, selected: &DagOfSelectedCandidates) -> Vec<ErrorBounds> {
        let nodes = selected.candidate_of_node.len();
        let mut producers: Vec<Vec<(usize, usize)>> = vec![Vec::new(); nodes];
        for edge in &selected.edges {
            producers[edge.consumer.0].push((edge.input_index_of_consumer, edge.producer.0));
        }
        let mut composed: Vec<Option<ErrorBounds>> = vec![None; nodes];
        // Depth first, inputs before their consumer; the selection is acyclic.
        for start in 0..nodes {
            let mut stack = vec![(start, false)];
            while let Some((node, inputs_done)) = stack.pop() {
                if composed[node].is_some() {
                    continue;
                }
                if !inputs_done {
                    stack.push((node, true));
                    stack.extend(
                        producers[node]
                            .iter()
                            .filter(|&&(_, producer)| composed[producer].is_none())
                            .map(|&(_, producer)| (producer, false)),
                    );
                    continue;
                }
                let c = selected.candidate_of_node[node].0;
                let mut error = ErrorBounds::EXACT;
                for &(input, producer) in &producers[node] {
                    let u = selected.candidate_of_node[producer].0;
                    let produced = composed[producer]
                        .as_ref()
                        .expect("an acyclic selection composes inputs first");
                    let read = self.read_error_on_edge(u, c);
                    for m in 0..METRIC_COUNT {
                        error.error_bound[m] += read.error_bound[m];
                        if produced.error_bound[m] == 0.0 {
                            continue;
                        }
                        match self.factor(c, input, m) {
                            Some(factor) => {
                                error.error_bound[m] += factor * produced.error_bound[m]
                            }
                            None => error.without_guarantee = true,
                        }
                        if self
                            .validity_cap(c, input, m)
                            .is_some_and(|cap| produced.error_bound[m] > cap)
                        {
                            error.without_guarantee = true;
                        }
                    }
                    error.add_failure_probability(read.failure_probability);
                    if self.propagates_failure(c, input) {
                        error.add_failure_probability(produced.failure_probability);
                    }
                    error.without_guarantee |= read.without_guarantee || produced.without_guarantee;
                }
                composed[node] = Some(error);
            }
        }
        composed
            .into_iter()
            .map(|error| error.expect("every node composed"))
            .collect()
    }
}

/// The error of consumer reads of `statistics` (a readout's one, or every
/// statistic read from a merge's group, when `merge`) from producer
/// `operator` with $\boldsymbol\theta$ `parameters`, built over tumbling
/// windows when `windowed`: exact unless `operator` builds a summary state;
/// for a sketch build, per metric the largest error and the largest
/// failure probability of the reads ([`read_guarantee`]); no guarantee
/// when one read has none, when a merge's family does not keep a
/// statistic's guarantee, or for a sample, wavelet or model state (no
/// error model).
fn read_bounds(
    memo: &Memo,
    operator: &Operator<MemoGroupId>,
    parameters: Option<&SketchParams>,
    windowed: bool,
    statistics: &[SketchStatistic],
    merge: bool,
    data: &DataWorkload,
) -> ErrorBounds {
    let Operator::ASAP(
        ASAPOp::SummaryAgg {
            family,
            input,
            child,
            ..
        }
        | ASAPOp::SummaryTumblingWindowChain {
            family,
            input,
            child,
            ..
        },
    ) = operator
    else {
        return ErrorBounds::EXACT;
    };
    match family {
        FieldDataType::ExactAggregate(..) | FieldDataType::Plain(_) => ErrorBounds::EXACT,
        FieldDataType::SummarySample(..)
        | FieldDataType::Wavelet(..)
        | FieldDataType::StatModel(..) => ErrorBounds::WITHOUT_GUARANTEE,
        FieldDataType::Sketch(..) => {
            let windowed = windowed
                || matches!(
                    operator,
                    Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
                );
            let mut bounds = ErrorBounds {
                failure_probability: Some(0.0),
                ..ErrorBounds::EXACT
            };
            for statistic in statistics {
                let Some(read) = read_guarantee(
                    memo,
                    family,
                    parameters,
                    windowed || merge,
                    input,
                    *child,
                    statistic,
                    data,
                ) else {
                    return ErrorBounds::WITHOUT_GUARANTEE;
                };
                let m = metric_index(read.metric);
                bounds.error_bound[m] = bounds.error_bound[m].max(read.error_bound);
                bounds.failure_probability = bounds
                    .failure_probability
                    .zip(read.failure_probability)
                    .map(|(a, b)| a.max(b));
            }
            bounds
        }
    }
}

/// The statistics read from state group `state`: by the readouts of it,
/// and of each group that merges it, transitively; each once.
fn statistics_read(memo: &Memo, state: MemoGroupId) -> Vec<SketchStatistic> {
    let mut statistics: Vec<SketchStatistic> = Vec::new();
    let mut pending = vec![state];
    let mut seen = vec![state];
    while let Some(group) = pending.pop() {
        for &parent in memo.groups()[group.0 as usize].parent_groups() {
            for consumer in &memo.groups()[parent.0 as usize].candidates {
                match &consumer.expression.operator {
                    Operator::ASAP(ASAPOp::SummaryEstimate {
                        summary_input,
                        query,
                    }) if *summary_input == group => {
                        if !statistics.contains(query) {
                            statistics.push(query.clone());
                        }
                    }
                    Operator::ASAP(ASAPOp::SummaryMerge { children, .. })
                        if children.contains(&group) && !seen.contains(&parent) =>
                    {
                        seen.push(parent);
                        pending.push(parent);
                    }
                    _ => {}
                }
            }
        }
    }
    statistics
}

/// The guarantee of one read of a sketch state.
struct ReadGuarantee {
    metric: AccuracyMetric,
    error_bound: f64,
    failure_probability: Option<f64>,
}

/// The guarantee of reading `statistic` from a sketch build of `family`
/// with $\boldsymbol\theta$ `parameters` over `update`s of group `rows`
/// (merged, from its tumbling windows or by a merge, when `merged`);
/// `None` when it has none:
/// - a merged state whose family does not keep the statistic's guarantee
///   (the family-capability table);
/// - no local guarantee (`asap_accuracy::estimators::local_guarantee`), or
///   an error bound it leaves unknown;
/// - a Count-Min count, or a DDSketch interior quantile, over values not
///   proven non-negative ([`proven_non_negative`]): the bound assumes them.
///
/// A distinct count (HLL, UnivMon) over one source's own rows whose
/// distinct bound the data workload declares gets its failure probability
/// at that bound (`distinct_count_guarantee_over_source`).
#[allow(clippy::too_many_arguments)]
fn read_guarantee(
    memo: &Memo,
    family: &FieldDataType,
    parameters: Option<&SketchParams>,
    merged: bool,
    update: &SummaryUpdate,
    rows: MemoGroupId,
    statistic: &SketchStatistic,
    data: &DataWorkload,
) -> Option<ReadGuarantee> {
    let FieldDataType::Sketch(algorithm, _) = family else {
        return None;
    };
    if merged
        && !matches!(
            FamilyCapability::of(family).merged_error(statistic),
            Some(MergedError::Exact | MergedError::AsIfBuiltOverUnion)
        )
    {
        return None;
    }
    let mut guarantee = local_guarantee(family, parameters, statistic)?;
    let needs_non_negative = match algorithm {
        SketchAlgorithm::Cms | SketchAlgorithm::CmsWithHeap => true,
        SketchAlgorithm::DDSketch => {
            matches!(statistic, SketchStatistic::Quantile { q } if *q > 0.0 && *q < 1.0)
        }
        _ => false,
    };
    if needs_non_negative && !proven_non_negative(memo, update, rows, data) {
        return None;
    }
    if matches!(statistic, SketchStatistic::Cardinality) {
        if let (Some(parameters), Some((source, max_distinct))) =
            (parameters, source_distinct_bound(memo, rows, data))
        {
            if let Some(over_source) =
                distinct_count_guarantee_over_source(parameters, &source, max_distinct)
            {
                guarantee = over_source;
            }
        }
    }
    Some(ReadGuarantee {
        metric: guarantee.spec.accuracy_metric,
        error_bound: guarantee.spec.error.evaluate()?,
        failure_probability: guarantee.spec.failure_probability.evaluate(),
    })
}

/// Whether the values `update` adds over group `rows` are proven
/// non-negative: its weight domain once the data workload's declared facts
/// answer it (`confirmed_weights`), or, for raw sample values, the metric
/// whose raw data samples `rows` are ([`raw_data_samples_of_metric`])
/// declared non-negative.
fn proven_non_negative(
    memo: &Memo,
    update: &SummaryUpdate,
    rows: MemoGroupId,
    data: &DataWorkload,
) -> bool {
    matches!(
        confirmed_weights(&update.weight_domain, data),
        WeightDomain::NonNegative { .. }
    ) || (update.weight == SummaryInputExpr::Column(ColumnRef::SampleValue)
        && raw_data_samples_of_metric(memo, rows)
            .is_some_and(|metric| declared_non_negative(data, &metric)))
}

/// The metric whose raw data samples, or plain sums of them, are every
/// value group `group` outputs: through a time range, an `offset`-only
/// time shift and a plain `sum`, to a time-series scan.
fn raw_data_samples_of_metric(memo: &Memo, group: MemoGroupId) -> Option<String> {
    match &memo.groups()[group.0 as usize].expression {
        Operator::NonASAP(NonASAPOp::Scan {
            source: asap_types::ir::operator::operator_properties::Source::TimeSeries { metric },
            ..
        }) => Some(metric.clone()),
        Operator::NonASAP(NonASAPOp::TimeRange { child, .. }) => {
            raw_data_samples_of_metric(memo, *child)
        }
        Operator::NonASAP(NonASAPOp::TimeShift { shift, child }) if shift.at.is_none() => {
            raw_data_samples_of_metric(memo, *child)
        }
        Operator::NonASAP(NonASAPOp::Aggregate {
            measures, child, ..
        }) if matches!(measures.as_slice(), [AggIntent::Sum { col: None }]) => {
            raw_data_samples_of_metric(memo, *child)
        }
        _ => None,
    }
}

/// The source whose own rows group `group` is (a scan behind filters,
/// time ranges and time shifts at most), with its declared bound on
/// distinct items per evaluation.
fn source_distinct_bound(
    memo: &Memo,
    group: MemoGroupId,
    data: &DataWorkload,
) -> Option<(String, u64)> {
    match &memo.groups()[group.0 as usize].expression {
        Operator::NonASAP(NonASAPOp::Scan { source, .. }) => {
            let name = source_name(source);
            let bound = data.sources.get(name)?.max_distinct_per_evaluation?;
            Some((name.to_string(), bound))
        }
        Operator::NonASAP(
            NonASAPOp::Filter { child, .. }
            | NonASAPOp::TimeRange { child, .. }
            | NonASAPOp::TimeShift { child, .. },
        ) => source_distinct_bound(memo, *child, data),
        _ => None,
    }
}

/// One query's limits at its root, (K): $E_{v,m} \le$
/// `error_bound_limit[m]` and $D_v \le$ `failure_probability_limit`
/// for its output candidate $v$, and no part without a guarantee.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RootAccuracyLimits {
    /// $\varepsilon_q$ (with the built-in check's tolerance) in a metric
    /// the target accepts, 0 in any other.
    pub(crate) error_bound_limit: [f64; METRIC_COUNT],
    /// $\delta_q$ over the values the target's scope covers, per value:
    /// $\delta_q / n$; 0 when $n$ is not bound; `None` without a
    /// $\delta$ target.
    pub(crate) failure_probability_limit: Option<f64>,
}

impl RootAccuracyLimits {
    /// The limits of `target` for a result of `values_per_result` values
    /// ($G_q$; `None` when not bound). A metric is accepted as the
    /// built-in check accepts it (`AccuracyModel::metric_can_meet`); an
    /// exact target accepts no error.
    pub(crate) fn of(target: &AccuracyTarget, values_per_result: Option<u64>) -> Self {
        if target.is_exact() {
            return Self::EXACT;
        }
        let error_bound_limit = std::array::from_fn(|m| {
            if BuiltinAccuracyModel.metric_can_meet(METRICS[m], target) {
                target.max_error * (1.0 + TARGET_TOLERANCE) + f64::EPSILON
            } else {
                0.0
            }
        });
        let failure_probability_limit = target.failure_probability.map(|delta| {
            match target.accuracy_scope.max_values_covered(values_per_result) {
                Some(values) if values > 0 => {
                    delta * (1.0 + TARGET_TOLERANCE) / values as f64 + f64::EPSILON
                }
                _ => 0.0,
            }
        });
        Self {
            error_bound_limit,
            failure_probability_limit,
        }
    }

    /// No error at all: the limits of an output whose composition has no
    /// error contract (a scalar root combining several outputs).
    pub(crate) const EXACT: Self = Self {
        error_bound_limit: [0.0; METRIC_COUNT],
        failure_probability_limit: Some(0.0),
    };
}

/// How many values one evaluation of each query returns ($G_q$), by
/// query: `values_per_result` of the root the front ends lowered.
pub(crate) fn values_per_result_per_query(
    query_roots: &[QueryRoot],
    data: &DataWorkload,
) -> Vec<Option<u64>> {
    query_roots
        .iter()
        .map(|root| values_per_result(root, series_read(root, data), data))
        .collect()
}

/// Each query's root limits, by query, `values_per_result[q]` its $G_q$;
/// every one `None` ((K) applies to no query) when the deployment's model
/// may accept an output regardless of its guarantee or its evidence may
/// meet a target (`AccuracyModel::may_accept_regardless_of_guarantee`,
/// `AccuracyEvidenceProvider::may_provide_evidence`, e.g.
/// `IgnoreAccuracyGuarantees`): their contract is that plan selection then
/// keeps every candidate a guarantee would rule out, and the check after
/// the solve decides.
pub(crate) fn root_accuracy_limits(
    entries: &[QueryWorkloadEntry],
    values_per_result: &[Option<u64>],
    model: &dyn AccuracyModel,
    evidence: &dyn AccuracyEvidenceProvider,
) -> Vec<Option<RootAccuracyLimits>> {
    let applies = !model.may_accept_regardless_of_guarantee() && !evidence.may_provide_evidence();
    entries
        .iter()
        .zip(values_per_result)
        .map(|(entry, &values)| {
            applies.then(|| RootAccuracyLimits::of(&entry.requirements.accuracy, values))
        })
        .collect()
}

/// Whether query root `root` returns its one output reference's values
/// unchanged: an operator root, or a scalar root that only reads one
/// operator's value (`scalar(v)`, a scalar subquery). Any other scalar
/// root combines outputs with no error contract, so its references must
/// be exact.
pub(crate) fn returns_its_output_unchanged<C>(root: &QueryRoot<C>) -> bool {
    match root {
        QueryRoot::Operator(_) => true,
        QueryRoot::Scalar(expression) => matches!(
            expression,
            ScalarExpr::PromqlScalarFromVector(_) | ScalarExpr::ScalarSubquery(_)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::operator::operator_properties::{Reduction, Source};
    use asap_types::ir::properties::AccuracyScope;
    use asap_types::ir::schema::{DataType, Field, GroupingStrategy, Schema};
    use asap_types::stages::{
        parameter_list, CandidateExpression, CandidateForMemoGroup, LogicalProperties,
        OriginOfCandidateForMemoGroup,
    };
    use asap_types::workload::SourceFacts;

    /// A rank target accepts only rank errors; a whole-result δ over `G`
    /// values allows δ / G per value, none when `G` is not bound; a
    /// per-value target allows δ; no δ target sets no limit; an exact
    /// target allows nothing.
    #[test]
    fn root_limits_accept_the_target_s_metric_and_divide_delta_over_its_scope() {
        let rank = AccuracyTarget::max_error(0.01)
            .accuracy_metric(AccuracyMetric::Rank)
            .failure_probability(0.01);
        let limits = RootAccuracyLimits::of(&rank, Some(1_000));
        for (m, &metric) in METRICS.iter().enumerate() {
            let expected = if metric == AccuracyMetric::Rank {
                0.01
            } else {
                0.0
            };
            assert!(
                (limits.error_bound_limit[m] - expected).abs() < 1e-9,
                "{metric:?}"
            );
        }
        let per_value_delta = limits.failure_probability_limit.unwrap();
        assert!((per_value_delta - 1e-5).abs() < 1e-9);
        assert_eq!(
            RootAccuracyLimits::of(&rank, None).failure_probability_limit,
            Some(0.0)
        );
        let per_value = rank.clone().accuracy_scope(AccuracyScope::PerValue);
        assert!(
            (RootAccuracyLimits::of(&per_value, None)
                .failure_probability_limit
                .unwrap()
                - 0.01)
                .abs()
                < 1e-9
        );
        let any_metric = AccuracyTarget::max_error(0.05);
        let limits = RootAccuracyLimits::of(&any_metric, None);
        assert!(limits.error_bound_limit.iter().all(|&limit| limit >= 0.05));
        assert_eq!(limits.failure_probability_limit, None);
        assert_eq!(
            RootAccuracyLimits::of(&AccuracyTarget::exact(), Some(10)),
            RootAccuracyLimits::EXACT
        );
    }

    fn add(
        memo: &mut Memo,
        group: MemoGroupId,
        operator: Operator<MemoGroupId>,
        parameters: Option<SketchParams>,
    ) {
        let expression = CandidateExpression { operator };
        let logical_properties = LogicalProperties::new(&expression, parameters, None).unwrap();
        let input_requirements =
            vec![LogicalRequirements::new(None); expression.operator.children().len()];
        memo.add_candidate(
            group,
            CandidateForMemoGroup {
                expression,
                logical_properties,
                input_requirements,
                fused_groups: Vec::new(),
                origin: OriginOfCandidateForMemoGroup::Pass1,
            },
        )
        .unwrap();
    }

    /// The memo of `latency` scanned, one `algorithm` state with
    /// `parameters` over it, and a readout group per quantile of
    /// `quantiles`; returns it with the state group.
    fn state_read_at(
        algorithm: SketchAlgorithm,
        parameters: &SketchParams,
        quantiles: &[f64],
    ) -> (Memo, MemoGroupId) {
        let mut memo = Memo::default();
        let scan = Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries {
                metric: "latency".into(),
            },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        });
        let rows = memo.find_or_add_group(scan.clone()).unwrap();
        add(&mut memo, rows, scan, None);
        let build = Operator::ASAP(ASAPOp::SummaryAgg {
            child: rows,
            family: FieldDataType::Sketch(algorithm, GroupingStrategy::default()),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            grouping: GroupingStrategy::default(),
            filter: None,
        });
        let state = memo.find_or_add_group(build.clone()).unwrap();
        add(&mut memo, state, build, Some(parameters.clone()));
        for &q in quantiles {
            let readout = Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: state,
                query: SketchStatistic::Quantile { q },
            });
            let group = memo.find_or_add_group(readout.clone()).unwrap();
            add(&mut memo, group, readout, None);
        }
        (memo, state)
    }

    /// The error of reading `statistics` from `state`'s first candidate,
    /// by a readout (`merge` false) or a merge.
    fn read(
        memo: &Memo,
        state: MemoGroupId,
        statistics: &[SketchStatistic],
        merge: bool,
        data: &DataWorkload,
    ) -> ErrorBounds {
        let candidate = &memo.groups()[state.0 as usize].candidates[0];
        read_bounds(
            memo,
            &candidate.expression.operator,
            candidate.logical_properties.parameters(),
            false,
            statistics,
            merge,
            data,
        )
    }

    fn quantile(q: f64) -> SketchStatistic {
        SketchStatistic::Quantile { q }
    }

    /// A read's error is its own statistic's on the producer's θ: a KLL
    /// read has its rank error; a DDSketch read at an interior quantile has
    /// a relative error only over values declared non-negative, while a
    /// read of the same state at the maximum (an extremum) always has one.
    /// A merge reading several statistics takes the largest per metric and
    /// has no guarantee when one of them has none.
    #[test]
    fn a_read_s_error_is_its_statistic_s_on_the_producer_s_parameters() {
        let kll = parameter_list(SketchAlgorithm::Kll)[2].clone();
        let (memo, state) = state_read_at(SketchAlgorithm::Kll, &kll, &[0.5, 0.99]);
        let expected = local_guarantee(
            &FieldDataType::Sketch(SketchAlgorithm::Kll, GroupingStrategy::default()),
            Some(&kll),
            &quantile(0.5),
        )
        .unwrap();
        let bounds = read(
            &memo,
            state,
            &[quantile(0.5)],
            false,
            &DataWorkload::default(),
        );
        assert!(!bounds.without_guarantee);
        assert_eq!(
            bounds.error_bound[metric_index(AccuracyMetric::Rank)],
            expected.spec.error.evaluate().unwrap()
        );
        assert_eq!(
            bounds.failure_probability,
            expected.spec.failure_probability.evaluate()
        );
        let ddsketch = SketchParams::DDSketch { alpha: 0.01 };
        let (memo, state) = state_read_at(SketchAlgorithm::DDSketch, &ddsketch, &[0.5, 1.0]);
        let undeclared = DataWorkload::default();
        assert!(read(&memo, state, &[quantile(0.5)], false, &undeclared).without_guarantee);
        let at_the_maximum = read(&memo, state, &[quantile(1.0)], false, &undeclared);
        assert!(!at_the_maximum.without_guarantee);
        assert_eq!(
            at_the_maximum.error_bound[metric_index(AccuracyMetric::Relative)],
            0.01
        );
        let both = [quantile(0.5), quantile(1.0)];
        assert!(read(&memo, state, &both, true, &undeclared).without_guarantee);
        let mut declared = DataWorkload::default();
        declared.sources.insert(
            "latency".into(),
            SourceFacts {
                values_non_negative: true,
                ..Default::default()
            },
        );
        let merged = read(&memo, state, &both, true, &declared);
        assert!(!merged.without_guarantee);
        assert_eq!(
            merged.error_bound[metric_index(AccuracyMetric::Relative)],
            0.01
        );
    }
}
