//! Compile logical computation to native operators with typed external inputs.
//! Compilation needs no readers; deployment resolves inputs after selection.
//! A node without a native lowering fails compilation; the deployment runtime
//! must execute it.
use crate::operators::{RowWeight, SummaryEvaluation};
use crate::summary_kernels::exact::ExactEvaluation;
use crate::{
    operators::{Expression, Operator, Reduction, SortKey},
    plan::{Boundedness, Emission, NodeId, PhysicalDAG, PhysicalOperator, PlanProperties},
    values::{Batch, SchemaRef},
    Error,
};
use planner_types::ir::operator::{
    agg_has_exact_per_entity_kernel, AggIntent, GroupKeys, Reduction as PlannerReduction,
};
use planner_types::ir::scalar::{ColumnRef, CompareOpKind};
use planner_types::ir::schema::{DataType, FieldDataType};
use planner_types::plan::materialized_view::MaterializedView;
use planner_types::plan::physical_asap_dag::{
    PhysicalASAPDAG, PhysicalASAPDAGNode, PhysicalASAPNodeId,
    PhysicalASAPOperatorPayload as Payload,
};
use planner_types::summary_type::{
    SketchParams, SketchStatistic, SummaryInputExpr, SummaryTypeCapability,
};
// `promql_rows` reads `OperatorNode` through `super::*` only in its
// test-support helpers.
#[cfg_attr(not(any(test, feature = "test-support")), allow(unused_imports))]
use planner_types::ir::{ASAPOp, NonASAPOp, OperatorNode, ScalarExpr, TimeRangeKind};
mod logical;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

// The executor's PromQL row encoding is internal: a deployment runtime reads
// raw data at each `Scan` through its `DataStore` and derives the
// materialized views from the installable plan. The Planner's own tests
// reach it with the `test-support` feature.
#[cfg(feature = "test-support")]
pub mod promql_rows;
#[cfg(not(feature = "test-support"))]
pub(crate) mod promql_rows;
pub mod promql_values;

mod compiled;
pub use compiled::CompiledPhysicalDAG;

mod row_values;

/// Compile computation without opening or retaining deployment readers.
/// Every reachable `Scan` leaf is a raw data slot, read by its `Source`; its
/// predicates must compile over its schema. Each of `views`
/// ([`PhysicalASAPDAG::materialized_views`]) is a materialized view slot,
/// read by its `MaterializedView`, with its node's `output_schema`; nothing
/// below it is compiled.
pub fn compile(
    dag: &PhysicalASAPDAG,
    views: &[MaterializedView],
    roots: &[NodeId],
) -> Result<CompiledPhysicalDAG, Error> {
    compile_internal(dag, views, roots)
}

