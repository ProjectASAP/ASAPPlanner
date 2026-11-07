//! What constraint (K) reads (concepts doc, section 4): each physical
//! candidate's own guarantee $\varepsilon^{\mathrm{loc}}_c$,
//! $\delta^{\mathrm{loc}}_c$, its error contract $\beta_{c,i}$, $b_{c,i}$
//! per metric (`LogicalRequirements::error_propagation`), the largest
//! error bound it can carry (the big-M constants $M$), and each query's
//! limits at its root ($\varepsilon_q$, $\delta_q$).
//!
//! Errors are kept per metric (absolute, absolute in either stream norm,
//! relative, rank): $E_{c,m}$. A consumer's contract in one metric says
//! nothing about another, so an input error in a metric its contract does
//! not carry must be zero when the edge is selected (the metric rule), and
//! a root accepts only the metrics its target accepts.
//!
//! A sketch state's own guarantee is the error of reading it: the
//! statistics read from its group, directly or through merges
//! ([`statistics_read`]), each through `asap_accuracy`'s local guarantee
//! of its $\boldsymbol\theta$; per metric the largest of them, and the
//! largest failure probability. A read whose guarantee needs a fact the
//! data workload does not declare (non-negative values for a Count-Min
//! count or a DDSketch interior quantile) has no guarantee.

use asap_accuracy::estimators::{distinct_count_guarantee_over_source, local_guarantee};
use asap_accuracy::{AccuracyModel, BuiltinAccuracyModel};
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

/// The metrics errors are kept in, indexed by position.
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

/// The largest error bound or failure probability a ceiling is carried
/// to: a larger one only rejects more plans (concepts doc: $M$ is any
/// upper bound; a smaller one can only make (K) stricter).
const CEILING_CAP: f64 = 1e6;

/// The index of `metric` in [`METRICS`].
fn metric_index(metric: AccuracyMetric) -> usize {
    METRICS
        .iter()
        .position(|&known| known == metric)
        .expect("every metric a guarantee states is in METRICS")
}

/// One candidate's own guarantee.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OwnGuarantee {
    /// $\varepsilon^{\mathrm{loc}}_{c,m}$, by metric index.
    pub(crate) error_bound: [f64; METRIC_COUNT],
    /// $\delta^{\mathrm{loc}}_c$; `None` when its guarantee leaves it
    /// unknown (counted as 1 by (K)).
    pub(crate) failure_probability: Option<f64>,
    /// Its output carries an approximation without a guarantee the
    /// planner can state (a read whose family has no error model, a merge
    /// that does not keep the guarantee, an unproven sign).
    pub(crate) without_guarantee: bool,
}

impl OwnGuarantee {
    const EXACT: Self = Self {
        error_bound: [0.0; METRIC_COUNT],
        failure_probability: Some(0.0),
        without_guarantee: false,
    };
}

/// Each physical candidate's error model, by [`PhysicalCandidateId`]
/// ([`Self::new`]).
pub(crate) struct CandidateErrorModel {
    own: Vec<OwnGuarantee>,
    /// `error_propagation(c, m)` by metric index: $\beta_{c,i}$ and
    /// $b_{c,i}$, `None` without a contract in that metric.
    propagation: Vec<[Option<ErrorPropagation>; METRIC_COUNT]>,
    /// Carries every metric with factor 1 on every input: the only
    /// consumers that carry an output without a guarantee on.
    value_preserving: Vec<bool>,
    /// $|\mathcal I(c)|$.
    input_count: Vec<usize>,
    /// The largest $E_{c,m}$ any selection gives (capped at
    /// [`CEILING_CAP`]); 0 when `c` never carries an `m` error.
    error_bound_ceiling: Vec<[f64; METRIC_COUNT]>,
    /// The largest $D_c$ any selection gives (capped).
    failure_probability_ceiling: Vec<f64>,
    /// Whether some selection gives `c`'s output a part without a
    /// guarantee.
    may_lack_guarantee: Vec<bool>,
}

