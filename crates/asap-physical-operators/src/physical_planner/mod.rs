//! Compile logical computation to native operators with typed external inputs.
//! Compilation needs no readers; deployment resolves inputs after selection.
use crate::operators::ReadoutQuery;
use crate::summary_kernels::exact::ExactReadout;
use crate::{
    operators::{Expression, Operator, Reduction, SortKey},
    plan::{Boundedness, Emission, NodeId, PhysicalExecution, PhysicalOperator, PlanProperties},
    values::{Batch, Schema},
    Error,
};
use planner_types::{
    post_asap::{
        ExactOperation, PostASAPDAGTransport, PostAsapDagNode, PostAsapOperatorPayload as Payload,
        SketchQuery, SummaryFamilyType, SummaryInputExpr, ValueOperation,
    },
    pre_asap::{
        AggIntent, ColumnRef, CompareOpKind, DataType, GroupKeys, PreASAPNode,
        Reduction as PlannerReduction,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

/// Source nodes cut the DAG at an installed storage/ingestion frontier. The
/// binding must have exactly the declared schema and no upstream dependencies.
/// A deployment must authorize these frontiers before calling this function.
pub type Source<'a> = Box<dyn PhysicalOperator<Batch, Schema> + 'a>;

pub mod precompute;
pub mod promql_fallback;
pub mod promql_rows;
pub mod promql_values;

mod candidates;
pub use candidates::{
    compile_candidate, compile_candidates, compile_physical_dag_candidates, cut_candidate,
    enumerate_frontiers, frontier_from_timing, select_candidate, CandidateCost,
    CandidatePhysicalDAGs, CandidateSelection, PhysicalCandidate, PhysicalCandidateError,
};

mod compiled;
pub use compiled::{InputContract, PhysicalDAG};

mod row_values;

/// Compile computation without opening or retaining deployment readers.
/// Input contracts identify explicit boundaries selected by maintenance planning.
/// The view is a transport document's ([`PostASAPDAGTransport::as_view`]), a
/// shared index's, or a lifecycle assignment's timing over that index.
pub fn compile(
    dag: planner_types::post_asap::PostASAPDAGView<'_>,
    inputs: BTreeMap<NodeId, InputContract>,
    roots: &[NodeId],
) -> Result<PhysicalDAG, Error> {
    compile_internal(&dag, inputs, roots)
}

/// Convenience for callers that already resolved inputs. Lowering still uses
/// only their contracts, and instantiation checks those contracts again.
pub fn bind<'a>(
    dag: &PostASAPDAGTransport,
    sources: BTreeMap<NodeId, Source<'a>>,
    roots: &[NodeId],
) -> Result<PhysicalExecution<'a, Batch, Schema>, Error> {
    let inputs = sources
        .iter()
        .map(|(&id, source)| (id, InputContract::from_source(source.as_ref())))
        .collect();
    compile(dag.as_view(), inputs, roots)?.instantiate(sources)
}

/// Resolve raw scan connectors before invoking the reader-independent compiler.
pub fn bind_with_data_sources<'a>(
    dag: &PostASAPDAGTransport,
    mut sources: BTreeMap<NodeId, Source<'a>>,
    roots: &[NodeId],
    data_sources: &crate::sources::DataSources,
) -> Result<PhysicalExecution<'a, Batch, Schema>, Error> {
    // Only resolve scans reachable below the selected input boundaries.
    let mut pending = roots.to_vec();
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) || sources.contains_key(&id) {
            continue;
        }
        let node = dag
            .nodes
            .iter()
            .find(|n| u64::from(n.id.0) == id)
            .ok_or_else(|| invalid(format!("missing node {id}")))?;
        if let Payload::Fallback {
            expression: expression @ PreASAPNode::Scan { .. },
        } = &node.payload
        {
            sources.insert(id, Box::new(data_sources.bind(expression)?));
        } else {
            pending.extend(
                dag.edges
                    .iter()
                    .filter(|e| u64::from(e.consumer.0) == id)
                    .map(|e| u64::from(e.producer.0)),
            );
        }
    }
    bind(dag, sources, roots)
}

