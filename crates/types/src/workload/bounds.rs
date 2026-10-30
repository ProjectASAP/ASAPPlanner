//! Bounds read from the data workload: rows, groups and values per result,
//! source rates, and confirmed weights.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use crate::ir::operator::operator_properties::{
    BinaryOpKind, PromQLVectorSetOpKind, RelationalSetOpKind,
};
use crate::ir::operator::{AggIntent, GroupKeys, Reduction, Source};
use crate::ir::physical_export::PhysicalASAPOperatorPayload as Payload;
use crate::ir::schema::{Schema, SketchStatistic, WeightDomain};
use crate::ir::{
    ASAPOp, NonASAPOp, Operator, OperatorNode, OperatorResultKind, QueryRoot, ScalarExpr,
};
use crate::workload::{DataWorkload, MetricType};

/// A scan source's name: its PromQL metric or SQL table.
pub fn source_name(source: &Source) -> &str {
    match source {
        Source::TimeSeries { metric } => metric,
        Source::Table { table_ref } => table_ref,
    }
}

/// An upper bound on the rows one node and the operators below it see or
/// return per evaluation, in terms of the sources they read:
/// `copies_per_source[s]` times the series of `s`, plus `constant_rows`.
#[derive(Debug, Clone, Default, PartialEq)]
struct RowBound {
    copies_per_source: BTreeMap<String, u64>,
    constant_rows: u64,
}

impl RowBound {
    /// The bound of a node that returns at most as many rows as its largest
    /// input, source by source: a fold, a join, a filter.
    fn largest(inputs: &[RowBound]) -> Self {
        let mut bound = RowBound::default();
        for input in inputs {
            for (source, copies) in &input.copies_per_source {
                let entry = bound.copies_per_source.entry(source.clone()).or_default();
                *entry = (*entry).max(*copies);
            }
            bound.constant_rows = bound.constant_rows.max(input.constant_rows);
        }
        bound
    }

    /// The bound of a union, which returns every row of each input.
    fn sum(inputs: &[RowBound]) -> Self {
        let mut bound = RowBound::default();
        for input in inputs {
            for (source, copies) in &input.copies_per_source {
                *bound.copies_per_source.entry(source.clone()).or_default() += copies;
            }
            bound.constant_rows += input.constant_rows;
        }
        bound
    }
}

/// [`RowBound`] of `node`: a scan returns its source's series once; a union
/// (PromQL `or`, SQL `UNION`, `Concat`) the rows of all its branches; a
/// constant vector one row and `VALUES` its rows; any other operator at
/// most as many as its largest input. `cache` holds the shared nodes' bounds.
fn row_bound(
    node: &Rc<OperatorNode>,
    cache: &mut HashMap<*const OperatorNode, RowBound>,
) -> RowBound {
    if let Some(bound) = cache.get(&Rc::as_ptr(node)) {
        return bound.clone();
    }
    let inputs: Vec<RowBound> = node
        .children()
        .into_iter()
        .map(|child| row_bound(child, cache))
        .collect();
    let bound = match node.non_asap() {
        Some(NonASAPOp::Scan { source, .. }) => RowBound {
            copies_per_source: [(source_name(source).to_string(), 1)].into(),
            constant_rows: 0,
        },
        Some(NonASAPOp::Values { rows, .. }) => RowBound {
            constant_rows: rows.len() as u64,
            ..RowBound::largest(&inputs)
        },
        // One row out, but the operators below still read their sources'
        // rows, which the bound keeps; `at_most_one_row` makes `G` of a
        // `vector()` root 1.
        Some(NonASAPOp::PromqlVectorFromScalar(_)) => {
            let mut bound = RowBound::largest(&inputs);
            bound.constant_rows = bound.constant_rows.max(1);
            bound
        }
        Some(
            NonASAPOp::SetOp {
                kind: RelationalSetOpKind::Union,
                ..
            }
            | NonASAPOp::Concat { .. },
        ) => RowBound::sum(&inputs),
        Some(NonASAPOp::BinaryOp { operator, .. })
            if operator.kind == BinaryOpKind::Set(PromQLVectorSetOpKind::Or) =>
        {
            RowBound::sum(&inputs)
        }
        _ => RowBound::largest(&inputs),
    };
    cache.insert(Rc::as_ptr(node), bound.clone());
    bound
}

