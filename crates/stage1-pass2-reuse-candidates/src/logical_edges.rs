//! After the rules: the error caps of every input, the parameter floors
//! they set, and the logical edges $(u, c, i)$ with $\alpha^{\log}$
//! (concepts doc, sections 3 and 4; #1201).
//!
//! Three passes over the memo's groups in dependency order:
//! 1. **Caps, top down.** Query $q$'s root group may be off by its target
//!    $\varepsilon_q$. A consumer $c$ passes a cap to input $i$ through its
//!    error contract (`LogicalRequirements::error_propagation`):
//!    $\min(b_{c,i},\ \text{cap}/\beta_{c,i})$. A group keeps, per query,
//!    the largest cap any consumer passes it: the bottleneck over paths,
//!    with $\lambda_{q,c,i}$ bounded below along any path, so the cap
//!    removes only what no plan can use.
//! 2. **Floors, top down.** A readout of a sketch state gets the floor of
//!    its group's cap for the statistic it reads ([`floor_for_cap`]),
//!    tightened with the floor it already has (a top-k's heap size). A
//!    merge of sketch states gets the weakest floor of the consumers
//!    reading its group: the merged state has its inputs' $\boldsymbol\theta$.
//! 3. **Output errors, bottom up, and edges.** Each candidate's possible
//!    output errors ([`OutputError`]): exact, a bound in one metric, or no
//!    guarantee. The metric rule: an input whose error is in a metric the
//!    consumer's contract does not carry gives no edge ($\alpha^{\log} =
//!    0$); an exact producer always may. $\alpha^{\log}$ also needs the
//!    producer's $\boldsymbol\theta$ to meet the floor.

use crate::memo::rules::{expression, produces_state, Pass2RuleError};
use crate::parameter_floors::{floor_for_cap, strongest, weakest_of};
use asap_accuracy::estimators::local_guarantee;
use asap_types::ir::properties::AccuracyMetric;
use asap_types::ir::schema::{
    FamilyCapability, FieldDataType, MergedError, SketchAlgorithm, SketchParams, SketchStatistic,
};
use asap_types::ir::{ASAPOp, Operator, QueryRoot};
use asap_types::stages::{
    producer_satisfies_logical_requirements, CandidateForMemoGroupId, LogicalEdge,
    LogicalRequirements, Memo, MemoGroupId,
};
use asap_types::workload::QueryWorkloadEntry;

/// The metrics a contract may be read in when a query's target names none.
const METRICS: [AccuracyMetric; 3] = [
    AccuracyMetric::ABSOLUTE,
    AccuracyMetric::Relative,
    AccuracyMetric::Rank,
];

/// What error a candidate's output may carry, by the producers its inputs
/// may read (the union over them).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum OutputError {
    /// No approximation error.
    Exact,
    /// An error bound in this metric.
    Bound(AccuracyMetric),
    /// An approximation without a guarantee the planner can state (a heap
    /// readout of merged windows; a family without an error model): only a
    /// value-preserving consumer carries it on, for Stage 3's accuracy
    /// model to judge.
    WithoutGuarantee,
}

/// One cap of a group: query `query` may use this group's output with an
/// error of at most `cap` (infinite: any).
#[derive(Debug, Clone)]
struct ErrorCap {
    query: usize,
    cap: f64,
}