#[cfg(test)]
thread_local! {
    /// Planner nodes lowered by this thread, for compile-once tests.
    static LOWERED_NODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Helper operators are numbered from their Planner node alone, above the u32
/// Planner ID range, so every boundary choice yields a sub-DAG of the same
/// lowering and candidate cuts need not renumber operators. A node lowering to
/// several helpers takes consecutive indices below its base.
fn helper_id(node: NodeId, index: u64) -> Result<NodeId, Error> {
    if index >= 1 << 16 {
        return Err(invalid(format!("node {node} needs more than 2^16 helpers")));
    }
    Ok(u64::MAX - (node << 16) - index)
}

fn compile_internal(
    dag: &PhysicalASAPDAG,
    views: &[MaterializedView],
    roots: &[NodeId],
) -> Result<CompiledPhysicalDAG, Error> {
    preflight_depth(dag)?;
    // Helper IDs lie above the u32 range; a larger Planner ID could collide.
    if let Some(node) = dag.nodes.iter().find(|n| n.id as u64 > u64::from(u32::MAX)) {
        return Err(invalid(format!("Planner node ID {} exceeds u32", node.id)));
    }
    let restored = logical::restore(dag)?;
    dag.validate().map_err(|e| invalid(e.to_string()))?;
    let nodes = dag
        .nodes
        .iter()
        .map(|node| (node.id as u64, node))
        .collect::<BTreeMap<_, _>>();
    let mut dependencies = BTreeMap::<NodeId, Vec<NodeId>>::new();
    // Binary input order is semantic; serialized edge order is not.
    let mut edges = dag.edges.iter().collect::<Vec<_>>();
    edges.sort_by_key(|edge| {
        (
            edge.consumer,
            match edge.role {
                planner_types::plan::physical_asap_dag::EdgeRole::Left => 0,
                planner_types::plan::physical_asap_dag::EdgeRole::Input => 1,
                planner_types::plan::physical_asap_dag::EdgeRole::Right => 2,
                planner_types::plan::physical_asap_dag::EdgeRole::ScalarRef => 3,
            },
        )
    });
    for edge in edges {
        dependencies
            .entry(edge.consumer as u64)
            .or_default()
            .push(edge.producer as u64);
    }
    let mut views_by_node = BTreeMap::new();
    for view in views {
        let id = view.node_id as NodeId;
        if !nodes.contains_key(&id) {
            return Err(invalid(format!("materialized view {id} is not in the DAG")));
        }
        if views_by_node.insert(id, view).is_some() {
            return Err(invalid(format!("materialized view {id} is given twice")));
        }
    }
    let mut ordered = Vec::new();
    let mut seen = BTreeSet::new();
    let mut pending = roots.iter().map(|&id| (id, false)).collect::<Vec<_>>();
    while let Some((id, expanded)) = pending.pop() {
        if expanded {
            ordered.push(id);
            continue;
        }
        if !seen.insert(id) {
            continue;
        }
        if !nodes.contains_key(&id) {
            return Err(invalid(format!("missing root {id}")));
        }
        pending.push((id, true));
        if !views_by_node.contains_key(&id) {
            for &input in dependencies.get(&id).into_iter().flatten() {
                pending.push((input, false));
            }
        }
    }
    // The consumers of each compiled node, for the windows its reads select.
    let mut consumers = BTreeMap::<NodeId, Vec<NodeId>>::new();
    for (&consumer, producers) in &dependencies {
        if seen.contains(&consumer) && !views_by_node.contains_key(&consumer) {
            for &producer in producers {
                consumers.entry(producer).or_default().push(consumer);
            }
        }
    }
    let compile_roots: BTreeSet<NodeId> = roots.iter().copied().collect();
    let mut physical_dag = CompiledPhysicalDAG::new(roots.to_vec());
    for id in ordered {
        let node = nodes[&id];
        let auxiliary = helper_id(id, 0)?;
        let output = Arc::new(node.output_schema.clone());
        crate::values::validate_schema(&output)?;
        if let Some(view) = views_by_node.get(&id) {
            // A chain's rows are read through the `TimeRange`s above it,
            // which select its windows; any other view's state is read for
            // the window of data it summarizes. Either is exact, so bounded.
            let read_window_ms = match node.payload {
                Payload::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }) => {
                    window_read_above(id, &compile_roots, &consumers, &nodes, &dependencies)
                }
                _ => window_below(id, &nodes, &dependencies),
            }
            .ok_or_else(|| invalid(format!("materialized view {id} is read unbounded")))?;
            physical_dag.add_materialized_view(
                id,
                compiled::MaterializedViewSlot {
                    view: (*view).clone(),
                    schema: output,
                    read_window_ms,
                },
            )?;
        } else if let Payload::NonASAP(NonASAPOp::Scan {
            source,
            predicates,
            schema,
        }) = &node.payload
        {
            // A raw data slot reads the payload's schema; its predicates
            // compile over it.
            if *schema != node.output_schema {
                return Err(invalid(format!(
                    "scan {id}: schema differs from its output schema"
                )));
            }
            let predicates = predicates
                .iter()
                .map(|predicate| {
                    local_scalar(&predicate.0).and_then(|predicate| {
                        crate::sources::compile_scan_predicate(&predicate, &output)
                    })
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| invalid(format!("scan {id}: {error}")))?;
            // The rows its consumers read; a compile root reads them all.
            let read_window_ms = output.time_index.and_then(|_| {
                window_read_above(id, &compile_roots, &consumers, &nodes, &dependencies)
            });
            physical_dag.add_raw_data(
                id,
                compiled::RawDataSlot {
                    source: source.clone(),
                    schema: output,
                    predicates,
                    read_window_ms,
                },
            )?;
        } else {
            #[cfg(test)]
            LOWERED_NODES.with(|count| count.set(count.get() + 1));
            let mut inputs = dependencies.get(&id).cloned().unwrap_or_default();
            let mut schemas = inputs
                .iter()
                .map(|id| Arc::new(nodes[id].output_schema.clone()))
                .collect::<Vec<_>>();
            if matches!(node.payload, Payload::ASAP(ASAPOp::SummaryMerge { .. }))
                && inputs.len() > 1
            {
                if schemas.iter().any(|s| s != &schemas[0]) {
                    return Err(invalid("summary merge inputs have different schemas"));
                }
                physical_dag.add(
                    auxiliary,
                    inputs,
                    Operator::union(schemas[0].clone(), schemas.len())?,
                )?;
                inputs = vec![auxiliary];
                schemas.truncate(1);
            }
            if let Payload::ASAP(ASAPOp::MaintainPopulation { population, .. }) = &node.payload {
                use planner_types::ir::operator::maintained_population::PopulationInput;
                let PopulationInput::CurrentSeries(spec) = &population.input else {
                    return Err(invalid(
                        "native maintained population requires a current-series input",
                    ));
                };
                let [input] = schemas.as_slice() else {
                    return Err(invalid("current-series population requires one input"));
                };
                if spec.without {
                    return Err(invalid(
                        "dynamic without grouping requires label-set projection",
                    ));
                }
                let identity = named_column(
                    input,
                    &ColumnRef::Named(promql_rows::SERIES_IDENTITY_COLUMN.into()),
                )?;
                let coordinate = input
                    .time_index
                    .ok_or_else(|| invalid("current-series input lacks timestamp"))?;
                let value = named_column(input, &ColumnRef::SampleValue)?;
                let lookback = i64::try_from(spec.lookback_ms)
                    .map_err(|_| invalid("current-series lookback overflows"))?;
                physical_dag.add(
                    id,
                    inputs,
                    Operator::current_series(input.clone(), identity, coordinate, value, lookback)?
                        .with_output_schema(output)?,
                )?;
                continue;
            }
            if let Payload::ASAP(ASAPOp::EvaluatePopulation { evaluation, .. }) = &node.payload {
                use planner_types::ir::operator::maintained_population::{
                    PopulationInput, PopulationStatistic,
                };
                let [producer] = inputs.as_slice() else {
                    return Err(invalid("population evaluation requires one input"));
                };
                let Payload::ASAP(ASAPOp::MaintainPopulation { population, .. }) =
                    &nodes[producer].payload
                else {
                    return Err(invalid(
                        "population evaluation requires its declared population",
                    ));
                };
                let PopulationInput::CurrentSeries(spec) = &population.input else {
                    return Err(invalid("current-series population required"));
                };
                if spec.without {
                    return Err(invalid(
                        "dynamic without ranking requires label-set projection",
                    ));
                }
                let input = schemas[0].clone();
                let PopulationStatistic::TopK { k } = evaluation else {
                    let mut chain =
                        row_values::population_aggregate(&input, &spec.grouping, evaluation)?;
                    let last = chain.pop().expect("nonempty chain");
                    let mut inputs = inputs;
                    for (index, operator) in chain.into_iter().enumerate() {
                        let helper = helper_id(id, index as u64)?;
                        physical_dag.add(helper, inputs, operator)?;
                        inputs = vec![helper];
                    }
                    physical_dag.add(id, inputs, last.with_output_schema(output)?)?;
                    continue;
                };
                let groups = spec
                    .grouping
                    .iter()
                    .map(|name| named_column(&input, &ColumnRef::Named(name.clone())))
                    .collect::<Result<Vec<_>, _>>()?;
                let value = named_column(&input, &ColumnRef::SampleValue)?;
                physical_dag.add(
                    auxiliary,
                    inputs,
                    Operator::sort(
                        input.clone(),
                        vec![SortKey {
                            column: value,
                            descending: true,
                            nulls_first: false,
                        }],
                        groups.clone(),
                    )?,
                )?;
                physical_dag.add(
                    id,
                    vec![auxiliary],
                    Operator::limit(input, *k as u64, 0, groups)?.with_output_schema(output)?,
                )?;
                continue;
            }
            // An exact per-series range reduction over raw data samples
            // (`quantile/sum/avg/min/max/count/distinct/l2/entropy_over_time`)
            // reduces each series' raw data samples in the window natively. Stage 3
            // selects no other (`agg_has_exact_per_entity_kernel`).
            if let Payload::NonASAP(NonASAPOp::Aggregate {
                reduction: PlannerReduction::PerEntity,
                measures,
                filters,
                having: None,
                ..
            }) = &node.payload
            {
                let [measure] = measures.as_slice() else {
                    return Err(invalid("per-entity aggregate has one measure"));
                };
                if filters.iter().any(Option::is_some) {
                    return Err(invalid("filtered aggregate has no native implementation"));
                }
                let [input] = schemas.as_slice() else {
                    return Err(invalid("per-entity aggregate requires one input"));
                };
                let value = match measure.input_cols().first() {
                    Some(column) => *column,
                    None => named_column(input, &ColumnRef::SampleValue)?,
                };
                if !agg_has_exact_per_entity_kernel(measure) {
                    return Err(invalid(format!(
                        "node {id}: per-entity aggregate {measure:?} has no native implementation"
                    )));
                }
                let reduction = match measure {
                    AggIntent::Count => Reduction::Count,
                    AggIntent::Sum { .. } => Reduction::Sum(value),
                    AggIntent::Avg { .. } => Reduction::Avg(value),
                    AggIntent::Min { .. } => Reduction::Min(value),
                    AggIntent::Max { .. } => Reduction::Max(value),
                    AggIntent::Quantile { q, .. } => Reduction::Quantile {
                        column: value,
                        q: *q,
                    },
                    AggIntent::Cardinality { .. } => Reduction::Cardinality(vec![value]),
                    AggIntent::FrequencyL2 { .. } => Reduction::FrequencyL2(value),
                    AggIntent::FrequencyEntropy { .. } => Reduction::FrequencyEntropy(value),
                    _ => {
                        return Err(invalid(format!(
                            "node {id}: per-entity kernel of {measure:?} is listed but not mapped"
                        )))
                    }
                };
                let coordinate = input
                    .time_index
                    .ok_or_else(|| invalid("temporal input lacks time"))?;
                let groups = (0..input.fields.len())
                    .filter(|&column| column != value && column != coordinate)
                    .collect();
                let aggregate =
                    Operator::aggregate(input.clone(), groups, vec![("value".into(), reduction)])?;
                // A PromQL sample is Float64; a count is Int64.
                let reduced = aggregate.schema();
                let project = Operator::project(
                    reduced.clone(),
                    reduced
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(i, field)| {
                            let expression = if field.dtype == FieldDataType::Plain(DataType::Int64)
                                && i + 1 == reduced.fields.len()
                            {
                                Expression::ExactFloat64(i)
                            } else {
                                Expression::Column(i)
                            };
                            (field.name.clone(), expression)
                        })
                        .collect(),
                )?;
                let projected = project.schema();
                physical_dag.add(auxiliary, inputs, aggregate)?;
                physical_dag.add(helper_id(id, 1)?, vec![auxiliary], project)?;
                let stamped = Operator::scope_timestamp(projected, output.clone())?;
                if output
                    .fields
                    .iter()
                    .any(|field| field.name == promql_rows::SERIES_IDENTITY_COLUMN)
                {
                    // Range functions drop the metric name, as their
                    // summary evaluations do.
                    physical_dag.add(helper_id(id, 2)?, vec![helper_id(id, 1)?], stamped)?;
                    physical_dag.add(
                        id,
                        vec![helper_id(id, 2)?],
                        Operator::series_without_name(output)?,
                    )?;
                } else {
                    physical_dag.add(id, vec![helper_id(id, 1)?], stamped)?;
                }
                continue;
            }
            // A chain builds, in an ingestion run, the window of its input
            // ending at the run's end: one row per key, stamped with that
            // `window_end`. Older windows are earlier runs', in the store.
            if let Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
                summary_type,
                input: update,
                reduction,
                filter,
                tumbling_window_width_ms,
                ..
            }) = &node.payload
            {
                if node.output_state.timing
                    != planner_types::ir::properties::ExecutionTiming::IngestionTime
                {
                    return Err(invalid(format!(
                        "node {id}: a tumbling-window chain is built in ingestion runs"
                    )));
                }
                crate::capability::validate_ingestion_time_summary(filter.as_ref())
                    .map_err(Error::Invalid)?;
                let [input] = schemas.as_slice() else {
                    return Err(invalid("a tumbling-window chain reads one input"));
                };
                let time = input
                    .time_index
                    .ok_or_else(|| invalid("a tumbling-window chain reads a time column"))?;
                let width_ms = i64::try_from(*tumbling_window_width_ms)
                    .map_err(|_| invalid("tumbling-window width exceeds Int64"))?;
                // A time series' stale markers are not samples.
                let below =
                    node_below_shifts(first_input(id, &dependencies), &nodes, &dependencies);
                let sample_value = match below.map(|below| &nodes[&below].payload) {
                    Some(Payload::NonASAP(NonASAPOp::Scan {
                        source: planner_types::ir::operator::Source::TimeSeries { .. },
                        ..
                    })) => Some(named_column(input, &ColumnRef::SampleValue)?),
                    _ => None,
                };
                let window =
                    Operator::time_range(input.clone(), time, 0, width_ms, sample_value, None)?;
                let build = match reduction {
                    PlannerReduction::PerEntity => {
                        let SummaryInputExpr::Column(value) = &update.weight else {
                            return Err(invalid(
                                "per-entity update requires a projected value column",
                            ));
                        };
                        let value = named_column(input, value)?;
                        let groups = (0..input.fields.len())
                            .filter(|&column| column != value && column != time)
                            .collect();
                        Operator::per_group_summary_build(
                            input.clone(),
                            summary_type.clone(),
                            node.parameters.clone(),
                            value,
                            Some(time),
                            groups,
                        )?
                    }
                    _ => summary_agg_build(
                        input,
                        summary_type,
                        node.parameters.as_ref(),
                        update,
                        reduction,
                    )?,
                };
                let build = match filter {
                    Some(filter) => build.with_row_filter(expression(&filter.0, input)?)?,
                    None => build,
                };
                let compact = build.schema();
                physical_dag.add(auxiliary, inputs, window)?;
                physical_dag.add(helper_id(id, 1)?, vec![auxiliary], build)?;
                physical_dag.add(
                    id,
                    vec![helper_id(id, 1)?],
                    Operator::scope_timestamp(compact, output)?,
                )?;
                continue;
            }
            // A `TimeShift` with an `@` anchor evaluates its input once, at
            // the anchor's time minus its offset.
            if let Payload::NonASAP(NonASAPOp::TimeShift {
                shift:
                    planner_types::ir::operator::TimeShift {
                        offset_ms,
                        at: Some(at),
                    },
                ..
            }) = &node.payload
            {
                let [input] = schemas.as_slice() else {
                    return Err(invalid("a time shift reads one input"));
                };
                physical_dag.add(
                    id,
                    inputs,
                    Operator::evaluate_at(input.clone(), *at, *offset_ms)?
                        .with_output_schema(output)?,
                )?;
                continue;
            }
            // A closed row must include either all source labels or the explicit
            // complete-label identity. Projected labels alone are insufficient.
            if let Payload::ASAP(ASAPOp::SummaryAgg {
                summary_type: family,
                input: update,
                reduction: PlannerReduction::PerEntity,
                filter: None,
                ..
            }) = &node.payload
            {
                let [input_id] = inputs.as_slice() else {
                    return Err(invalid("per-entity summary requires one input"));
                };
                let Some(NonASAPOp::TimeRange { child, .. }) = restored[input_id].non_asap() else {
                    return Err(invalid(
                        "per-entity summary requires a resolved raw time range",
                    ));
                };
                // A tumbling window reads its window through `TimeShift`s; the
                // deployment supplies the shifted raw rows.
                let source = below_time_shifts(child, |node| node.non_asap(), Ok)?;
                let Some(NonASAPOp::Scan { schema, .. }) = source.non_asap() else {
                    return Err(invalid("per-entity summary requires a resolved source"));
                };
                // An unkeyed frequency family (UnivMon) counts each value of
                // its one item column once, as a value-frequency build.
                let frequency_item = crate::capability::unit_frequency_column(update)
                    .filter(|_| unkeyed_frequency_family(family));
                if !schema.closed || (update.item.is_some() && frequency_item.is_none()) {
                    return Err(invalid(
                        "per-entity summary requires complete source identity",
                    ));
                }
                crate::capability::validate_summary_kernel(
                    family,
                    node.parameters.as_ref(),
                    update,
                )
                .map_err(Error::Invalid)?;
                let value = match (frequency_item, &update.weight) {
                    (Some(item), _) => item,
                    (None, SummaryInputExpr::Column(value)) => value,
                    (None, _) => {
                        return Err(invalid(
                            "per-entity update requires a projected value column",
                        ))
                    }
                };
                let input = schemas[0].clone();
                let value = named_column(&input, value)?;
                let coordinate = input
                    .time_index
                    .ok_or_else(|| invalid("temporal input lacks time"))?;
                let groups = (0..input.fields.len())
                    .filter(|&column| column != value && column != coordinate)
                    .collect();
                let build = Operator::per_group_summary_build(
                    input,
                    family.clone(),
                    node.parameters.clone(),
                    value,
                    Some(coordinate),
                    groups,
                )?;
                let compact = build.schema();
                physical_dag.add(auxiliary, inputs, build)?;
                physical_dag.add(
                    id,
                    vec![auxiliary],
                    Operator::scope_timestamp(compact, output)?,
                )?;
                continue;
            }
            if let Payload::NonASAP(NonASAPOp::BinaryOp {
                operator,
                return_bool,
                ..
            }) = &node.payload
            {
                let operator = crate::expressions::binary::BinaryOperator::from_logical(
                    operator,
                    *return_bool,
                );
                let query_time = node.output_state.timing
                    == planner_types::ir::properties::ExecutionTiming::QueryTime;
                let label_map = |schema: &SchemaRef| {
                    schema
                        .fields
                        .iter()
                        .any(|f| matches!(f.dtype, FieldDataType::Plain(DataType::Map { .. })))
                };
                // Grouped rows carry their labels as columns; per-series rows
                // carry the series identity.
                if let (true, [left, right]) = (query_time, schemas.as_slice()) {
                    if !label_map(left) && !label_map(right) {
                        let binary = Operator::series_binary(
                            left.clone(),
                            right.clone(),
                            operator.clone(),
                            [false, false],
                        )
                        .map_err(|error| invalid(format!("node {id}: {error}")))?;
                        physical_dag.add(id, inputs, binary.with_output_schema(output)?)?;
                        continue;
                    }
                }
            }
            if let Payload::ASAP(ASAPOp::FinalizeExactAccumulator { .. }) = &node.payload {
                // Exact counts evaluate to Int64; PromQL declares a Float64 sample.
                let evaluation = bind_operation(node, &schemas)
                    .map_err(|error| invalid(format!("node {id}: {error}")))?;
                let actual = evaluation.schema();
                let converted = actual.fields.iter().zip(&output.fields).position(|(a, d)| {
                    a.dtype == FieldDataType::Plain(DataType::Int64)
                        && d.dtype == FieldDataType::Plain(DataType::Float64)
                });
                if let Some(column) = converted {
                    let columns = actual
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(i, field)| {
                            (
                                field.name.clone(),
                                if i == column {
                                    Expression::ExactFloat64(i)
                                } else {
                                    Expression::Column(i)
                                },
                            )
                        })
                        .collect();
                    let project =
                        Operator::project(actual, columns)?.with_output_schema(output.clone())?;
                    physical_dag.add(auxiliary, inputs, evaluation)?;
                    if temporal_evaluation_drops_name(node) {
                        physical_dag.add(auxiliary - 1, vec![auxiliary], project)?;
                        physical_dag.add(
                            id,
                            vec![auxiliary - 1],
                            Operator::series_without_name(output)?,
                        )?;
                    } else {
                        physical_dag.add(id, vec![auxiliary], project)?;
                    }
                    continue;
                }
            }
            if let Payload::ASAP(ASAPOp::SummaryMerge { .. }) = &node.payload {
                let input = schemas
                    .first()
                    .ok_or_else(|| invalid(format!("node {id}: summary merge requires inputs")))?;
                require_sound_merge(input).map_err(|e| invalid(format!("node {id}: {e}")))?;
                if output.time_index.is_some() {
                    // Tumbling window timestamps describe their own builds. The merged
                    // state covers this evaluation's window, so merge by
                    // series identity and attach the run scope's timestamp,
                    // as a per-series `SummaryAgg` does.
                    let merged = bind_operation(node, &schemas)?;
                    let compact = merged.schema();
                    physical_dag.add(helper_id(id, 1)?, inputs, merged)?;
                    physical_dag.add(
                        id,
                        vec![helper_id(id, 1)?],
                        Operator::scope_timestamp(compact, output)?,
                    )?;
                    continue;
                }
            }
            // A rollup of a relation's states to no keys is a global SQL
            // aggregate: one row, an empty state, over no input rows.
            if let Payload::ASAP(ASAPOp::SummaryMerge {
                group_by: Some(keys),
                ..
            }) = &node.payload
            {
                if keys.is_empty() && built_from_relation(&restored[&id]) {
                    // The merge carries the θ of the states it reads.
                    let parameters = node.parameters.clone();
                    let operator = compile_node(node, &schemas)
                        .and_then(|merge| merge.with_emit_empty_global(parameters))
                        .map_err(|error| invalid(format!("node {id}: {error}")))?;
                    physical_dag.add(id, inputs, operator)?;
                    continue;
                }
            }
            // A window over a table or a time series (and a tumbling window's
            // shift under it) runs natively on the time column.
            if let Some(operator) = time_window_operator(id, &nodes, &dependencies, &schemas)
                .map_err(|error| invalid(format!("node {id}: {error}")))?
            {
                physical_dag.add(id, inputs, operator.with_output_schema(output)?)?;
                continue;
            }
            let mut operator = compile_node(node, &schemas)
                .map_err(|error| invalid(format!("node {id}: {error}")))?;
            if operator.is_counter_evaluation() {
                if let Some((offset, lookback)) = counter_window(id, &nodes, &dependencies)? {
                    operator = operator.with_counter_window(offset, lookback)?;
                }
            }
            if temporal_evaluation_drops_name(node) {
                physical_dag.add(auxiliary, inputs, operator)?;
                physical_dag.add(id, vec![auxiliary], Operator::series_without_name(output)?)?;
            } else {
                physical_dag.add(id, inputs, operator)?;
            }
        }
    }
    physical_dag.validate()?;
    Ok(physical_dag)
}