/// An upper bound on the rows any operator of `root` sees or returns in one
/// evaluation (`row_bound`), from the series count each source it reads
/// declares: the input row count of its folds, and `G` for a PromQL
/// instant vector ([`values_per_result`]). A join or fold of one source
/// counts it once; an `or` counts each branch. `None` when `root` reads no
/// source, or a source it reads declares no series count.
pub fn series_read(root: &QueryRoot, data: &DataWorkload) -> Option<u64> {
    let mut cache = HashMap::new();
    let inputs: Vec<RowBound> = root
        .operator_refs()
        .into_iter()
        .map(|node| row_bound(node, &mut cache))
        .collect();
    let bound = RowBound::largest(&inputs);
    if bound.copies_per_source.is_empty() {
        return None;
    }
    let mut rows = bound.constant_rows;
    for (source, copies) in &bound.copies_per_source {
        rows += copies * data.sources.get(source)?.series_count?;
    }
    Some(rows)
}

/// How many values one evaluation of `root` returns at most (`G`): one for
/// a scalar root and for a root of at most one row (`at_most_one_row`),
/// provable from the query. Otherwise the smaller of two bounds, either
/// alone when the other is unknown: `series_read` ([`series_read`]) for a
/// PromQL instant vector, which bounds the values it can return (each
/// branch of an `or` adds its own), and, for a grouped root of any
/// language, the groups its keys' declared distinct values allow
/// (`declared_group_bound`), at most `series_read`. `None` when neither
/// applies: an ungrouped SQL result, a grouping with an undeclared key, a
/// range vector.
pub fn values_per_result(
    root: &QueryRoot,
    series_read: Option<u64>,
    data: &DataWorkload,
) -> Option<u64> {
    let node = match root {
        QueryRoot::Scalar(_) => return Some(1),
        QueryRoot::Operator(node) if at_most_one_row(node) => return Some(1),
        QueryRoot::Operator(node) => node,
    };
    let by_series = (node.result_kind == OperatorResultKind::InstantVector)
        .then_some(series_read)
        .flatten();
    // Each group holds at least one row read, so the series read also
    // bound the groups.
    let by_groups = declared_group_bound(node, data)
        .map(|groups| series_read.map_or(groups, |series| groups.min(series)));
    by_groups.or(by_series).map(|values| values.max(1))
}

/// An upper bound on the groups `node` returns, from the distinct values
/// the sources it reads declare for its group keys
/// (`SourceFacts::distinct_values_per_label` and
/// `distinct_values_per_label_set`): the smallest product of declared counts
/// covering the keys ([`declared_distinct_combinations`]). The grouping is
/// found below operators that only keep, drop or reorder rows, and through
/// a summary's estimate (not a top-k, which returns several rows per
/// group), merge and finalization. `None` when no such grouping is found,
/// it is a `without (...)`, or a key is not a source column passed through
/// unchanged covered by declared counts: the declared counts are upper
/// bounds, never defaults.
fn declared_group_bound(node: &OperatorNode, data: &DataWorkload) -> Option<u64> {
    let grouped = |reduction: &Reduction, input: &OperatorNode| -> Option<u64> {
        let Reduction::Reduce(keys) = reduction else {
            return None;
        };
        if keys.is_without() {
            return None;
        }
        declared_distinct_combinations(input, keys.keys(), data)
    };
    match &node.operator {
        Operator::NonASAP(NonASAPOp::Aggregate {
            reduction,
            measures,
            child,
            ..
        }) if !measures
            .iter()
            .any(|m| matches!(m, AggIntent::TopK { .. } | AggIntent::CountValues { .. })) =>
        {
            grouped(reduction, child)
        }
        Operator::ASAP(
            ASAPOp::SummaryAgg {
                reduction, child, ..
            }
            | ASAPOp::PaneChain {
                reduction, child, ..
            },
        ) => grouped(reduction, child),
        // A merge's children are states of one definition: its first
        // build's grouping stands for all of them.
        Operator::ASAP(ASAPOp::SummaryMerge { .. }) => match node.summary_build() {
            Some(
                ASAPOp::SummaryAgg {
                    reduction, child, ..
                }
                | ASAPOp::PaneChain {
                    reduction, child, ..
                },
            ) => grouped(reduction, child),
            _ => None,
        },
        Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query,
        }) if !matches!(query, SketchStatistic::TopK { .. }) => {
            declared_group_bound(summary_input, data)
        }
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child })
        | Operator::NonASAP(
            NonASAPOp::Project { child, .. }
            | NonASAPOp::Filter { child, .. }
            | NonASAPOp::Sort { child, .. }
            | NonASAPOp::Limit { child, .. }
            | NonASAPOp::Dedup { child, .. },
        ) => declared_group_bound(child, data),
        _ => None,
    }
}