/// Sets every candidate's parameter floors from its inputs' error caps and
/// returns the logical edges, grouped by consumer and input, producers in
/// group order. `entries[q]` is query `q`'s workload entry (its accuracy
/// target).
pub(crate) fn logical_edges_with_capped_floors(
    memo: &mut Memo,
    entries: &[QueryWorkloadEntry],
) -> Result<Vec<LogicalEdge>, Pass2RuleError> {
    let order = memo
        .topological_order()
        .ok_or(Pass2RuleError::CyclicDependencies)?;
    let caps = error_caps(memo, entries, &order);
    set_floors(memo, entries, &caps, &order);
    let errors = output_errors(memo, &order);
    let mut edges = Vec::new();
    for (g, group) in memo.groups().iter().enumerate() {
        for (index_in_group, consumer) in group.candidates.iter().enumerate() {
            let consumer_id = CandidateForMemoGroupId {
                group: MemoGroupId(g as u32),
                index_in_group,
            };
            for (input, &child) in consumer
                .expression
                .operator
                .children()
                .into_iter()
                .enumerate()
            {
                for (u, producer) in memo.groups()[child.0 as usize]
                    .candidates
                    .iter()
                    .enumerate()
                {
                    let satisfies = producer_satisfies_logical_requirements(
                        &producer.logical_properties,
                        &consumer.input_requirements[input],
                    ) && (produces_state(expression(memo, child))
                        || errors[child.0 as usize][u]
                            .iter()
                            .any(|error| carries(&consumer.expression.operator, *error)));
                    edges.push(LogicalEdge {
                        producer_candidate_id: CandidateForMemoGroupId {
                            group: child,
                            index_in_group: u,
                        },
                        consumer_candidate_id: consumer_id,
                        input_index_of_consumer: input,
                        satisfies_logical_requirement: satisfies,
                    });
                }
            }
        }
    }
    Ok(edges)
}

/// The metric rule: whether `consumer`'s error contract carries an input
/// error `error`. Exact always; a bound in a metric its contract has;
/// no guarantee only through a value-preserving consumer (one whose
/// contract carries every metric with factor 1).
fn carries(consumer: &Operator<MemoGroupId>, error: OutputError) -> bool {
    match error {
        OutputError::Exact => true,
        OutputError::Bound(metric) => {
            LogicalRequirements::error_propagation(consumer, metric).is_some()
        }
        OutputError::WithoutGuarantee => value_preserving(consumer),
    }
}

/// Whether `consumer` carries every metric unchanged (factor 1 on every
/// input): a projection of columns, a time range, a readout or a merge.
fn value_preserving(consumer: &Operator<MemoGroupId>) -> bool {
    METRICS.iter().all(|&metric| {
        LogicalRequirements::error_propagation(consumer, metric).is_some_and(|propagation| {
            propagation
                .factor_per_input
                .iter()
                .all(|&factor| factor == 1.0)
        })
    })
}

/// Each group's caps ([`ErrorCap`]), by group id: each query root group's
/// target, then every consumer's caps passed down through its contract.
fn error_caps(
    memo: &Memo,
    entries: &[QueryWorkloadEntry],
    order: &[MemoGroupId],
) -> Vec<Vec<ErrorCap>> {
    let mut caps: Vec<Vec<ErrorCap>> = vec![Vec::new(); memo.groups().len()];
    // A scalar root's expression is not read here: its operands get no cap.
    for (query, root) in memo.query_roots.iter().enumerate() {
        let (Some(entry), QueryRoot::Operator(group)) = (entries.get(query), root) else {
            continue;
        };
        add_cap(
            &mut caps[group.0 as usize],
            ErrorCap {
                query,
                cap: entry.requirements.accuracy.max_error,
            },
        );
    }
    for &group in order.iter().rev() {
        let group_caps = caps[group.0 as usize].clone();
        if group_caps.is_empty() {
            continue;
        }
        for consumer in &memo.groups()[group.0 as usize].candidates {
            let operator = &consumer.expression.operator;
            for (input, &child) in operator.children().into_iter().enumerate() {
                for cap in &group_caps {
                    let target_metric = entries
                        .get(cap.query)
                        .and_then(|entry| entry.requirements.accuracy.accuracy_metric);
                    if let Some(passed) = passed_cap(operator, input, cap, target_metric) {
                        add_cap(&mut caps[child.0 as usize], passed);
                    }
                }
            }
        }
    }
    caps
}