/// Whether every summary state `state` merges was built from a relation's
/// rows (a SQL table's), where a global aggregate has a row over no input
/// row; not from a PromQL vector's, where it has none.
fn built_from_relation(state: &std::rc::Rc<OperatorNode>) -> bool {
    match state.asap() {
        Some(ASAPOp::SummaryMerge { children, .. }) => children.iter().all(built_from_relation),
        Some(
            ASAPOp::SummaryAgg { child, .. } | ASAPOp::SummaryTumblingWindowChain { child, .. },
        ) => child.result_kind == planner_types::ir::OperatorResultKind::Relation,
        // A filter keeping whole states reads them from their build.
        _ => planner_types::ir::operator::asap::selected_from(state)
            .is_some_and(|states| built_from_relation(&states)),
    }
}

/// The native operator of `id` when it is a `TimeRange` or a `TimeShift`
/// without an `@` anchor over a `Scan` of a SQL table or a time series, or
/// over a tumbling-window chain (below any stacked `TimeShift`s); `None`
/// for any other node.
///
/// A `TimeRange(w)` over shifts summing to `s` keeps the rows whose time
/// column lies in `(T - s - w, T - s]` at evaluation time `T`
/// ([`time_range_window`]), the window a PromQL selector and a SQL filter
/// against `now()` read; an ingestion run keeps the window ending at its
/// `window_end_ms` ([`Operator::time_range`]). Over a time series a stale
/// marker is not a row, and an instant window keeps each series' (each
/// label set's) latest row. Over a chain of width `c`, it keeps the windows
/// whose `window_end` lies there, `w / c` of them: `c` must divide `w` and
/// `s`, and the window must be a range. A `TimeShift` only moves the window
/// its `TimeRange` reads (the range above applies its offset), so it passes
/// the rows through. An instant window over a table is refused.
fn time_window_operator(
    id: NodeId,
    nodes: &BTreeMap<NodeId, &PhysicalASAPDAGNode>,
    dependencies: &BTreeMap<NodeId, Vec<NodeId>>,
    input_schemas: &[SchemaRef],
) -> Result<Option<Operator>, Error> {
    let below = match &nodes[&id].payload {
        Payload::NonASAP(NonASAPOp::TimeRange { .. }) => first_input(id, dependencies),
        Payload::NonASAP(NonASAPOp::TimeShift { .. }) => Some(id),
        _ => return Ok(None),
    };
    let Some(below) = node_below_shifts(below, nodes, dependencies) else {
        return Ok(None);
    };
    let chain_width_ms = match &nodes[&below].payload {
        Payload::NonASAP(NonASAPOp::Scan { .. }) => None,
        Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            tumbling_window_width_ms,
            ..
        }) => Some(
            i64::try_from(*tumbling_window_width_ms)
                .map_err(|_| invalid("tumbling-window width exceeds Int64"))?,
        ),
        _ => return Ok(None),
    };
    let [input] = input_schemas else {
        return Err(invalid("a time range or shift reads one input"));
    };
    let Payload::NonASAP(NonASAPOp::TimeRange { kind, .. }) = &nodes[&id].payload else {
        // A shift passes its rows through.
        let columns = input
            .fields
            .iter()
            .enumerate()
            .map(|(i, field)| (field.name.clone(), Expression::Column(i)))
            .collect();
        return Operator::project(input.clone(), columns).map(Some);
    };
    let Some((end_offset_ms, width_ms)) = time_range_window(id, nodes, dependencies) else {
        return Ok(None);
    };
    let time = input
        .time_index
        .ok_or_else(|| invalid("a time range needs its input's time column"))?;
    let (sample_value, latest_per_series) = match (&nodes[&below].payload, kind) {
        (_, TimeRangeKind::Instant) if chain_width_ms.is_some() => {
            return Err(invalid("an instant window over a tumbling-window chain"))
        }
        (Payload::ASAP(_), TimeRangeKind::Range) => {
            let chain_width_ms = chain_width_ms.expect("a chain");
            if width_ms % chain_width_ms != 0 || end_offset_ms % chain_width_ms != 0 {
                return Err(invalid(format!(
                    "a window ({end_offset_ms} ms back, {width_ms} ms wide) is not whole \
                     tumbling windows of {chain_width_ms} ms"
                )));
            }
            (None, None)
        }
        (
            Payload::NonASAP(NonASAPOp::Scan {
                source: planner_types::ir::operator::Source::Table { .. },
                ..
            }),
            TimeRangeKind::Range,
        ) => (None, None),
        (
            Payload::NonASAP(NonASAPOp::Scan {
                source: planner_types::ir::operator::Source::Table { .. },
                ..
            }),
            TimeRangeKind::Instant,
        ) => return Err(invalid("an instant window over a SQL table is unsupported")),
        (_, kind) => {
            let value = named_column(input, &ColumnRef::SampleValue)?;
            // A series is its label set: every column but its time and value.
            let series = (*kind == TimeRangeKind::Instant).then(|| {
                (0..input.fields.len())
                    .filter(|&column| column != value && column != time)
                    .collect()
            });
            (Some(value), series)
        }
    };
    Operator::time_range(
        input.clone(),
        time,
        end_offset_ms,
        width_ms,
        sample_value,
        latest_per_series,
    )
    .map(Some)
}