impl CandidateErrorModel {
    /// The error model of `physical`'s candidates, the data workload
    /// `data` answering the facts their guarantees need (values' signs,
    /// a source's distinct bound).
    pub(crate) fn new(physical: &PhysicalASAPMemoCandidates, data: &DataWorkload) -> Self {
        let memo = &physical.logical_candidates.memo;
        let operator = |c: usize| {
            &memo
                .candidate(physical.physical_candidates[c].logical_candidate_id)
                .expression
                .operator
        };
        let count = physical.physical_candidates.len();
        let mut own = Vec::with_capacity(count);
        let mut propagation = Vec::with_capacity(count);
        let mut value_preserving = Vec::with_capacity(count);
        let mut input_count = Vec::with_capacity(count);
        for (c, candidate) in physical.physical_candidates.iter().enumerate() {
            let logical = memo.candidate(candidate.logical_candidate_id);
            own.push(own_guarantee(
                memo,
                candidate.logical_candidate_id.group,
                operator(c),
                logical.logical_properties.parameters(),
                logical.logical_properties.window_layout().is_some(),
                data,
            ));
            let contracts: [Option<ErrorPropagation>; METRIC_COUNT] = std::array::from_fn(|m| {
                LogicalRequirements::error_propagation(operator(c), METRICS[m])
            });
            value_preserving.push(contracts.iter().all(|contract| {
                contract
                    .as_ref()
                    .is_some_and(|p| p.factor_per_input.iter().all(|&factor| factor == 1.0))
            }));
            propagation.push(contracts);
            input_count.push(operator(c).children().len());
        }
        let mut model = Self {
            own,
            propagation,
            value_preserving,
            input_count,
            error_bound_ceiling: Vec::new(),
            failure_probability_ceiling: Vec::new(),
            may_lack_guarantee: Vec::new(),
        };
        model.set_ceilings(physical);
        model
    }