/// The distinct combinations of values columns `columns` of `node` take
/// together at most, when each is a source column passed through
/// unchanged: traced down through operators that keep their input's
/// columns (filter, sort, limit, time range and shift), a projection that
/// selects them, a grouped aggregate's keys, a per-series reduction's other
/// columns, and each branch of a union (whose combinations add), to the
/// scans, where the source's declared label and label-set counts bound them
/// ([`SourceFacts::declared_distinct_combinations`]). A join is not traced:
/// it can pair values of different rows. `None` when a column is computed
/// or relabeled on the way (its values are no longer the source's), or a
/// source's declared counts do not cover the columns.
///
/// [`SourceFacts::declared_distinct_combinations`]: super::SourceFacts::declared_distinct_combinations
fn declared_distinct_combinations(
    node: &OperatorNode,
    columns: &[usize],
    data: &DataWorkload,
) -> Option<u64> {
    match node.non_asap()? {
        NonASAPOp::Scan { source, .. } => {
            let labels = columns
                .iter()
                .map(|&column| Some(node.schema.fields.get(column)?.name.as_str()))
                .collect::<Option<Vec<_>>>()?;
            data.sources
                .get(source_name(source))?
                .declared_distinct_combinations(&labels)
        }
        NonASAPOp::Filter { child, .. }
        | NonASAPOp::Sort { child, .. }
        | NonASAPOp::Limit { child, .. }
        | NonASAPOp::TimeRange { child, .. }
        | NonASAPOp::TimeShift { child, .. } => {
            declared_distinct_combinations(child, columns, data)
        }
        // A grouped aggregate returns its keys first, unchanged; but a
        // `count_values` label named like a key takes that key's place
        // with the stringified sample values.
        NonASAPOp::Aggregate {
            reduction: Reduction::Reduce(keys),
            measures,
            child,
            ..
        } if !keys.is_without()
            && !measures
                .iter()
                .any(|m| matches!(m, AggIntent::CountValues { .. })) =>
        {
            let inputs = columns
                .iter()
                .map(|&column| keys.keys().get(column).copied())
                .collect::<Option<Vec<_>>>()?;
            declared_distinct_combinations(child, &inputs, data)
        }
        // A per-series reduction keeps every column but the one it reduces.
        NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            measures,
            child,
            ..
        } => {
            let reduced = match measures.first()?.input_cols().first() {
                Some(&input) => input,
                None => crate::ir::scalar::column_resolution::resolve_column_ref(
                    &crate::ir::scalar::ColumnRef::SampleValue,
                    &child.schema,
                )
                .ok()?,
            };
            (!columns.contains(&reduced))
                .then(|| declared_distinct_combinations(child, columns, data))
                .flatten()
        }
        NonASAPOp::Project { cols, child, .. } => {
            let inputs = columns
                .iter()
                .map(|&column| match cols.get(column)?.expr {
                    ScalarExpr::Column(input) => Some(input),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            declared_distinct_combinations(child, &inputs, data)
        }
        NonASAPOp::SetOp {
            kind: RelationalSetOpKind::Union,
            left,
            right,
            ..
        } => Some(
            declared_distinct_combinations(left, columns, data)?
                .saturating_add(declared_distinct_combinations(right, columns, data)?),
        ),
        NonASAPOp::Concat { children, .. } => children.iter().try_fold(0u64, |total, child| {
            Some(total.saturating_add(declared_distinct_combinations(child, columns, data)?))
        }),
        _ => None,
    }
}

/// Whether `node` returns at most one row whatever its input: a global
/// aggregate or summary (a reduction with no group keys), `vector(s)`, and
/// above one only operators that keep or drop its rows, or join two such
/// inputs without adding unmatched rows of both sides.
fn at_most_one_row(node: &OperatorNode) -> bool {
    let global = |reduction: &crate::ir::operator::Reduction| {
        reduction
            .group_keys()
            .is_some_and(|by| !by.is_without() && by.keys().is_empty())
    };
    match &node.operator {
        // A top-k or `count_values` returns several rows per group.
        Operator::NonASAP(NonASAPOp::Aggregate {
            reduction,
            measures,
            ..
        }) => {
            global(reduction)
                && !measures
                    .iter()
                    .any(|m| matches!(m, AggIntent::TopK { .. } | AggIntent::CountValues { .. }))
        }
        Operator::ASAP(ASAPOp::SummaryAgg { reduction, .. }) => global(reduction),
        // A rollup to no keys gives one state.
        Operator::ASAP(ASAPOp::SummaryMerge {
            group_by: Some(keys),
            ..
        }) => keys.is_empty(),
        // A merge gives one state per group, whatever the panes it merges.
        Operator::ASAP(ASAPOp::SummaryMerge { .. }) => matches!(
            node.summary_build(),
            Some(
                ASAPOp::SummaryAgg { reduction, .. } | ASAPOp::PaneChain { reduction, .. }
            ) if global(reduction)
        ),
        Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query,
        }) => !matches!(query, SketchStatistic::TopK { .. }) && at_most_one_row(summary_input),
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) => at_most_one_row(child),
        // `vector(s)` turns one scalar into one row.
        Operator::NonASAP(NonASAPOp::PromqlVectorFromScalar(_)) => true,
        Operator::NonASAP(
            NonASAPOp::Project { child, .. }
            | NonASAPOp::Filter { child, .. }
            | NonASAPOp::Sort { child, .. }
            | NonASAPOp::Limit { child, .. }
            | NonASAPOp::Dedup { child, .. },
        ) => at_most_one_row(child),
        Operator::NonASAP(NonASAPOp::Join {
            kind, left, right, ..
        }) => {
            use crate::ir::operator::operator_properties::JoinKind;
            match kind {
                JoinKind::Semi | JoinKind::Anti => at_most_one_row(left),
                JoinKind::Full => false,
                _ => at_most_one_row(left) && at_most_one_row(right),
            }
        }
        Operator::NonASAP(NonASAPOp::BinaryOp { lhs, rhs, .. }) => {
            at_most_one_row(lhs) && at_most_one_row(rhs)
        }
        _ => false,
    }
}