/// `(s, w)` of the `TimeRange(w)` `id` over `TimeShift`s summing to `s`: it
/// reads `(T - s - w, T - s]` at evaluation time `T`. `None` when `id` is not
/// a `TimeRange` or a shift below it has an `@` anchor.
fn time_range_window(
    id: NodeId,
    nodes: &BTreeMap<NodeId, &PhysicalASAPDAGNode>,
    dependencies: &BTreeMap<NodeId, Vec<NodeId>>,
) -> Option<(i64, i64)> {
    let Payload::NonASAP(NonASAPOp::TimeRange { range, .. }) = &nodes.get(&id)?.payload else {
        return None;
    };
    let mut end_offset_ms = 0i64;
    let mut below = first_input(id, dependencies);
    while let Some(Payload::NonASAP(NonASAPOp::TimeShift { shift, .. })) = below
        .and_then(|below| nodes.get(&below))
        .map(|node| &node.payload)
    {
        if shift.at.is_some() {
            return None;
        }
        end_offset_ms = end_offset_ms.saturating_add(shift.offset_ms);
        below = below.and_then(|below| first_input(below, dependencies));
    }
    Some((end_offset_ms, i64::try_from(range.as_millis()).ok()?))
}

/// The smallest window `(end_offset_ms, width_ms)` holding both.
fn window_union(a: (i64, i64), b: (i64, i64)) -> (i64, i64) {
    let end_offset_ms = a.0.min(b.0);
    let start_back_ms = a.0.saturating_add(a.1).max(b.0.saturating_add(b.1));
    (end_offset_ms, start_back_ms.saturating_sub(end_offset_ms))
}