/// The cap `consumer` passes to its input `input` from its own group's
/// `cap`, through its contract in the query's target metric (in every
/// metric when the target names none, the largest); `None` when it has no
/// contract for it.
fn passed_cap(
    consumer: &Operator<MemoGroupId>,
    input: usize,
    cap: &ErrorCap,
    target_metric: Option<AccuracyMetric>,
) -> Option<ErrorCap> {
    let metrics: &[AccuracyMetric] = match &target_metric {
        Some(metric) => std::slice::from_ref(metric),
        None => &METRICS,
    };
    let passed = metrics
        .iter()
        .filter_map(|&metric| {
            let propagation = LogicalRequirements::error_propagation(consumer, metric)?;
            let factor = propagation.factor_per_input[input];
            let validity = propagation.validity_cap_per_input[input].unwrap_or(f64::INFINITY);
            Some(if factor > 0.0 {
                validity.min(cap.cap / factor)
            } else {
                validity
            })
        })
        .reduce(f64::max)?;
    Some(ErrorCap {
        query: cap.query,
        cap: passed,
    })
}

/// Whether a merged state of `state`'s family keeps the guarantee of
/// `statistic` (exact, or as if built over the union).
fn keeps_guarantee(memo: &Memo, state: MemoGroupId, statistic: &SketchStatistic) -> bool {
    family(expression(memo, state)).is_some_and(|family| merge_keeps_guarantee(family, statistic))
}

/// Whether a merged state of `family` keeps one build's guarantee of
/// `statistic`: its merged error is exact or as if built over the union
/// (the family-capability table).
fn merge_keeps_guarantee(family: &FieldDataType, statistic: &SketchStatistic) -> bool {
    matches!(
        FamilyCapability::of(family).merged_error(statistic),
        Some(MergedError::Exact | MergedError::AsIfBuiltOverUnion)
    )
}

/// The family a state operator builds or merges; `None` for a merge (its
/// inputs' family) and every other operator.
fn family(operator: &Operator<MemoGroupId>) -> Option<&FieldDataType> {
    match operator {
        Operator::ASAP(
            ASAPOp::SummaryAgg { family, .. } | ASAPOp::SummaryTumblingWindowChain { family, .. },
        ) => Some(family),
        _ => None,
    }
}

/// Adds `cap` to `caps`, keeping the larger cap of one query.
fn add_cap(caps: &mut Vec<ErrorCap>, cap: ErrorCap) {
    match caps.iter_mut().find(|known| known.query == cap.query) {
        Some(known) => known.cap = known.cap.max(cap.cap),
        None => caps.push(cap),
    }
}

/// Tightens the floor of every input that reads a sketch state, consumers
/// before producers (`order` reversed): a readout's, by the cap of its
/// group ([`readout_floor`]) with the floor it already has; a merge's, the
/// weakest floor of the consumers reading its group (any of them may read
/// the merged state, which has its inputs' $\boldsymbol\theta$).
fn set_floors(
    memo: &mut Memo,
    entries: &[QueryWorkloadEntry],
    caps: &[Vec<ErrorCap>],
    order: &[MemoGroupId],
) {
    for &group in order.iter().rev() {
        for index_in_group in 0..memo.groups()[group.0 as usize].candidates.len() {
            let id = CandidateForMemoGroupId {
                group,
                index_in_group,
            };
            let consumer = memo.candidate(id);
            let operator = &consumer.expression.operator;
            let mut requirements = consumer.input_requirements.clone();
            let mut changed = false;
            for (input, &child) in operator.children().into_iter().enumerate() {
                let Some((family, algorithm)) = sketch_family(expression(memo, child)) else {
                    continue;
                };
                let floor = match operator {
                    Operator::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
                        readout_floor(family, algorithm, query, &caps[group.0 as usize], entries)
                    }
                    Operator::ASAP(ASAPOp::SummaryMerge { .. }) => {
                        weakest_floor_of_consumers(memo, group)
                    }
                    _ => None,
                };
                let Some(floor) = floor else {
                    continue;
                };
                let tightened = match requirements[input].parameter_floor_for_required_accuracy() {
                    Some(existing) => strongest(existing, &floor).unwrap_or(existing.clone()),
                    None => floor,
                };
                requirements[input] = LogicalRequirements::new(Some(tightened));
                changed = true;
            }
            if changed {
                memo.set_input_requirements(id, requirements);
            }
        }
    }
}

/// The sketch family a state group's expression builds.
fn sketch_family(operator: &Operator<MemoGroupId>) -> Option<(&FieldDataType, SketchAlgorithm)> {
    match family(operator)? {
        family @ FieldDataType::Sketch(algorithm, _) => Some((family, *algorithm)),
        _ => None,
    }
}