/// `domain` with an open requirement answered from `data`'s declared source
/// facts (`WeightDomain::confirmed`): samples are proven when the metric is
/// declared a counter or non-negative, a counter derivative only when it is
/// declared a counter; unknown otherwise. No type is inferred from a name.
pub fn confirmed_weights(domain: &WeightDomain, data: &DataWorkload) -> WeightDomain {
    domain.confirmed(
        |metric| declared_counter(data, metric),
        |metric| declared_non_negative(data, metric),
    )
}

/// Whether `data` declares every sample of `metric` non-negative: the
/// metric is declared a counter, or a metric (a gauge, say) with
/// [`SourceFacts::values_non_negative`](crate::workload::SourceFacts::values_non_negative).
pub fn declared_non_negative(data: &DataWorkload, metric: &str) -> bool {
    declared_counter(data, metric)
        || data
            .sources
            .get(metric)
            .is_some_and(|facts| facts.values_non_negative)
}

/// Whether `data` declares `metric` a counter. A declared type does not
/// expire, so its evidence is not checked.
fn declared_counter(data: &DataWorkload, metric: &str) -> bool {
    data.sources
        .get(metric)
        .is_some_and(|facts| facts.metric_type == Some(MetricType::Counter))
}

/// `roots` with every open [`WeightDomain::RawDataSamplesOfMetric`] requirement
/// replaced by its answer from `data` ([`confirmed_weights`]): the form of a
/// selected plan the executor runs, which reads no data workload. The same
/// `Rc` where nothing changes.
pub fn with_confirmed_weights(roots: &[QueryRoot], data: &DataWorkload) -> Vec<QueryRoot> {
    with_confirmed_weights_at(roots, data, &|_| true)
}