/// The window `(end_offset_ms, width_ms)` back from the evaluation time that
/// the compiled consumers of `id` read through the `TimeShift`s (without an
/// `@` anchor) above it: a `TimeRange(w)`, or a chain of width `w` (its
/// window ending at the evaluation time), over shifts summing to `s` reads
/// `(s, w)`; several read the smallest window holding all of theirs.
/// `None` when `id` is a compile root, a root or another consumer reads it
/// outside a window, or nothing reads it.
fn window_read_above(
    id: NodeId,
    roots: &BTreeSet<NodeId>,
    consumers: &BTreeMap<NodeId, Vec<NodeId>>,
    nodes: &BTreeMap<NodeId, &PhysicalASAPDAGNode>,
    dependencies: &BTreeMap<NodeId, Vec<NodeId>>,
) -> Option<(i64, i64)> {
    let mut window: Option<(i64, i64)> = None;
    let mut pending = vec![(id, 0i64)];
    while let Some((node, end_offset_ms)) = pending.pop() {
        if roots.contains(&node) {
            return None;
        }
        for &consumer in consumers.get(&node).into_iter().flatten() {
            let read = match &nodes[&consumer].payload {
                Payload::NonASAP(NonASAPOp::TimeShift { shift, .. }) if shift.at.is_none() => {
                    pending.push((consumer, end_offset_ms.saturating_add(shift.offset_ms)));
                    continue;
                }
                Payload::NonASAP(NonASAPOp::TimeRange { .. }) => {
                    time_range_window(consumer, nodes, dependencies)?
                }
                Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
                    tumbling_window_width_ms,
                    ..
                }) => (
                    end_offset_ms,
                    i64::try_from(*tumbling_window_width_ms).ok()?,
                ),
                _ => return None,
            };
            window = Some(window.map_or(read, |window| window_union(window, read)));
        }
    }
    window
}

/// The window `(end_offset_ms, width_ms)` back from the evaluation time of
/// the data `id`'s state summarizes: the smallest window holding every
/// `TimeRange` below it. `None` when none is below it, or one is read
/// through an `@` anchor.
fn window_below(
    id: NodeId,
    nodes: &BTreeMap<NodeId, &PhysicalASAPDAGNode>,
    dependencies: &BTreeMap<NodeId, Vec<NodeId>>,
) -> Option<(i64, i64)> {
    let mut seen = BTreeSet::new();
    let mut pending = vec![id];
    let mut window: Option<(i64, i64)> = None;
    while let Some(node) = pending.pop() {
        if !seen.insert(node) {
            continue;
        }
        if let Payload::NonASAP(NonASAPOp::TimeRange { .. }) = &nodes[&node].payload {
            let read = time_range_window(node, nodes, dependencies)?;
            window = Some(window.map_or(read, |window| window_union(window, read)));
        } else {
            pending.extend(dependencies.get(&node).into_iter().flatten().copied());
        }
    }
    window
}

fn first_input(id: NodeId, dependencies: &BTreeMap<NodeId, Vec<NodeId>>) -> Option<NodeId> {
    dependencies.get(&id).and_then(|d| d.first()).copied()
}

/// The first node at or below `start` that is not a `TimeShift`; `None`
/// when `start` is `None`, a shift has an `@` anchor, or a shift lacks its
/// child.
fn node_below_shifts(
    start: Option<NodeId>,
    nodes: &BTreeMap<NodeId, &PhysicalASAPDAGNode>,
    dependencies: &BTreeMap<NodeId, Vec<NodeId>>,
) -> Option<NodeId> {
    let mut below = start;
    while let Some(node) = below {
        let Payload::NonASAP(NonASAPOp::TimeShift { shift, .. }) = &nodes[&node].payload else {
            return Some(node);
        };
        if shift.at.is_some() {
            return None;
        }
        below = first_input(node, dependencies);
    }
    None
}