    /// The ceilings over `physical`'s admissible edges: rounds of
    /// $\bar E_{c,m} = \varepsilon^{\mathrm{loc}}_{c,m} + \sum_i
    /// \beta_{c,i,m} \max_{u \in P_{c,i}} \bar E_{u,m}$ (an input whose
    /// contract does not carry $m$ adds nothing: (K) makes it zero), and
    /// likewise for $D$ and the lack of a guarantee, until nothing changes.
    /// A selection is acyclic, so its error is reached within as many
    /// rounds as there are candidates; the rounds stop there.
    fn set_ceilings(&mut self, physical: &PhysicalASAPMemoCandidates) {
        let count = self.own.len();
        let edges: Vec<_> = physical.edges().map(|(_, edge)| edge).collect();
        let mut error = vec![[0.0; METRIC_COUNT]; count];
        let mut failure = vec![0.0; count];
        let mut lacks = vec![false; count];
        for _ in 0..count.max(1) {
            let mut changed = false;
            for c in 0..count {
                let own = &self.own[c];
                let mut next_error = own.error_bound;
                let mut next_failure = own.failure_probability.unwrap_or(1.0);
                let mut next_lacks = own.without_guarantee;
                for input in 0..self.input_count[c] {
                    let producers = physical
                        .admissible_edges_into(PhysicalCandidateId(c), input)
                        .iter()
                        .map(|edge| edges[edge.0].producer_candidate_id.0);
                    let mut largest_error = [0.0_f64; METRIC_COUNT];
                    let mut largest_failure = 0.0_f64;
                    let mut any_lacks = false;
                    for u in producers {
                        for m in 0..METRIC_COUNT {
                            largest_error[m] = largest_error[m].max(error[u][m]);
                        }
                        largest_failure = largest_failure.max(failure[u]);
                        any_lacks |= lacks[u];
                    }
                    for (m, largest) in largest_error.iter().enumerate() {
                        next_error[m] += self.factor(c, input, m).unwrap_or(0.0) * largest;
                    }
                    if self.propagates_failure(c, input) {
                        next_failure += largest_failure;
                    }
                    next_lacks |= any_lacks && self.value_preserving[c];
                }
                for m in 0..METRIC_COUNT {
                    let next = next_error[m].min(CEILING_CAP);
                    if next > error[c][m] {
                        error[c][m] = next;
                        changed = true;
                    }
                }
                let next_failure = next_failure.min(CEILING_CAP);
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

    pub(crate) fn own(&self, c: usize) -> &OwnGuarantee {
        &self.own[c]
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
}

impl CandidateErrorModel {
    /// The composed error of every node of `selected` (section 4: the
    /// root conditions over the selected edges, with no big-M), by
    /// `SelectedNodeId`: its own guarantee plus $\beta$ times each input's,
    /// failure probabilities by a union bound. An input error the
    /// consumer's contract does not carry (the metric rule), or one over its
    /// validity cap, leaves the output without a guarantee.
    pub(crate) fn composed_errors(&self, selected: &DagOfSelectedCandidates) -> Vec<OwnGuarantee> {
        let nodes = selected.candidate_of_node.len();
        let mut producers: Vec<Vec<(usize, usize)>> = vec![Vec::new(); nodes];
        for edge in &selected.edges {
            producers[edge.consumer.0].push((edge.input_index_of_consumer, edge.producer.0));
        }
        let mut composed: Vec<Option<OwnGuarantee>> = vec![None; nodes];
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
                let mut error = self.own[c].clone();
                for &(input, producer) in &producers[node] {
                    let read = composed[producer]
                        .as_ref()
                        .expect("an acyclic selection composes inputs first");
                    for m in 0..METRIC_COUNT {
                        if read.error_bound[m] == 0.0 {
                            continue;
                        }
                        match self.factor(c, input, m) {
                            Some(factor) => error.error_bound[m] += factor * read.error_bound[m],
                            None => error.without_guarantee = true,
                        }
                        if self
                            .validity_cap(c, input, m)
                            .is_some_and(|cap| read.error_bound[m] > cap)
                        {
                            error.without_guarantee = true;
                        }
                    }
                    if self.propagates_failure(c, input) {
                        error.failure_probability = error
                            .failure_probability
                            .zip(read.failure_probability)
                            .map(|(own, read)| own + read);
                    }
                    if read.without_guarantee {
                        error.without_guarantee = true;
                    }
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

/// The own guarantee of the candidate `operator` of group `group`, with
/// $\boldsymbol\theta$ `parameters`, built over tumbling windows when
/// `windowed`:
/// - a sketch build: for each statistic read from its group
///   ([`statistics_read`]), the guarantee of reading it
///   ([`read_guarantee`]); per metric the largest error, the largest
///   failure probability; no guarantee when one read has none;
/// - a build of a sample, wavelet or model state: no guarantee (no error
///   model);
/// - a merge of sketch states whose family's merged state does not keep
///   the guarantee of a statistic read from it: no guarantee;
/// - every other candidate: exact (its error comes from its inputs).
fn own_guarantee(
    memo: &Memo,
    group: MemoGroupId,
    operator: &Operator<MemoGroupId>,
    parameters: Option<&SketchParams>,
    windowed: bool,
    data: &DataWorkload,
) -> OwnGuarantee {
    let without_guarantee = OwnGuarantee {
        without_guarantee: true,
        ..OwnGuarantee::EXACT
    };
    match operator {
        Operator::ASAP(
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
        ) => match family {
            FieldDataType::Sketch(..) => {
                let windowed = windowed
                    || matches!(
                        operator,
                        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
                    );
                let mut own = OwnGuarantee::EXACT;
                for statistic in statistics_read(memo, group) {
                    let Some(read) = read_guarantee(
                        memo, family, parameters, windowed, input, *child, &statistic, data,
                    ) else {
                        return without_guarantee;
                    };
                    let m = metric_index(read.metric);
                    own.error_bound[m] = own.error_bound[m].max(read.error_bound);
                    own.failure_probability =
                        match (own.failure_probability, read.failure_probability) {
                            (Some(a), Some(b)) => Some(a.max(b)),
                            _ => None,
                        };
                }
                own
            }
            FieldDataType::ExactAggregate(..) | FieldDataType::Plain(_) => OwnGuarantee::EXACT,
            FieldDataType::SummarySample(..)
            | FieldDataType::Wavelet(..)
            | FieldDataType::StatModel(..) => without_guarantee,
        },
        Operator::ASAP(ASAPOp::SummaryMerge { children, .. }) => {
            let statistics = statistics_read(memo, group);
            let keeps = children.iter().all(|&child| {
                match state_family(&memo.groups()[child.0 as usize].expression) {
                    Some(family @ FieldDataType::Sketch(..)) => statistics.iter().all(|s| {
                        matches!(
                            FamilyCapability::of(family).merged_error(s),
                            Some(MergedError::Exact | MergedError::AsIfBuiltOverUnion)
                        )
                    }),
                    // An exact state merges exactly; a merge over a merge
                    // or another state is judged at its own level.
                    _ => true,
                }
            });
            if keeps {
                OwnGuarantee::EXACT
            } else {
                without_guarantee
            }
        }
        _ => OwnGuarantee::EXACT,
    }
}

/// The family a state build's expression builds; `None` for any other.
fn state_family(operator: &Operator<MemoGroupId>) -> Option<&FieldDataType> {
    match operator {
        Operator::ASAP(
            ASAPOp::SummaryAgg { family, .. } | ASAPOp::SummaryTumblingWindowChain { family, .. },
        ) => Some(family),
        _ => None,
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
/// (merged from its tumbling windows when `windowed`); `None` when it has
/// none:
/// - a windowed build whose family's merged state does not keep the
///   statistic's guarantee (the family-capability table);
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
    windowed: bool,
    update: &SummaryUpdate,
    rows: MemoGroupId,
    statistic: &SketchStatistic,
    data: &DataWorkload,
) -> Option<ReadGuarantee> {
    let FieldDataType::Sketch(algorithm, _) = family else {
        return None;
    };
    if windowed
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

/// Each query's root limits, by query: `None` when (K) does not apply to
/// it, because the deployment's accuracy model accepts its output
/// whatever its guarantee (`AccuracyModel::accepts_output_regardless_of_guarantee`
/// on the root the front ends lowered, `query_roots[q]`; e.g.
/// `IgnoreAccuracyGuarantees`). $G_q$ is `values_per_result` of that root.
pub(crate) fn root_accuracy_limits(
    entries: &[QueryWorkloadEntry],
    query_roots: &[QueryRoot],
    data: &DataWorkload,
    model: &dyn AccuracyModel,
) -> Vec<Option<RootAccuracyLimits>> {
    entries
        .iter()
        .enumerate()
        .map(|(query, entry)| {
            let target = &entry.requirements.accuracy;
            let root = query_roots.get(query)?;
            if model.accepts_output_regardless_of_guarantee(root, target) {
                return None;
            }
            let values = values_per_result(root, series_read(root, data), data);
            Some(RootAccuracyLimits::of(target, values))
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

    fn own_of_state(memo: &Memo, state: MemoGroupId, data: &DataWorkload) -> OwnGuarantee {
        let candidate = &memo.groups()[state.0 as usize].candidates[0];
        own_guarantee(
            memo,
            state,
            &candidate.expression.operator,
            candidate.logical_properties.parameters(),
            false,
            data,
        )
    }

    /// A KLL state read at p50 and p99 has its rank error once; a DDSketch
    /// state read at an interior quantile has a relative error only over
    /// values declared non-negative, at the maximum (an extremum) always.
    #[test]
    fn a_state_s_own_guarantee_is_its_reads_largest_error_per_metric() {
        let kll = parameter_list(SketchAlgorithm::Kll)[2].clone();
        let (memo, state) = state_read_at(SketchAlgorithm::Kll, &kll, &[0.5, 0.99]);
        let own = own_of_state(&memo, state, &DataWorkload::default());
        let expected = local_guarantee(
            &FieldDataType::Sketch(SketchAlgorithm::Kll, GroupingStrategy::default()),
            Some(&kll),
            &SketchStatistic::Quantile { q: 0.5 },
        )
        .unwrap();
        assert!(!own.without_guarantee);
        assert_eq!(
            own.error_bound[metric_index(AccuracyMetric::Rank)],
            expected.spec.error.evaluate().unwrap()
        );
        assert_eq!(
            own.failure_probability,
            expected.spec.failure_probability.evaluate()
        );
        let ddsketch = SketchParams::DDSketch { alpha: 0.01 };
        let (memo, state) = state_read_at(SketchAlgorithm::DDSketch, &ddsketch, &[0.5]);
        assert!(own_of_state(&memo, state, &DataWorkload::default()).without_guarantee);
        let mut declared = DataWorkload::default();
        declared.sources.insert(
            "latency".into(),
            SourceFacts {
                values_non_negative: true,
                ..Default::default()
            },
        );
        let own = own_of_state(&memo, state, &declared);
        assert!(!own.without_guarantee);
        assert_eq!(
            own.error_bound[metric_index(AccuracyMetric::Relative)],
            0.01
        );
        let (memo, state) = state_read_at(SketchAlgorithm::DDSketch, &ddsketch, &[1.0]);
        assert!(!own_of_state(&memo, state, &DataWorkload::default()).without_guarantee);
    }
}