#[cfg(test)]
thread_local! {
    /// Planner nodes lowered by this thread, for compile-once tests.
    static LOWERED_NODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Helper operators are numbered from their Planner node alone, above the u32
/// Planner ID range, so every boundary choice yields a subgraph of the same
/// lowering and candidate cuts need not renumber operators. A node lowering to
/// several helpers takes consecutive indices below its base.
fn helper_id(node: NodeId, index: u64) -> NodeId {
    debug_assert!(node <= u64::from(u32::MAX) && index < 1 << 16);
    u64::MAX - (node << 16) - index
}

fn compile_internal(
    dag: &planner_types::post_asap::PostASAPDAGView<'_>,
    mut sources: BTreeMap<NodeId, InputContract>,
    roots: &[NodeId],
) -> Result<PhysicalDAG, Error> {
    preflight_depth(dag)?;
    dag.validate().map_err(|e| invalid(e.to_string()))?;
    let nodes = dag
        .nodes()
        .iter()
        .map(|node| (u64::from(node.id.0), node))
        .collect::<BTreeMap<_, _>>();
    let mut dependencies = BTreeMap::<NodeId, Vec<NodeId>>::new();
    // Binary input order is semantic; serialized edge order is not.
    let mut edges = dag.edges().iter().collect::<Vec<_>>();
    edges.sort_by_key(|edge| {
        (
            edge.consumer.0,
            match edge.role {
                planner_types::post_asap::EdgeRole::Left => 0,
                planner_types::post_asap::EdgeRole::Input => 1,
                planner_types::post_asap::EdgeRole::Right => 2,
            },
        )
    });
    // Scalar literal operands of query-time arithmetic are folded into the consumer.
    let mut literals = BTreeMap::<NodeId, (f64, bool)>::new();
    for edge in edges {
        let consumer = u64::from(edge.consumer.0);
        if let (
            Payload::Fallback { expression },
            Some(PostAsapDagNode {
                payload: Payload::Binary { .. },
                ..
            }),
        ) = (
            &nodes[&u64::from(edge.producer.0)].payload,
            nodes.get(&consumer),
        ) {
            if let Some(value) = row_values::scalar_literal(expression) {
                let left = edge.role == planner_types::post_asap::EdgeRole::Left;
                if literals.insert(consumer, (value, left)).is_some() {
                    return Err(invalid("binary with two scalar literals is not folded"));
                }
                continue;
            }
        }
        dependencies
            .entry(u64::from(edge.consumer.0))
            .or_default()
            .push(u64::from(edge.producer.0));
    }
    let known = |id: &NodeId| {
        nodes.contains_key(id)
            || promql_fallback::raw_series_owner(*id).is_some_and(|owner| {
                matches!(
                    nodes.get(&owner),
                    Some(PostAsapDagNode {
                        payload: Payload::Fallback { .. },
                        ..
                    })
                )
            })
    };
    if !sources.keys().all(known) {
        return Err(invalid("source binding names an unknown node"));
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
        if !sources.contains_key(&id) {
            for &input in dependencies.get(&id).into_iter().flatten() {
                pending.push((input, false));
            }
        }
    }
    let mut graph = PhysicalDAG::new(roots.to_vec());
    for id in ordered {
        let node = nodes[&id];
        let mut auxiliary = helper_id(id, 0);
        let output = Arc::new(node.output_schema.clone());
        crate::values::validate_schema(&output)?;
        if let Some(source) = sources.remove(&id) {
            if source.schema != output {
                return Err(invalid("frontier does not have the declared schema"));
            }
            graph.add_input(id, source)?;
        } else {
            #[cfg(test)]
            LOWERED_NODES.with(|count| count.set(count.get() + 1));
            let mut inputs = dependencies.get(&id).cloned().unwrap_or_default();
            let mut schemas = inputs
                .iter()
                .map(|id| Arc::new(nodes[id].output_schema.clone()))
                .collect::<Vec<_>>();
            if matches!(node.payload, Payload::SummaryMerge) && inputs.len() > 1 {
                if schemas.iter().any(|s| s != &schemas[0]) {
                    return Err(invalid("summary merge inputs have different schemas"));
                }
                graph.add(
                    auxiliary,
                    inputs,
                    Operator::union(schemas[0].clone(), schemas.len())?,
                )?;
                inputs = vec![auxiliary];
                schemas.truncate(1);
            }
            // A consumed bare selector supplies raw range rows (e.g. to a
            // per-entity summary), not an instant vector, so only its consumer computes.
            let raw_rows = matches!(
                &node.payload,
                Payload::Fallback {
                    expression: PreASAPNode::TimeRange { .. }
                }
            ) && dag.edges().iter().any(|e| u64::from(e.producer.0) == id);
            if let (Payload::Fallback { expression }, false) = (&node.payload, raw_rows) {
                let promql_fallback::Lowering {
                    selectors,
                    mut steps,
                } = promql_fallback::lower(expression)
                    .map_err(|error| invalid(format!("node {id}: {error}")))?;
                let mut slots = Vec::new();
                for (i, (_, schema)) in selectors.iter().enumerate() {
                    let slot = promql_fallback::raw_series_input(id, i);
                    match sources.remove(&slot) {
                        Some(contract) if &contract.schema == schema => {
                            graph.add_input(slot, contract)?
                        }
                        Some(_) => {
                            return Err(invalid(format!(
                            "node {id}: raw series input {slot} differs from the selector schema"
                        )))
                        }
                        None => {
                            return Err(invalid(format!(
                                "node {id}: PromQL fallback requires raw series input {slot}"
                            )))
                        }
                    }
                    slots.push(slot);
                }
                let (last, last_inputs) = steps
                    .pop()
                    .ok_or_else(|| invalid("empty PromQL lowering"))?;
                let mut ids = Vec::new();
                let resolve = |inputs: Vec<promql_fallback::Input>, ids: &[NodeId]| {
                    inputs
                        .into_iter()
                        .map(|input| match input {
                            promql_fallback::Input::Raw(i) => slots[i],
                            promql_fallback::Input::Step(i) => ids[i],
                        })
                        .collect::<Vec<_>>()
                };
                for (operator, inputs) in steps {
                    graph.add(auxiliary, resolve(inputs, &ids), operator)?;
                    ids.push(auxiliary);
                    auxiliary -= 1;
                }
                graph.add(
                    id,
                    resolve(last_inputs, &ids),
                    last.with_output_schema(output)?,
                )?;
                continue;
            }
            if let Payload::Value {
                operation: ValueOperation::MaintainPopulation { population },
            } = &node.payload
            {
                use planner_types::post_asap::maintained_population::PopulationInput;
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
                graph.add(
                    id,
                    inputs,
                    Operator::current_series(input.clone(), identity, coordinate, value, lookback)?
                        .with_output_schema(output)?,
                )?;
                continue;
            }
            if let Payload::Value {
                operation: ValueOperation::ReadPopulation { readout },
            } = &node.payload
            {
                use planner_types::post_asap::maintained_population::{
                    PopulationInput, PopulationReadout,
                };
                let [producer] = inputs.as_slice() else {
                    return Err(invalid("population readout requires one input"));
                };
                let Payload::Value {
                    operation: ValueOperation::MaintainPopulation { population },
                } = &nodes[producer].payload
                else {
                    return Err(invalid(
                        "population readout requires its declared population",
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
                let PopulationReadout::TopK { k } = readout else {
                    let mut chain =
                        row_values::population_aggregate(&input, &spec.grouping, readout)?;
                    let last = chain.pop().expect("nonempty chain");
                    let mut inputs = inputs;
                    for operator in chain {
                        graph.add(auxiliary, inputs, operator)?;
                        inputs = vec![auxiliary];
                        auxiliary -= 1;
                    }
                    graph.add(id, inputs, last.with_output_schema(output)?)?;
                    continue;
                };
                let groups = spec
                    .grouping
                    .iter()
                    .map(|name| named_column(&input, &ColumnRef::Named(name.clone())))
                    .collect::<Result<Vec<_>, _>>()?;
                let value = named_column(&input, &ColumnRef::SampleValue)?;
                graph.add(
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
                graph.add(
                    id,
                    vec![auxiliary],
                    Operator::limit(input, *k as u64, 0, groups)?.with_output_schema(output)?,
                )?;
                continue;
            }
            // A closed row must include either all source labels or the explicit
            // complete-label identity. Projected labels alone are insufficient.
            if let Payload::SummaryAgg {
                family,
                input: update,
                reduction: PlannerReduction::PerEntity,
                grouping,
            } = &node.payload
            {
                let [input_id] = inputs.as_slice() else {
                    return Err(invalid("per-entity summary requires one input"));
                };
                let Payload::Fallback {
                    expression: PreASAPNode::TimeRange { child, .. },
                } = &nodes[input_id].payload
                else {
                    return Err(invalid(
                        "per-entity summary requires a resolved raw time range",
                    ));
                };
                let PreASAPNode::Scan { schema, .. } = child.as_ref() else {
                    return Err(invalid("per-entity summary requires a resolved source"));
                };
                if !schema.closed || update.item.is_some() {
                    return Err(invalid(
                        "per-entity summary requires complete source identity",
                    ));
                }
                crate::capability::validate_summary_kernel(family, update, grouping)
                    .map_err(Error::Invalid)?;
                let SummaryInputExpr::Column(value) = &update.weight else {
                    return Err(invalid(
                        "per-entity update requires a projected value column",
                    ));
                };
                let input = schemas[0].clone();
                let value = named_column(&input, value)?;
                let coordinate = input
                    .time_index
                    .ok_or_else(|| invalid("temporal input lacks time"))?;
                let groups = (0..input.fields.len())
                    .filter(|&column| column != value && column != coordinate)
                    .collect();
                let build = Operator::summary_build(
                    input,
                    family.clone(),
                    value,
                    Some(coordinate),
                    groups,
                )?;
                let compact = build.schema();
                graph.add(auxiliary, inputs, build)?;
                graph.add(
                    id,
                    vec![auxiliary],
                    Operator::scope_timestamp(compact, output)?,
                )?;
                continue;
            }
            if let Payload::Binary { operator } = &node.payload {
                let query_time =
                    dag.timing(node) == planner_types::post_asap::ExecutionTiming::QueryTime;
                if let Some(&(value, left)) = literals.get(&id) {
                    let [input] = schemas.as_slice() else {
                        return Err(invalid("scalar binary requires one row input"));
                    };
                    if !query_time {
                        return Err(invalid("scalar literal binary must run at query time"));
                    }
                    let scalar =
                        Operator::scalar(crate::values::Value::Float64(value), DataType::Float64)?;
                    let (sides, scalars, operands) = if left {
                        (
                            [scalar.schema(), input.clone()],
                            [true, false],
                            vec![auxiliary, inputs[0]],
                        )
                    } else {
                        (
                            [input.clone(), scalar.schema()],
                            [false, true],
                            vec![inputs[0], auxiliary],
                        )
                    };
                    let [l, r] = sides;
                    let binary = Operator::series_binary(l, r, operator.clone(), scalars)
                        .map_err(|error| invalid(format!("node {id}: {error}")))?;
                    graph.add(auxiliary, vec![], scalar)?;
                    graph.add(id, operands, binary.with_output_schema(output)?)?;
                    auxiliary -= 1;
                    continue;
                }
                let label_map = |schema: &Schema| {
                    schema
                        .fields
                        .iter()
                        .any(|f| matches!(f.dtype, SummaryFamilyType::Plain(DataType::Map { .. })))
                };
                // Grouped rows carry their labels as columns; per-series rows
                // carry the series identity.
                if let (true, [left, right]) = (query_time, schemas.as_slice()) {
                    if !label_map(left) && !label_map(right) {
                        // A scalar-valued Fallback operand, such as `scalar(x)`, has no labels.
                        let scalar = |input: &NodeId| {
                            matches!(
                                nodes.get(input).map(|node| &node.payload),
                                Some(Payload::Fallback { expression })
                                    if promql_fallback::scalar(expression)
                            )
                        };
                        let binary = Operator::series_binary(
                            left.clone(),
                            right.clone(),
                            operator.clone(),
                            [scalar(&inputs[0]), scalar(&inputs[1])],
                        )
                        .map_err(|error| invalid(format!("node {id}: {error}")))?;
                        graph.add(id, inputs, binary.with_output_schema(output)?)?;
                        continue;
                    }
                }
            }
            if let Payload::Value {
                operation: ValueOperation::FinalizeExactAccumulator,
            } = &node.payload
            {
                // Exact counts read out as Int64; PromQL declares a Float64 sample.
                let readout = bind_operation(node, dag.timing(node), &schemas)
                    .map_err(|error| invalid(format!("node {id}: {error}")))?;
                let actual = readout.schema();
                let converted = actual.fields.iter().zip(&output.fields).position(|(a, d)| {
                    a.dtype == SummaryFamilyType::Plain(DataType::Int64)
                        && d.dtype == SummaryFamilyType::Plain(DataType::Float64)
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
                    graph.add(auxiliary, inputs, readout)?;
                    if temporal_readout_drops_name(node) {
                        graph.add(auxiliary - 1, vec![auxiliary], project)?;
                        graph.add(
                            id,
                            vec![auxiliary - 1],
                            Operator::series_without_name(output)?,
                        )?;
                    } else {
                        graph.add(id, vec![auxiliary], project)?;
                    }
                    auxiliary -= 1;
                    continue;
                }
            }
            let mut operator = compile_timed_node(node, dag.timing(node), &schemas)
                .map_err(|error| invalid(format!("node {id}: {error}")))?;
            if operator.is_counter_readout() {
                let mut pending = vec![id];
                let mut visited = BTreeSet::new();
                let mut ranges = BTreeSet::new();
                while let Some(ancestor) = pending.pop() {
                    if !visited.insert(ancestor) {
                        continue;
                    }
                    if let Payload::Fallback {
                        expression: PreASAPNode::TimeRange { range, .. },
                    } = &nodes[&ancestor].payload
                    {
                        ranges.insert(
                            i64::try_from(range.as_millis())
                                .map_err(|_| invalid("counter lookback exceeds Int64"))?,
                        );
                        continue;
                    }
                    pending.extend(dependencies.get(&ancestor).into_iter().flatten().copied());
                }
                if ranges.len() > 1 {
                    return Err(invalid("counter readout has ambiguous logical windows"));
                }
                if let Some(lookback) = ranges.into_iter().next() {
                    operator = operator.with_counter_lookback(lookback)?;
                }
            }
            if temporal_readout_drops_name(node) {
                graph.add(auxiliary, inputs, operator)?;
                graph.add(id, vec![auxiliary], Operator::series_without_name(output)?)?;
            } else {
                graph.add(id, inputs, operator)?;
            }
        }
    }
    graph.validate()?;
    Ok(graph)
}

// Temporal summary readouts produce PromQL vectors, whose range functions drop
// the metric name before matching/filtering. Stored state retains its full identity.
fn temporal_readout_drops_name(node: &PostAsapDagNode) -> bool {
    node.output_schema
        .fields
        .iter()
        .any(|field| field.name == promql_rows::SERIES_IDENTITY_COLUMN)
        && matches!(
            &node.payload,
            Payload::Value {
                operation: ValueOperation::FinalizeExactAccumulator
            } | Payload::SummaryEstimate {
                query: SketchQuery::Quantile { .. }
                    | SketchQuery::Cardinality
                    | SketchQuery::PointCount { .. }
                    | SketchQuery::FrequencyL2
                    | SketchQuery::FrequencyEntropy
            }
        )
}

/// Bind a Planner node against the schemas supplied by its deployment edges.
/// This is the same checked path used by complete DAG binding.
pub fn compile_node(node: &PostAsapDagNode, inputs: &[Schema]) -> Result<Operator, Error> {
    compile_timed_node(node, node.output_state.timing, inputs)
}

/// Lower `node` under `timing`, which a lifecycle assignment may overlay.
fn compile_timed_node(
    node: &PostAsapDagNode,
    timing: planner_types::post_asap::ExecutionTiming,
    inputs: &[Schema],
) -> Result<Operator, Error> {
    for schema in inputs {
        crate::values::validate_schema(schema)?;
    }
    bind_operation(node, timing, inputs)?.with_output_schema(Arc::new(node.output_schema.clone()))
}

fn bind_operation(
    node: &PostAsapDagNode,
    timing: planner_types::post_asap::ExecutionTiming,
    inputs: &[Schema],
) -> Result<Operator, Error> {
    if let Payload::Binary { operator } = &node.payload {
        let [left, right] = inputs else {
            return Err(invalid("binary requires two inputs"));
        };
        if timing == planner_types::post_asap::ExecutionTiming::IngestionTime {
            let value = |schema: &Schema| -> Result<usize, Error> {
                let columns = schema
                    .fields
                    .iter()
                    .enumerate()
                    .filter(|(_, field)| {
                        field.dtype
                            == SummaryFamilyType::Plain(planner_types::pre_asap::DataType::Float64)
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
    if let Payload::RelationalJoin {
        join_kind,
        pred,
        pruning,
    } = &node.payload
    {
        use planner_types::{post_asap::CandidateCompleteness, pre_asap::JoinKind};
        if pruning.is_some() && *join_kind != JoinKind::Semi {
            return Err(invalid("pruning certificate requires a semi-join"));
        }
        if matches!(pruning,Some(CandidateCompleteness::Certified { guarantee }) if guarantee.has_unknown() || guarantee.metric != planner_types::post_asap::ErrorMetric::TopKMembership)
        {
            return Err(invalid("invalid pruning certificate"));
        }
        let [left, right] = inputs else {
            return Err(invalid("join requires two inputs"));
        };
        if *join_kind == JoinKind::Semi {
            if let Ok(keys) = equijoin_keys(pred, left, right) {
                let operator = Operator::semi_join(left.clone(), right.clone(), keys)?;
                return Ok(
                    if matches!(pruning, Some(CandidateCompleteness::Certified { .. })) {
                        operator.require_complete_right()
                    } else {
                        operator
                    },
                );
            }
        }
        if matches!(pruning, Some(CandidateCompleteness::Certified { .. })) {
            return Err(invalid("certified pruning requires explicit equijoin keys"));
        }
        return Operator::relational_join(
            left.clone(),
            right.clone(),
            join_kind.clone(),
            pred,
            Arc::new(node.output_schema.clone()),
        );
    }
    let [input] = inputs else {
        return Err(invalid(
            "native Planner binding currently requires a unary operation or an explicit source",
        ));
    };
    match &node.payload {
        Payload::Value { operation, .. } => match operation {
            ValueOperation::Project { cols, .. } => Operator::project(
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
                                PreASAPNode::Column(index) => Expression::Column(*index),
                                expr => expression(expr, input)?,
                            },
                        ))
                    })
                    .collect::<Result<_, Error>>()?,
            ),
            ValueOperation::Filter { pred } => {
                Operator::filter(input.clone(), expression(&pred.0, input)?)
            }
            ValueOperation::Sort { keys, partition_by } => Operator::sort(
                input.clone(),
                keys.iter()
                    .map(|key| {
                        let PreASAPNode::Column(column) = key.expr else {
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
            ValueOperation::Limit {
                n,
                offset,
                partition_by,
            } => Operator::limit(
                input.clone(),
                *n as u64,
                *offset as u64,
                groups(input, partition_by)?,
            ),
            ValueOperation::Exact(ExactOperation::Aggregate {
                reduction,
                measures,
                output_names,
                having: None,
            }) => {
                if measures.len() != output_names.len() {
                    return Err(invalid("aggregate output names differ from measures"));
                }
                let PlannerReduction::Reduce(keys) = reduction else {
                    return Err(invalid(
                        "per-entity aggregate requires an explicit entity binding",
                    ));
                };
                let measures = measures
                    .iter()
                    .zip(output_names)
                    .map(|(m, name)| {
                        let column = |col: Option<usize>| {
                            col.map(Ok)
                                .unwrap_or_else(|| named_column(input, &ColumnRef::SampleValue))
                        };
                        let m = match m {
                            AggIntent::Count { .. } => Reduction::Count,
                            AggIntent::Sum { col } => Reduction::Sum(column(*col)?),
                            AggIntent::Avg { col } => Reduction::Avg(column(*col)?),
                            AggIntent::Min { col } => Reduction::Min(column(*col)?),
                            AggIntent::Max { col } => Reduction::Max(column(*col)?),
                            _ => {
                                return Err(invalid(
                                    "aggregate intent has no native implementation",
                                ))
                            }
                        };
                        Ok((name.clone(), m))
                    })
                    .collect::<Result<_, Error>>()?;
                Operator::aggregate(input.clone(), groups(input, keys)?, measures)
            }
            ValueOperation::FinalizeExactAccumulator => {
                let state = summary_column(input)?;
                use crate::Statistic as S;
                use planner_types::post_asap::ExactKind as E;
                let statistic = match &input.fields[state].dtype {
                    SummaryFamilyType::ExactAggregate(kind, _) => match kind {
                        E::Sum => S::Sum,
                        E::Count => S::Count,
                        E::Min => S::Min,
                        E::Max => S::Max,
                        E::Rate => S::Rate,
                        E::Increase => S::Increase,
                        _ => return Err(invalid("exact family readout is unsupported")),
                    },
                    _ => return Err(invalid("exact finalization requires exact state")),
                };
                Operator::readout(
                    input.clone(),
                    state,
                    ReadoutQuery::Exact(ExactReadout {
                        statistic,
                        lookback_ms: None,
                    }),
                )
            }
            _ => Err(invalid("value operation has no native implementation")),
        },
        Payload::SummaryAgg {
            family,
            input: update,
            reduction,
            grouping,
        } => {
            if let Some(item) = &update.item {
                let PlannerReduction::Reduce(keys) = reduction else {
                    return Err(invalid("keyed summary requires explicit partitions"));
                };
                let SummaryInputExpr::Column(weight) = &update.weight else {
                    return Err(invalid(
                        "keyed summary weight must be a finalized value column",
                    ));
                };
                if matches!(family, SummaryFamilyType::Sketch(kind, _) if kind.algorithm() == &planner_types::post_asap::SketchAlgorithm::CmsWithHeap)
                    && !matches!(
                        update.weight_domain,
                        planner_types::post_asap::WeightDomain::NonNegative { .. }
                    )
                {
                    return Err(invalid("CMS requires a nonnegative weight contract"));
                }
                fn columns(
                    expr: &SummaryInputExpr,
                    input: &Schema,
                    result: &mut Vec<usize>,
                ) -> Result<(), Error> {
                    match expr {
                        SummaryInputExpr::Column(column) => {
                            result.push(named_column(input, column)?)
                        }
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
                return Operator::keyed_summary_build(
                    input.clone(),
                    family.clone(),
                    named_column(input, weight)?,
                    items,
                    groups(input, keys)?,
                );
            }
            crate::capability::validate_summary_kernel(family, update, grouping)
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
            Operator::summary_build(
                input.clone(),
                family.clone(),
                named_column(input, column)?,
                input.time_index,
                groups(input, keys)?,
            )
        }
        Payload::SummaryMerge => {
            let state = summary_column(input)?;
            Operator::summary_merge(
                input.clone(),
                state,
                (0..input.fields.len())
                    .filter(|&i| i != state && Some(i) != input.time_index)
                    .collect(),
            )
        }
        Payload::SummaryEstimate { query } => {
            if let SketchQuery::TopK { k } = query {
                return Operator::keyed_readout(
                    input.clone(),
                    summary_column(input)?,
                    *k,
                    Arc::new(node.output_schema.clone()),
                );
            }
            Operator::readout(
                input.clone(),
                summary_column(input)?,
                ReadoutQuery::Sketch(query.clone()),
            )
        }
        _ => Err(invalid(
            "physical operation has no native binding; no fallback is installed",
        )),
    }
}
fn summary_column(input: &Schema) -> Result<usize, Error> {
    let columns = input
        .fields
        .iter()
        .enumerate()
        .filter(|(_, f)| !matches!(f.dtype, SummaryFamilyType::Plain(_)))
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    match columns.as_slice() {
        [column] => Ok(*column),
        _ => Err(invalid("one summary state column required")),
    }
}
fn named_column(input: &Schema, column: &ColumnRef) -> Result<usize, Error> {
    let name = match column {
        // Executable SummarySchema retains column names, not table qualifiers.
        // Frontend binding has resolved the qualifier; still reject ambiguous
        // names here rather than guessing a join side.
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
fn groups(input: &Schema, groups: &GroupKeys) -> Result<Vec<usize>, Error> {
    if groups.is_without() {
        return Err(invalid("grouping without requires resolved label columns"));
    }
    if groups.keys().iter().any(|&i| i >= input.fields.len()) {
        return Err(invalid("grouping column out of range"));
    }
    Ok(groups.keys().to_vec())
}
fn expression(expr: &PreASAPNode, input: &Schema) -> Result<Expression, Error> {
    Ok(Expression::planner(
        crate::expressions::CompiledExpression::compile(expr, input)?,
    ))
}

struct CheckedSource<'a> {
    source: Source<'a>,
    output: Schema,
}
impl PhysicalOperator<Batch, Schema> for CheckedSource<'_> {
    fn properties(&self, inputs: &[crate::plan::PlanProperties]) -> crate::plan::PlanProperties {
        self.source.properties(inputs)
    }

    fn name(&self) -> &str {
        self.source.name()
    }
    fn input_schemas(&self) -> Vec<Schema> {
        vec![]
    }
    fn output_schema(&self) -> Schema {
        self.output.clone()
    }
    fn output_bytes(&self, batch: &Batch) -> usize {
        self.source.output_bytes(batch)
    }
    fn start<'a>(
        &'a self,
        inputs: Vec<crate::runtime::Input<'a, Batch>>,
        context: crate::runtime::RunContext,
    ) -> Result<crate::runtime::OutputStream<'a, Batch>, Error> {
        use futures::StreamExt;
        Ok(self
            .source
            .start(inputs, context)?
            .map(|batch| {
                let batch = batch?;
                if batch.schema() != &self.output {
                    return Err(invalid("source batch differs from its bound schema"));
                }
                Ok(batch)
            })
            .boxed_local())
    }
}

// Bound recursion before invoking the upstream recursive provenance validator.
fn preflight_depth(dag: &planner_types::post_asap::PostASAPDAGView<'_>) -> Result<(), Error> {
    let mut remaining = dag
        .nodes()
        .iter()
        .map(|node| (node.id, 0usize))
        .collect::<BTreeMap<_, _>>();
    if remaining.len() != dag.nodes().len() {
        return Err(invalid("duplicate Planner node"));
    }
    let mut consumers = BTreeMap::<_, Vec<_>>::new();
    for edge in dag.edges() {
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
    if visited != dag.nodes().len() {
        return Err(invalid("Planner DAG contains a cycle"));
    }
    Ok(())
}

/// Join predicates address the concatenated left/right schema.
fn semi_join_keys(
    expr: &PreASAPNode,
    left: usize,
    right: usize,
    keys: &mut Vec<(usize, usize)>,
) -> Result<(), Error> {
    match expr {
        PreASAPNode::BoolAnd(parts) => {
            for part in parts {
                semi_join_keys(part, left, right, keys)?;
            }
        }
        PreASAPNode::Compare {
            left: a,
            op: CompareOpKind::Eq,
            right: b,
        } => {
            let (PreASAPNode::Column(a), PreASAPNode::Column(b)) = (a.as_ref(), b.as_ref()) else {
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
    pred: &planner_types::pre_asap::Predicate,
    left: &planner_types::post_asap::SummarySchema,
    right: &planner_types::post_asap::SummarySchema,
) -> Result<Vec<(usize, usize)>, Error> {
    let mut keys = Vec::new();
    semi_join_keys(&pred.0, left.fields.len(), right.fields.len(), &mut keys)?;
    if keys.is_empty() {
        return Err(invalid("semi-join requires explicit matching keys"));
    }
    Ok(keys)
}