/// The first node at or below `node` that is not a `TimeShift`, following
/// each shift's child: a range reads its raw source through any number of
/// stacked shifts. `operator` gives a node's non-ASAP operator (`None` for an
/// ASAP one) and `resolve` a child's node; an error when a child is missing.
pub(crate) fn below_time_shifts<'a, N, C: 'a>(
    mut node: &'a N,
    operator: impl Fn(&'a N) -> Option<&'a NonASAPOp<C>>,
    resolve: impl Fn(&'a C) -> Result<&'a N, Error>,
) -> Result<&'a N, Error> {
    while let Some(NonASAPOp::TimeShift { child, .. }) = operator(node) {
        node = resolve(child)?;
    }
    Ok(node)
}

/// The logical window of the counter evaluation `id`, as `(offset,
/// lookback)`: it ends `offset` ms before the evaluation time and spans
/// `lookback` ms. It is the union of the range windows the evaluation
/// reads, each moved back by the time shifts below its range: one range for
/// a single build or for the windows of a chain it merges. `None`
/// without a range; an error when the ranges do not form one window.
fn counter_window(
    id: NodeId,
    nodes: &BTreeMap<NodeId, &PhysicalASAPDAGNode>,
    dependencies: &BTreeMap<NodeId, Vec<NodeId>>,
) -> Result<Option<(i64, i64)>, Error> {
    let millis =
        |ms: u128| i64::try_from(ms).map_err(|_| invalid("counter lookback exceeds Int64"));
    let mut pending = vec![id];
    let mut visited = BTreeSet::new();
    // (offset, end of the window back from the evaluation time) per range.
    let mut windows = BTreeSet::new();
    while let Some(ancestor) = pending.pop() {
        if !visited.insert(ancestor) {
            continue;
        }
        if let Payload::NonASAP(NonASAPOp::TimeRange { range, .. }) = &nodes[&ancestor].payload {
            let mut offset = 0i64;
            let mut below = dependencies.get(&ancestor).and_then(|d| d.first()).copied();
            while let Some(input) = below {
                let Payload::NonASAP(NonASAPOp::TimeShift { shift, .. }) = &nodes[&input].payload
                else {
                    break;
                };
                if shift.at.is_none() {
                    offset = offset
                        .checked_add(shift.offset_ms)
                        .ok_or_else(|| invalid("counter window overflows Int64"))?;
                }
                below = dependencies.get(&input).and_then(|d| d.first()).copied();
            }
            let back = offset
                .checked_add(millis(range.as_millis())?)
                .ok_or_else(|| invalid("counter window overflows Int64"))?;
            windows.insert((offset, back));
            continue;
        }
        pending.extend(dependencies.get(&ancestor).into_iter().flatten().copied());
    }
    let mut windows = windows.into_iter();
    let Some((offset, mut back)) = windows.next() else {
        return Ok(None);
    };
    // Back-to-back windows, newest first, form one window.
    for (start, end) in windows {
        if start != back {
            return Err(invalid("counter evaluation has ambiguous logical windows"));
        }
        back = end;
    }
    Ok(Some((offset, back - offset)))
}

// Temporal summary evaluations produce PromQL vectors, whose range functions drop
// the metric name before matching/filtering. Stored state retains its full identity.
fn temporal_evaluation_drops_name(node: &PhysicalASAPDAGNode) -> bool {
    node.output_schema
        .fields
        .iter()
        .any(|field| field.name == promql_rows::SERIES_IDENTITY_COLUMN)
        && matches!(
            &node.payload,
            Payload::ASAP(ASAPOp::FinalizeExactAccumulator { .. })
                | Payload::ASAP(ASAPOp::SummaryEstimate {
                    query: SketchStatistic::Quantile { .. }
                        | SketchStatistic::Cardinality
                        | SketchStatistic::PointCount { .. }
                        | SketchStatistic::FrequencyL2
                        | SketchStatistic::FrequencyEntropy
                        // A heap ranking series identities over raw data samples
                        // answers a `sum_over_time` or `count_over_time` per
                        // ranked series.
                        | SketchStatistic::TopK { .. },
                    ..
                })
        )
}

/// The IR accepts a `SummaryMerge` over any state family; the executor runs
/// one only when the family-capability table gives the family a sound merge.
pub(crate) fn require_sound_merge(input: &SchemaRef) -> Result<(), Error> {
    let state = &input.fields[summary_column(input)?].dtype;
    if SummaryTypeCapability::of(state).merge_inputs.is_none() {
        return Err(invalid(format!(
            "summary merge over {state:?} is unsupported: the family has no sound merge"
        )));
    }
    Ok(())
}

/// Bind a Planner node against the schemas supplied by its deployment edges.
/// This is the same checked path used by complete DAG binding.
pub fn compile_node(node: &PhysicalASAPDAGNode, inputs: &[SchemaRef]) -> Result<Operator, Error> {
    for schema in inputs {
        crate::values::validate_schema(schema)?;
    }
    bind_operation(node, inputs)?.with_output_schema(Arc::new(node.output_schema.clone()))
}

fn bind_operation(node: &PhysicalASAPDAGNode, inputs: &[SchemaRef]) -> Result<Operator, Error> {
    if let Payload::NonASAP(NonASAPOp::BinaryOp {
        operator,
        return_bool,
        ..
    }) = &node.payload
    {
        let operator =
            crate::expressions::binary::BinaryOperator::from_logical(operator, *return_bool);
        let [left, right] = inputs else {
            return Err(invalid("binary requires two inputs"));
        };
        if node.output_state.timing == planner_types::ir::properties::ExecutionTiming::IngestionTime
        {
            let value = |schema: &SchemaRef| -> Result<usize, Error> {
                let columns = schema
                    .fields
                    .iter()
                    .enumerate()
                    .filter(|(_, field)| {
                        field.dtype
                            == FieldDataType::Plain(planner_types::ir::schema::DataType::Float64)
                    })
                    .map(|(i, _)| i)
                    .collect::<Vec<_>>();
                match columns.as_slice() {
                    [value] => Ok(*value),
                    _ => Err(invalid("aligned binary requires one value column")),
                }
            };
            let (l, r) = (value(left)?, value(right)?);
            let keys = left
                .fields
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != l)
                .map(|(i, field)| {
                    right
                        .fields
                        .iter()
                        .position(|other| other.name == field.name && other.dtype == field.dtype)
                        .map(|j| (i, j))
                        .ok_or_else(|| invalid("aligned input identities differ"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            return Operator::aligned_binary(
                left.clone(),
                right.clone(),
                keys,
                (l, r),
                operator.clone(),
            );
        }
        return Operator::vector_binary(left.clone(), right.clone(), operator.clone(), false);
    }
    if let Payload::NonASAP(NonASAPOp::Join {
        kind: join_kind,
        pred,
        ..
    }) = &node.payload
    {
        let [left, right] = inputs else {
            return Err(invalid("join requires two inputs"));
        };
        let pred = planner_types::ir::Predicate(local_scalar(&pred.0)?);
        if *join_kind == planner_types::ir::operator::JoinKind::Semi {
            if let Ok(keys) = equijoin_keys(&pred, left, right) {
                return Operator::semi_join(left.clone(), right.clone(), keys);
            }
        }
        return Operator::relational_join(
            left.clone(),
            right.clone(),
            join_kind.clone(),
            &pred,
            Arc::new(node.output_schema.clone()),
        );
    }
    if let Payload::NonASAP(NonASAPOp::Values { rows, schema, .. }) = &node.payload {
        if !inputs.is_empty() {
            return Err(invalid("Values takes no relational inputs"));
        }
        let empty = Arc::new(planner_types::ir::schema::Schema::default());
        let rows = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|expr| expression(expr, &empty)?.evaluate(&[]))
                    .collect::<Result<Vec<_>, Error>>()
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let schema = Arc::new(schema.clone());
        return Operator::source(
            schema.clone(),
            vec![crate::values::Batch::try_new(schema, rows)?],
        );
    }
    let [input] = inputs else {
        return Err(invalid(
            "native Planner binding currently requires a unary operation or an explicit source",
        ));
    };
    match &node.payload {
        Payload::ASAP(ASAPOp::FinalizeExactAccumulator { .. }) => {
            let state = summary_column(input)?;
            use planner_types::summary_type::ExactKind as E;
            let statistic = match &input.fields[state].dtype {
                FieldDataType::ExactAggregate(kind, _) => match kind {
                    E::Sum | E::Count | E::Min | E::Max | E::Rate | E::Increase => *kind,
                    _ => return Err(invalid("exact family evaluation is unsupported")),
                },
                _ => return Err(invalid("exact finalization requires exact state")),
            };
            Operator::evaluation(
                input.clone(),
                state,
                SummaryEvaluation::Exact(ExactEvaluation {
                    statistic,
                    lookback_ms: None,
                    offset_ms: 0,
                }),
            )
        }

        Payload::NonASAP(operator) => match operator {
            NonASAPOp::Project { cols, .. } => Operator::project(
                input.clone(),
                cols.iter()
                    .enumerate()
                    .map(|(i, col)| {
                        Ok((
                            node.output_schema
                                .fields
                                .get(i)
                                .ok_or_else(|| invalid("projection width mismatch"))?
                                .name
                                .clone(),
                            match &col.expr {
                                ScalarExpr::Column(index) => Expression::Column(*index),
                                expr => expression(expr, input)?,
                            },
                        ))
                    })
                    .collect::<Result<_, Error>>()?,
            ),
            // A filter keeping whole summary states reads only their keys.
            NonASAPOp::Filter { pred, .. } if !input.is_all_plain() => Operator::filter(
                input.clone(),
                Expression::planner(crate::expressions::CompiledExpression::compile_over_states(
                    &local_scalar(&pred.0)?,
                    input,
                )?),
            ),
            NonASAPOp::Filter { pred, .. } => {
                Operator::filter(input.clone(), expression(&pred.0, input)?)
            }
            NonASAPOp::Sort {
                keys, partition_by, ..
            } => Operator::sort(
                input.clone(),
                keys.iter()
                    .map(|key| {
                        let ScalarExpr::Column(column) = key.expr else {
                            return Err(invalid(
                                "sort expression must be projected before sorting",
                            ));
                        };
                        Ok(SortKey {
                            column,
                            descending: !key.ascending,
                            nulls_first: key.nulls_first,
                        })
                    })
                    .collect::<Result<_, Error>>()?,
                groups(input, partition_by)?,
            ),
            NonASAPOp::Limit {
                n,
                offset,
                partition_by,
                ..
            } => Operator::limit(
                input.clone(),
                n.unwrap_or(usize::MAX) as u64,
                *offset as u64,
                groups(input, partition_by)?,
            ),
            NonASAPOp::Aggregate {
                reduction,
                measures,
                output_names,
                filters,
                having: None,
                ..
            } => {
                if measures.len() != output_names.len() {
                    return Err(invalid("aggregate output names differ from measures"));
                }
                // PromQL `absent(v)`: one reduction of every series to at
                // most one synthesized sample.
                if let [AggIntent::Absent { labels }] = measures.as_slice() {
                    if !matches!(reduction, PlannerReduction::Reduce(keys) if *keys == GroupKeys::none())
                        || filters.iter().any(Option::is_some)
                    {
                        return Err(invalid("absent reduces by no key, unfiltered"));
                    }
                    return Operator::absent(
                        input.clone(),
                        labels.clone(),
                        Arc::new(node.output_schema.clone()),
                    );
                }
                let PlannerReduction::Reduce(keys) = reduction else {
                    return Err(invalid(
                        "per-entity aggregate requires an explicit entity binding",
                    ));
                };
                // PromQL `histogram_quantile` over classic buckets: one
                // histogram per label set but `le`, interpolated as
                // Prometheus does. The frontend groups `without (le)`.
                if let [AggIntent::HistogramQuantile { q, le }] = measures.as_slice() {
                    if !(keys.is_without()
                        && keys.keys() == [*le]
                        && filters.iter().all(Option::is_none))
                    {
                        return Err(invalid(
                            "histogram_quantile requires grouping without([le]) and no measure filter",
                        ));
                    }
                    return Operator::series_histogram_quantile(input.clone(), *q, *le);
                }
                let measures = measures
                    .iter()
                    .zip(output_names)
                    .enumerate()
                    .map(|(index, (m, name))| {
                        let column = |col: Option<usize>| {
                            col.map(Ok)
                                .unwrap_or_else(|| named_column(input, &ColumnRef::SampleValue))
                        };
                        let m = match m {
                            AggIntent::Count => Reduction::Count,
                            AggIntent::Cardinality { cols, .. } => {
                                Reduction::Cardinality(if cols.is_empty() {
                                    vec![column(None)?]
                                } else {
                                    cols.clone()
                                })
                            }
                            AggIntent::FrequencyL2 { col, .. } => {
                                Reduction::FrequencyL2(column(*col)?)
                            }
                            AggIntent::FrequencyEntropy { col, .. } => {
                                Reduction::FrequencyEntropy(column(*col)?)
                            }
                            AggIntent::Sum { col } => Reduction::Sum(column(*col)?),
                            AggIntent::Avg { col } => Reduction::Avg(column(*col)?),
                            AggIntent::Min { col } => Reduction::Min(column(*col)?),
                            AggIntent::Max { col } => Reduction::Max(column(*col)?),
                            // PromQL `quantile by (G) (φ, v)` exactly: the
                            // group's values, interpolated as Prometheus does
                            // (NaN first). A filtered one has no kernel: the IR
                            // declares it NULL for a group with no matching
                            // row, where Prometheus gives NaN.
                            AggIntent::Quantile { col, q, .. }
                                if input.has_promql_series_identity()
                                    && filters.get(index).is_none_or(Option::is_none) =>
                            {
                                Reduction::Quantile {
                                    column: column(*col)?,
                                    q: *q,
                                }
                            }
                            // A SQL percentile (`approx_percentile_cont`)
                            // exactly: `percentile_cont`, NULL for a group
                            // with no (matching) value.
                            AggIntent::Quantile { col, q, .. }
                                if !input.has_promql_series_identity() =>
                            {
                                Reduction::Percentile {
                                    column: column(*col)?,
                                    q: *q,
                                }
                            }
                            _ => {
                                return Err(invalid(
                                    "aggregate intent has no native implementation",
                                ))
                            }
                        };
                        Ok((name.clone(), m))
                    })
                    .collect::<Result<_, Error>>()?;
                Operator::aggregate(input.clone(), groups(input, keys)?, measures)?
                    .with_measure_filters(
                        filters
                            .iter()
                            .map(|f| f.as_ref().map(|f| expression(&f.0, input)).transpose())
                            .collect::<Result<_, Error>>()?,
                    )
            }
            _ => Err(invalid("value operation has no native implementation")),
        },
        Payload::ASAP(ASAPOp::SummaryAgg {
            summary_type: family,
            input: update,
            reduction,
            filter,
            ..
        }) => {
            if node.output_state.timing
                == planner_types::ir::properties::ExecutionTiming::IngestionTime
            {
                crate::capability::validate_ingestion_time_summary(filter.as_ref())
                    .map_err(Error::Invalid)?;
            }
            let build =
                summary_agg_build(input, family, node.parameters.as_ref(), update, reduction)?;
            match filter {
                Some(filter) => build.with_row_filter(expression(&filter.0, input)?),
                None => Ok(build),
            }
        }
        Payload::ASAP(ASAPOp::SummaryMerge { group_by, .. }) => {
            let state = summary_column(input)?;
            let groups = match group_by {
                // A rollup: each output group merges the input states whose
                // keys match. Counter states join time-ordered windows of
                // one series, never different series, and a Hydra's groups
                // share one state.
                Some(keys) => {
                    let family = &input.fields[state].dtype;
                    if !SummaryTypeCapability::merges_groups(family) {
                        return Err(invalid(format!(
                            "a rollup of {family:?} states: the summary type cannot merge different groups"
                        )));
                    }
                    keys.clone()
                }
                None => (0..input.fields.len())
                    .filter(|&i| i != state && Some(i) != input.time_index)
                    .collect(),
            };
            Operator::summary_merge(input.clone(), state, groups)
        }
        Payload::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
            if let SketchStatistic::TopK { k } = query {
                return Operator::keyed_evaluation(
                    input.clone(),
                    summary_column(input)?,
                    *k,
                    Arc::new(node.output_schema.clone()),
                );
            }
            let state = summary_column(input)?;
            let evaluation = Operator::evaluation(
                input.clone(),
                state,
                SummaryEvaluation::Sketch(query.clone()),
            )?;
            // A distinct count is Int64 where the Planner types it so (SQL
            // `COUNT(DISTINCT)`), Float64 as a PromQL sample value. The
            // evaluation's value replaces the state column in place, so the
            // declared value is at the state's index.
            let declared_whole = node.output_schema.fields.get(state).map(|f| &f.dtype)
                == Some(&FieldDataType::Plain(DataType::Int64));
            match query {
                SketchStatistic::Cardinality if declared_whole => {
                    evaluation.with_whole_distinct_count()
                }
                _ => Ok(evaluation),
            }
        }
        _ => Err(invalid(
            "physical operation has no native binding; the deployment runtime must execute it",
        )),
    }
}
/// The build for a `SummaryAgg` before its row filter: all groups sharing
/// one state (Hydra), a keyed state per group, or a state per group over
/// one value column.
fn summary_agg_build(
    input: &SchemaRef,
    family: &FieldDataType,
    parameters: Option<&SketchParams>,
    update: &planner_types::summary_type::SummaryUpdate,
    reduction: &PlannerReduction,
) -> Result<Operator, Error> {
    if matches!(
        family,
        FieldDataType::Sketch(planner_types::summary_type::SketchAlgorithm::HydraCms)
    ) {
        crate::capability::validate_summary_kernel(family, parameters, update)
            .map_err(Error::Invalid)?;
        let PlannerReduction::Reduce(keys) = reduction else {
            return Err(invalid("an all-groups summary requires explicit groups"));
        };
        let Some(SummaryInputExpr::Column(item)) = &update.item else {
            unreachable!("validated HydraCms item column")
        };
        // Validated: the unit count, a positive integer constant or a
        // non-negative weight column.
        return Operator::all_groups_summary_build(
            input.clone(),
            family.clone(),
            parameters.cloned(),
            named_column(input, item)?,
            row_weight(input, &update.weight)?,
            groups(input, keys)?,
        );
    }
    // An unkeyed frequency family (UnivMon) counts each value of its
    // one item column once: a value-frequency build over that column.
    if let (Some(item), true) = (
        crate::capability::unit_frequency_column(update),
        unkeyed_frequency_family(family),
    ) {
        crate::capability::validate_summary_kernel(family, parameters, update)
            .map_err(Error::Invalid)?;
        let PlannerReduction::Reduce(keys) = reduction else {
            return Err(invalid(
                "summary construction requires explicit grouping columns",
            ));
        };
        return Operator::per_group_summary_build(
            input.clone(),
            family.clone(),
            parameters.cloned(),
            named_column(input, item)?,
            input.time_index,
            groups(input, keys)?,
        );
    }
    if let Some(item) = &update.item {
        let PlannerReduction::Reduce(keys) = reduction else {
            return Err(invalid("keyed summary requires explicit partitions"));
        };
        fn columns(
            expr: &SummaryInputExpr,
            input: &SchemaRef,
            result: &mut Vec<usize>,
        ) -> Result<(), Error> {
            match expr {
                SummaryInputExpr::Column(column) => result.push(named_column(input, column)?),
                SummaryInputExpr::Tuple(items) => {
                    for item in items {
                        columns(item, input, result)?;
                    }
                }
                _ => return Err(invalid("keyed summary needs explicit item columns")),
            }
            Ok(())
        }
        let mut items = Vec::new();
        columns(item, input, &mut items)?;
        return Operator::per_group_keyed_summary_build(
            input.clone(),
            family.clone(),
            parameters.cloned(),
            items,
            row_weight(input, &update.weight)?,
            groups(input, keys)?,
        );
    }
    crate::capability::validate_summary_kernel(family, parameters, update)
        .map_err(Error::Invalid)?;
    let SummaryInputExpr::Column(column) = &update.weight else {
        return Err(invalid(
            "summary update expression must be projected to a column",
        ));
    };
    let PlannerReduction::Reduce(keys) = reduction else {
        return Err(invalid(
            "summary construction requires explicit grouping columns",
        ));
    };
    Operator::per_group_summary_build(
        input.clone(),
        family.clone(),
        parameters.cloned(),
        named_column(input, column)?,
        input.time_index,
        groups(input, keys)?,
    )
}
/// What each row adds in a keyed build: a projected weight column, or a
/// positive integer constant (1 for a row count) read without a column.
fn row_weight(input: &SchemaRef, weight: &SummaryInputExpr) -> Result<RowWeight, Error> {
    match weight {
        SummaryInputExpr::Column(weight) => Ok(RowWeight::Column(named_column(input, weight)?)),
        SummaryInputExpr::Constant(weight) => Ok(RowWeight::Constant(
            crate::capability::constant_row_weight(*weight).map_err(Error::Invalid)?,
        )),
        _ => Err(invalid(
            "keyed summary weight must be a projected column or a constant",
        )),
    }
}
/// Families whose state counts values of one column without keyed items:
/// their unit-frequency update builds over that column.
fn unkeyed_frequency_family(family: &FieldDataType) -> bool {
    matches!(family, FieldDataType::Sketch(kind) if matches!(
        kind,
        planner_types::summary_type::SketchAlgorithm::UnivMon
            | planner_types::summary_type::SketchAlgorithm::Hll
    ))
}
fn summary_column(input: &SchemaRef) -> Result<usize, Error> {
    let columns = input
        .fields
        .iter()
        .enumerate()
        .filter(|(_, f)| !matches!(f.dtype, FieldDataType::Plain(_)))
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    match columns.as_slice() {
        [column] => Ok(*column),
        _ => Err(invalid("one summary state column required")),
    }
}
fn named_column(input: &SchemaRef, column: &ColumnRef) -> Result<usize, Error> {
    let name = match column {
        // This summary-update lookup matches column names without qualifiers.
        // Reject ambiguous names rather than guessing a join side.
        ColumnRef::Named(name) | ColumnRef::Qualified { name, .. } => name.as_str(),
        ColumnRef::SampleValue => "value",
        _ => {
            return Err(invalid(
                "summary update requires an unambiguous bound column",
            ))
        }
    };
    let matches = input
        .fields
        .iter()
        .enumerate()
        .filter(|(_, field)| field.name == name)
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [column] => Ok(*column),
        _ => Err(invalid("summary update column missing or ambiguous")),
    }
}
fn groups(input: &SchemaRef, groups: &GroupKeys) -> Result<Vec<usize>, Error> {
    if groups.is_without() {
        return Err(invalid("grouping without requires resolved label columns"));
    }
    if groups.keys().iter().any(|&i| i >= input.fields.len()) {
        return Err(invalid("grouping column out of range"));
    }
    Ok(groups.keys().to_vec())
}
fn expression(
    expr: &ScalarExpr<PhysicalASAPNodeId>,
    input: &SchemaRef,
) -> Result<Expression, Error> {
    let expr = local_scalar(expr)?;
    Ok(Expression::planner(
        crate::expressions::CompiledExpression::compile(&expr, input)?,
    ))
}

// Bound recursion before invoking the upstream recursive provenance validator.
fn preflight_depth(dag: &PhysicalASAPDAG) -> Result<(), Error> {
    let mut remaining = dag
        .nodes
        .iter()
        .map(|node| (node.id, 0usize))
        .collect::<BTreeMap<_, _>>();
    if remaining.len() != dag.nodes.len() {
        return Err(invalid("duplicate Planner node"));
    }
    let mut consumers = BTreeMap::<_, Vec<_>>::new();
    for edge in &dag.edges {
        if !remaining.contains_key(&edge.producer) {
            return Err(invalid("missing Planner edge producer"));
        }
        *remaining
            .get_mut(&edge.consumer)
            .ok_or_else(|| invalid("missing Planner edge consumer"))? += 1;
        consumers
            .entry(edge.producer)
            .or_default()
            .push(edge.consumer);
    }
    let mut ready = remaining
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(id, _)| *id)
        .collect::<std::collections::VecDeque<_>>();
    let mut depths = BTreeMap::new();
    let mut visited = 0;
    while let Some(id) = ready.pop_front() {
        visited += 1;
        let depth = *depths.get(&id).unwrap_or(&1usize);
        if depth > 128 {
            return Err(invalid("DAG exceeds the supported execution depth of 128"));
        }
        for &consumer in consumers.get(&id).into_iter().flatten() {
            let next = depths.entry(consumer).or_insert(1);
            *next = (*next).max(depth + 1);
            let count = remaining.get_mut(&consumer).expect("validated endpoint");
            *count -= 1;
            if *count == 0 {
                ready.push_back(consumer);
            }
        }
    }
    if visited != dag.nodes.len() {
        return Err(invalid("Planner DAG contains a cycle"));
    }
    Ok(())
}