/// The floor of reading `statistic` from a state of `family` within the
/// largest of `caps` (its group's, per query) that applies: a query's cap
/// applies when its target names no metric or the statistic's; one that
/// does not leaves the input unbounded. `None` without caps.
fn readout_floor(
    family: &FieldDataType,
    algorithm: SketchAlgorithm,
    statistic: &SketchStatistic,
    caps: &[ErrorCap],
    entries: &[QueryWorkloadEntry],
) -> Option<SketchParams> {
    let metric = parameter_list_metric(family, algorithm, statistic);
    let cap = caps
        .iter()
        .map(|cap| {
            let target = entries
                .get(cap.query)
                .and_then(|entry| entry.requirements.accuracy.accuracy_metric);
            match (target, metric) {
                (None, _) => cap.cap,
                (Some(target), Some(metric)) if target == metric => cap.cap,
                _ => f64::INFINITY,
            }
        })
        .reduce(f64::max)?;
    floor_for_cap(family, algorithm, statistic, cap)
}

/// The weakest of the floors the consumers of `group` (the candidates of
/// its parent groups) set on their inputs reading it; `None` when one of
/// them sets none.
fn weakest_floor_of_consumers(memo: &Memo, group: MemoGroupId) -> Option<SketchParams> {
    let mut floors = Vec::new();
    for &parent in memo.groups()[group.0 as usize].parent_groups() {
        for consumer in &memo.groups()[parent.0 as usize].candidates {
            for (input, &child) in consumer
                .expression
                .operator
                .children()
                .into_iter()
                .enumerate()
            {
                if child == group {
                    floors.push(
                        consumer.input_requirements[input]
                            .parameter_floor_for_required_accuracy()?
                            .clone(),
                    );
                }
            }
        }
    }
    weakest_of(floors)
}

/// The metric of reading `statistic` from a state of `family`, by its
/// first listed configuration.
fn parameter_list_metric(
    family: &FieldDataType,
    algorithm: SketchAlgorithm,
    statistic: &SketchStatistic,
) -> Option<AccuracyMetric> {
    let parameters = asap_types::stages::parameter_list(algorithm).first()?;
    Some(
        local_guarantee(family, Some(parameters), statistic)?
            .spec
            .accuracy_metric,
    )
}

/// Each candidate's possible output errors, by group id and index in the
/// group, children first.
fn output_errors(memo: &Memo, order: &[MemoGroupId]) -> Vec<Vec<Vec<OutputError>>> {
    let mut errors: Vec<Vec<Vec<OutputError>>> = vec![Vec::new(); memo.groups().len()];
    for &group in order {
        let mut group_errors = Vec::new();
        for candidate in &memo.groups()[group.0 as usize].candidates {
            group_errors.push(candidate_errors(
                memo,
                &errors,
                &candidate.expression.operator,
            ));
        }
        errors[group.0 as usize] = group_errors;
    }
    errors
}

/// The output errors of one candidate `operator` whose input groups'
/// candidates have `errors` already. A summary state's are its rows'
/// errors (through its accumulator's contract); a readout's are the
/// guarantee of each producer state it may read ([`read_errors`]).
fn candidate_errors(
    memo: &Memo,
    errors: &[Vec<Vec<OutputError>>],
    operator: &Operator<MemoGroupId>,
) -> Vec<OutputError> {
    let children: Vec<MemoGroupId> = operator.children().into_iter().copied().collect();
    match operator {
        Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query,
        }) => read_errors(memo, errors, *summary_input, query),
        Operator::ASAP(ASAPOp::SummaryMerge { .. } | ASAPOp::FinalizeExactAccumulator { .. }) => {
            let mut out = Vec::new();
            for &child in &children {
                for error in errors[child.0 as usize].iter().flatten() {
                    push(&mut out, *error);
                }
            }
            out
        }
        _ => {
            // Per input, the errors the contract admits, as they leave it.
            let mut out = Vec::new();
            let mut all_exact = true;
            for (input, &child) in children.iter().enumerate() {
                let mut admitted = Vec::new();
                for error in errors[child.0 as usize].iter().flatten() {
                    if carries(operator, *error) {
                        push(&mut admitted, *error);
                    }
                }
                if admitted.is_empty() {
                    return Vec::new();
                }
                // Each admitted error as it leaves the contract: a factor 0
                // (a count reads no value) makes it exact.
                let leaving: Vec<OutputError> = admitted
                    .into_iter()
                    .map(|error| match error {
                        OutputError::Bound(metric)
                            if LogicalRequirements::error_propagation(operator, metric)
                                .is_some_and(|p| p.factor_per_input[input] == 0.0) =>
                        {
                            OutputError::Exact
                        }
                        other => other,
                    })
                    .collect();
                all_exact &= leaving.contains(&OutputError::Exact);
                for error in leaving {
                    if error != OutputError::Exact {
                        push(&mut out, error);
                    }
                }
            }
            if all_exact || children.is_empty() {
                push(&mut out, OutputError::Exact);
            }
            out
        }
    }
}