/// [`with_confirmed_weights`] at the summaries `confirm_at` names only: the
/// selected plan with the weight proofs Stage 3 made for its candidates
/// applied (`Selection::candidates_with_proven_non_negative_weights`).
pub fn with_confirmed_weights_at(
    roots: &[QueryRoot],
    data: &DataWorkload,
    confirm_at: &dyn Fn(&Rc<OperatorNode>) -> bool,
) -> Vec<QueryRoot> {
    fn confirm(
        node: &Rc<OperatorNode>,
        data: &DataWorkload,
        confirm_at: &dyn Fn(&Rc<OperatorNode>) -> bool,
        cache: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    ) -> Rc<OperatorNode> {
        if let Some(done) = cache.get(&Rc::as_ptr(node)) {
            return Rc::clone(done);
        }
        let mut changed = false;
        let mut operator = node.operator.map_children(|child| {
            let rebuilt = confirm(child, data, confirm_at, cache);
            changed |= !Rc::ptr_eq(&rebuilt, child);
            rebuilt
        });
        if let Operator::ASAP(ASAPOp::SummaryAgg { input, .. } | ASAPOp::PaneChain { input, .. }) =
            &mut operator
        {
            let confirmed = confirmed_weights(&input.weight_domain, data);
            if confirmed != input.weight_domain && confirm_at(node) {
                input.weight_domain = confirmed;
                changed = true;
            }
        }
        let rebuilt = if changed {
            Rc::new(
                OperatorNode::with_schema(operator, node.schema.clone())
                    .with_guarantee(node.guarantee.clone())
                    .with_timing(node.timing)
                    .with_materialization(node.materialization),
            )
        } else {
            Rc::clone(node)
        };
        cache.insert(Rc::as_ptr(node), Rc::clone(&rebuilt));
        rebuilt
    }
    let mut cache = HashMap::new();
    roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(n) => QueryRoot::Operator(confirm(n, data, confirm_at, &mut cache)),
            QueryRoot::Scalar(expr) => QueryRoot::Scalar(
                expr.map_operator_refs(&mut |n| confirm(n, data, confirm_at, &mut cache)),
            ),
        })
        .collect()
}

/// Rows per millisecond the sources in `sources` receive together, each
/// at its own rate: `series_count` series each sampled every
/// `sample_interval`, or, for a source that does not declare both, its
/// share of the workload's `rows_per_second` by series count when every
/// source declares one, else all of it (no source receives more). A node
/// reading no source sees the whole `rows_per_second`. `None` when a
/// source's rate has neither (the arrival declares no rate).
pub fn source_rows_per_ms(sources: &BTreeSet<&str>, data: &DataWorkload) -> Option<f64> {
    let workload_rows_per_ms = data.arrival.rows_per_second().map(|rows| rows / 1_000.0);
    if sources.is_empty() {
        return workload_rows_per_ms;
    }
    let mut every_source: BTreeSet<&str> = data.sources.keys().map(String::as_str).collect();
    every_source.extend(sources.iter().copied());
    let every_source_series = declared_series(&every_source, data);
    // Sources with a rate of their own, and the share of the workload's
    // rate the others receive together (all of it at most).
    let (mut own, mut shared) = (0.0, 0.0);
    for source in sources {
        let facts = data.sources.get(*source);
        match facts.and_then(|facts| facts.series_count.zip(facts.sample_interval)) {
            Some((series, interval)) if interval.0 > 0 => {
                own += series as f64 / interval.0 as f64;
            }
            _ => {
                let workload_rows_per_ms = workload_rows_per_ms?;
                let series = facts.and_then(|facts| facts.series_count);
                shared += match (series, every_source_series) {
                    (Some(series), Some(every)) => {
                        workload_rows_per_ms * series as f64 / every.max(1) as f64
                    }
                    _ => workload_rows_per_ms,
                };
            }
        }
    }
    Some(own + workload_rows_per_ms.map_or(shared, |all| shared.min(all)))
}