/// Join predicates address the concatenated left/right schema.
fn semi_join_keys(
    expr: &ScalarExpr,
    left: usize,
    right: usize,
    keys: &mut Vec<(usize, usize)>,
) -> Result<(), Error> {
    match expr {
        ScalarExpr::BoolAnd(parts) => {
            for part in parts {
                semi_join_keys(part, left, right, keys)?;
            }
        }
        ScalarExpr::Compare {
            left: a,
            op: CompareOpKind::Eq,
            right: b,
            ..
        } => {
            let (ScalarExpr::Column(a), ScalarExpr::Column(b)) = (a.as_ref(), b.as_ref()) else {
                return Err(invalid("semi-join requires column equality keys"));
            };
            let (a, b) = if a < b { (*a, *b) } else { (*b, *a) };
            if a >= left || b < left || b >= left + right {
                return Err(invalid("semi-join key must match left to right"));
            }
            keys.push((a, b - left));
        }
        _ => return Err(invalid("unsupported semi-join predicate")),
    }
    Ok(())
}

/// Resolve equality keys against the Planner join's concatenated input schema.
/// Deployments may use these positions to bind their source columns.
pub fn equijoin_keys(
    pred: &planner_types::ir::Predicate,
    left: &planner_types::ir::schema::Schema,
    right: &planner_types::ir::schema::Schema,
) -> Result<Vec<(usize, usize)>, Error> {
    let mut keys = Vec::new();
    semi_join_keys(&pred.0, left.fields.len(), right.fields.len(), &mut keys)?;
    if keys.is_empty() {
        return Err(invalid("semi-join requires explicit matching keys"));
    }
    Ok(keys)
}