/// The errors of reading `statistic` from state group `state`, over each of
/// its producers: a sketch build's guarantee of its $\boldsymbol\theta$
/// ([`local_guarantee`]); a windowed build's the same when its family's
/// merged state keeps it ([`merged_tumbling_windows_guarantee`]); a
/// merge's those of the states it merges, when the merged state keeps the
/// guarantee; an exact accumulator's its rows' errors. A build over rows
/// none of whose producers it may read gives none.
fn read_errors(
    memo: &Memo,
    errors: &[Vec<Vec<OutputError>>],
    state: MemoGroupId,
    statistic: &SketchStatistic,
) -> Vec<OutputError> {
    let mut out = Vec::new();
    for (index, producer) in memo.groups()[state.0 as usize]
        .candidates
        .iter()
        .enumerate()
    {
        let rows = &errors[state.0 as usize][index];
        if rows.is_empty() {
            continue;
        }
        match &producer.expression.operator {
            Operator::ASAP(ASAPOp::SummaryMerge { children, .. }) => {
                for &child in children {
                    if keeps_guarantee(memo, child, statistic) {
                        for error in read_errors(memo, errors, child, statistic) {
                            push(&mut out, error);
                        }
                    } else {
                        push(&mut out, OutputError::WithoutGuarantee);
                    }
                }
            }
            operator => match family(operator) {
                Some(family @ FieldDataType::Sketch(..)) => push(
                    &mut out,
                    merged_tumbling_windows_guarantee(
                        family,
                        producer.logical_properties.parameters(),
                        producer.logical_properties.window_layout().is_some(),
                        statistic,
                    ),
                ),
                Some(FieldDataType::ExactAggregate(..)) => {
                    for error in rows {
                        push(&mut out, *error);
                    }
                }
                // A sample, wavelet or model state has no error model.
                _ => push(&mut out, OutputError::WithoutGuarantee),
            },
        }
    }
    out
}

/// The error of reading `statistic` from a sketch build of `family` with
/// $\boldsymbol\theta$ `parameters`, built over the whole window or, when
/// `windowed`, merged from its tumbling windows: the guarantee of one
/// build over the window when the family's merged error for the read is
/// exact or as if built over the union (the family-capability table), no
/// guarantee otherwise.
pub(crate) fn merged_tumbling_windows_guarantee(
    family: &FieldDataType,
    parameters: Option<&asap_types::ir::schema::SketchParams>,
    windowed: bool,
    statistic: &SketchStatistic,
) -> OutputError {
    if windowed && !merge_keeps_guarantee(family, statistic) {
        return OutputError::WithoutGuarantee;
    }
    match local_guarantee(family, parameters, statistic) {
        Some(guarantee) if guarantee.is_exact() => OutputError::Exact,
        Some(guarantee) => OutputError::Bound(guarantee.spec.accuracy_metric),
        None => OutputError::WithoutGuarantee,
    }
}

/// Adds `error` to `errors` once.
fn push(errors: &mut Vec<OutputError>, error: OutputError) {
    if !errors.contains(&error) {
        errors.push(error);
    }
}