/// The names of `node`'s output columns that still hold one source row's
/// label (or column) values unchanged, given each producer's: every column
/// of a scan; through operators that keep their input's rows and columns
/// (filter, sort, limit, dedup, time range and shift), a projection or
/// grouping key that passes a column on under its own name, a per-series
/// reduction's other columns, a union's columns unchanged in every branch,
/// and a summary's grouping columns. Any other operator (a join or binary
/// operator pairing rows, a relabeling, a computed column) gives none, so a
/// declared label set never bounds values it does not describe.
/// `producers` are each producer's output schema and unchanged columns.
pub fn unchanged_source_columns<'a>(
    node: &'a crate::ir::physical_export::PhysicalASAPDAGNode,
    producers: &[(&'a Schema, &BTreeSet<&'a str>)],
) -> BTreeSet<&'a str> {
    let output_names = || node.output_schema.fields.iter().map(|f| f.name.as_str());
    let first = || {
        producers
            .first()
            .map(|&(schema, unchanged)| (unchanged, schema))
    };
    // The output columns passing input column `input[i]` on as itself.
    let passed_on = |inputs: &mut dyn Iterator<Item = Option<usize>>| -> BTreeSet<&'a str> {
        let Some((unchanged, schema)) = first() else {
            return BTreeSet::new();
        };
        output_names()
            .zip(inputs)
            .filter(|(name, input)| {
                input
                    .and_then(|input| schema.fields.get(input))
                    .is_some_and(|field| field.name == *name && unchanged.contains(*name))
            })
            .map(|(name, _)| name)
            .collect()
    };
    let kept = || -> BTreeSet<&'a str> {
        first().map_or_else(BTreeSet::new, |(unchanged, _)| {
            output_names()
                .filter(|name| unchanged.contains(name))
                .collect()
        })
    };
    match &node.payload {
        Payload::NonASAP(operator) => match operator {
            NonASAPOp::Scan { .. } => output_names().collect(),
            NonASAPOp::Filter { .. }
            | NonASAPOp::Sort { .. }
            | NonASAPOp::Limit { .. }
            | NonASAPOp::Dedup { .. }
            | NonASAPOp::TimeRange { .. }
            | NonASAPOp::TimeShift { .. } => kept(),
            NonASAPOp::Project { cols, .. } => {
                passed_on(&mut cols.iter().map(|col| match col.expr {
                    ScalarExpr::Column(input) => Some(input),
                    _ => None,
                }))
            }
            // A `count_values` label named like a key replaces it.
            NonASAPOp::Aggregate {
                reduction: Reduction::Reduce(keys),
                measures,
                ..
            } if !keys.is_without()
                && !measures
                    .iter()
                    .any(|m| matches!(m, AggIntent::CountValues { .. })) =>
            {
                passed_on(&mut keys.keys().iter().map(|&key| Some(key)))
            }
            // A per-series reduction keeps every column but the ones it
            // reduces and the ones it names.
            NonASAPOp::Aggregate {
                reduction: Reduction::PerEntity,
                measures,
                output_names: measure_names,
                ..
            } => {
                let Some((_, schema)) = first() else {
                    return BTreeSet::new();
                };
                let mut columns = kept();
                for measure in measures {
                    for input in measure.input_cols() {
                        if let Some(field) = schema.fields.get(input) {
                            columns.remove(field.name.as_str());
                        }
                    }
                }
                for name in measure_names {
                    columns.remove(name.as_str());
                }
                columns
            }
            NonASAPOp::SetOp {
                kind: RelationalSetOpKind::Union,
                ..
            }
            | NonASAPOp::Concat { .. } => {
                let mut columns = kept();
                for (_, unchanged) in producers {
                    columns.retain(|name| unchanged.contains(name));
                }
                columns
            }
            _ => BTreeSet::new(),
        },
        Payload::ASAP(
            ASAPOp::SummaryAgg { .. }
            | ASAPOp::PaneChain { .. }
            | ASAPOp::SummaryMerge { .. }
            | ASAPOp::SummaryEstimate { .. }
            | ASAPOp::FinalizeExactAccumulator { .. },
        ) => kept(),
        _ => BTreeSet::new(),
    }
}