fn local_scalar(expr: &ScalarExpr<PhysicalASAPNodeId>) -> Result<ScalarExpr, Error> {
    if !expr.operator_refs().is_empty() {
        return Err(invalid(
            "scalar plan reads require explicit execution bindings",
        ));
    }
    Ok(expr.map_operator_refs(&mut |_| unreachable!("no operator references")))
}

#[cfg(test)]
mod compilation_tests {
    use super::*;

    fn empty_dag() -> PhysicalASAPDAG {
        PhysicalASAPDAG {
            nodes: vec![],
            edges: vec![],
            roots: vec![],
            scalar_roots: vec![],
        }
    }

    /// An empty DAG (no roots) is invalid.
    #[test]
    fn an_empty_dag_is_invalid() {
        assert!(matches!(
            compile(&empty_dag(), &[], &[]),
            Err(Error::Invalid(_))
        ));
    }
}

/// Helpers for the Planner's own tests (`test-support`).
#[cfg(feature = "test-support")]
pub mod test_support {
    use super::*;
    use planner_types::stages::{Lifetime, StorageMedium};
    use std::collections::HashMap;

    /// Set θ on every sketch build among `nodes` (`SummaryAgg`,
    /// `SummaryTumblingWindowChain` of a sketch family): `parameters` when
    /// given, else the first θ its algorithm lists (`parameter_list`).
    /// Exact builds keep `None`. For tests of hand-built plans, whose nodes
    /// carry no θ: pass `&mut dag.nodes`, or `[&mut node]`.
    pub fn set_build_parameters<'a>(
        nodes: impl IntoIterator<Item = &'a mut PhysicalASAPDAGNode>,
        parameters: Option<&SketchParams>,
    ) {
        for node in nodes {
            if let Payload::ASAP(
                ASAPOp::SummaryAgg {
                    summary_type: FieldDataType::Sketch(algorithm),
                    ..
                }
                | ASAPOp::SummaryTumblingWindowChain {
                    summary_type: FieldDataType::Sketch(algorithm),
                    ..
                },
            ) = &node.payload
            {
                node.parameters = parameters.cloned().or_else(|| {
                    planner_types::summary_type::parameter_list(*algorithm)
                        .first()
                        .cloned()
                });
            }
        }
    }

    /// The materialized views at `nodes`, in that order: each node that is
    /// not a view is marked materialized at ingestion time, and it and
    /// every node below it run at ingestion time (their outputs and
    /// outgoing edges); every view without a retention or coverage is
    /// retained for ever with a coverage of its own. For tests that bind
    /// states at those nodes, as a deployment binds its views.
    pub fn views_at(
        dag: &mut PhysicalASAPDAG,
        nodes: &[NodeId],
    ) -> Result<Vec<MaterializedView>, Error> {
        use planner_types::ir::properties::ExecutionTiming;
        let mut producers: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for edge in &dag.edges {
            producers
                .entry(edge.consumer as NodeId)
                .or_default()
                .push(edge.producer as NodeId);
        }
        let marked: Vec<NodeId> = dag
            .nodes
            .iter()
            .filter(|node| {
                nodes.contains(&(node.id as NodeId))
                    && !matches!(
                        node.lifetime,
                        planner_types::stages::Lifetime::AcrossEvaluations(_)
                    )
            })
            .map(|node| node.id as NodeId)
            .collect();
        let mut at_ingestion = BTreeSet::new();
        let mut pending = marked.clone();
        while let Some(id) = pending.pop() {
            if at_ingestion.insert(id) {
                pending.extend(producers.get(&id).into_iter().flatten());
            }
        }
        for node in &mut dag.nodes {
            let id = node.id as NodeId;
            if marked.contains(&id) {
                node.lifetime = Lifetime::AcrossEvaluations(StorageMedium::Memory);
            }
            if matches!(node.lifetime, Lifetime::AcrossEvaluations(_)) {
                node.retention_ms.get_or_insert(i64::MAX as u64);
                node.view_coverage.get_or_insert_with(|| {
                    planner_types::plan::physical_asap_dag::ViewCoverage::new(
                        format!("node {id}"),
                        vec![],
                    )
                });
            }
            if at_ingestion.contains(&id) {
                node.output_state.timing = ExecutionTiming::IngestionTime;
            }
        }
        for edge in &mut dag.edges {
            if at_ingestion.contains(&(edge.producer as NodeId)) {
                edge.data_state_mut().timing = ExecutionTiming::IngestionTime;
            }
        }
        let views = dag
            .materialized_views()
            .map_err(|error| invalid(error.to_string()))?;
        nodes
            .iter()
            .map(|&id| {
                views
                    .iter()
                    .find(|view| view.node_id as NodeId == id)
                    .cloned()
                    .ok_or_else(|| invalid(format!("node {id} is not a materialized view")))
            })
            .collect()
    }
}