/// The groups (or partitions) a `by (keys)` over rows of schema `input`
/// from `sources` is priced at, from declared or derivable facts only: the
/// smallest of
/// - when the keys read one source and are all in
///   `unchanged_input_columns` (one source row's labels, unchanged), the
///   smallest product of the counts it declares in
///   `distinct_values_per_label` and `distinct_values_per_label_set` that
///   covers the keys
///   (`SourceFacts::declared_distinct_combinations`);
///   otherwise (several sources, a join pairing labels of different rows,
///   or a relabeled key) the product of each key's
///   `distinct_values_per_label` count summed over the sources (a union
///   holds at most the sum, while a join matching on the key is
///   over-counted), when every key has one;
/// - the series (PromQL) or groups of rows (SQL) the sources declare in
///   `series_count`, when they all do (their product over several
///   sources): each is in one group;
/// - `rows_read`, the rows the grouping reads per evaluation: each row is
///   in at most one group.
pub fn group_count_bound(
    keys: &GroupKeys,
    input: Option<&Schema>,
    sources: &BTreeSet<&str>,
    unchanged_input_columns: &BTreeSet<&str>,
    data: &DataWorkload,
    rows_read: u64,
) -> u64 {
    let labels = || -> Option<Vec<&str>> {
        keys.keys()
            .iter()
            .map(|key| Some(input?.fields.get(*key)?.name.as_str()))
            .collect()
    };
    let label_count = |label: &str| -> Option<u64> {
        sources.iter().try_fold(0u64, |total, source| {
            let count = data
                .sources
                .get(*source)?
                .distinct_values_per_label
                .get(label)?;
            Some(total.saturating_add(*count))
        })
    };
    let declared = || -> Option<u64> {
        if keys.is_without() || sources.is_empty() {
            return None;
        }
        let labels = labels()?;
        // Keys that are one source row's unchanged labels: the source's
        // declared label sets bound their combinations.
        if let (1, Some(source)) = (sources.len(), sources.first()) {
            if labels
                .iter()
                .all(|label| unchanged_input_columns.contains(label))
            {
                return data
                    .sources
                    .get(*source)?
                    .declared_distinct_combinations(&labels);
            }
        }
        labels.into_iter().try_fold(1u64, |product, label| {
            Some(product.saturating_mul(label_count(label)?))
        })
    };
    // Over several sources a group combines one series (or group of rows)
    // of each at most, as a join does; a union's groups are fewer.
    let declared_series = if sources.is_empty() {
        None
    } else {
        sources.iter().try_fold(1u64, |product, source| {
            Some(product.saturating_mul(data.sources.get(*source)?.series_count?))
        })
    };
    [declared(), declared_series, Some(rows_read)]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(rows_read)
        .max(1)
}

/// The series (or groups of rows) the sources in `sources` declare
/// together, when every one declares `series_count`; `None` otherwise and
/// for a node that reads no source.
pub fn declared_series(sources: &BTreeSet<&str>, data: &DataWorkload) -> Option<u64> {
    if sources.is_empty() {
        return None;
    }
    sources.iter().try_fold(0u64, |total, source| {
        Some(total.saturating_add(data.sources.get(*source)?.series_count?))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::{DataArrival, DurationMs, SourceFacts};

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
                        sample_interval: interval.map(DurationMs),
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
}
