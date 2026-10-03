//! Compile logical computation to native operators with typed external inputs.
//! Compilation needs no readers; deployment resolves inputs after selection.
use crate::operators::SummaryEvaluation;
use crate::summary_kernels::exact::ExactEvaluation;
use crate::{
    operators::{Expression, Operator, Reduction, SortKey},
    plan::{Boundedness, Emission, NodeId, PhysicalDAG, PhysicalOperator, PlanProperties},
    values::{Batch, SchemaRef},
    Error,
};
use planner_types::ir::export::{
    NonASAPOpKind, PostAsapDAG, PostAsapDAGNode, PostAsapOperatorPayload as Payload, WireScalarExpr,
};
use planner_types::ir::{ASAPOp, NonASAPOp, Operator as LogicalOperator, OperatorNode, ScalarExpr};
use planner_types::{
    post_asap::{FieldDataType, SketchStatistic, SummaryInputExpr},
    pre_asap::{
        AggIntent, ColumnRef, CompareOpKind, DataType, GroupKeys, Reduction as PlannerReduction,
    },
};
mod logical;
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
pub type Source<'a> = Box<dyn PhysicalOperator<Batch, SchemaRef> + 'a>;

pub mod precompute;
pub mod promql_fallback;
pub mod promql_rows;
pub mod promql_values;

mod candidates;
pub use candidates::{
    compile_candidate, compile_candidates, cut_candidate, enumerate_frontiers,
    frontier_from_timing, select_candidate, CandidateCost, CandidateSelection, PhysicalASAPDAG,
};

mod compiled;
pub use compiled::{CompiledPhysicalDAG, InputContract};

mod row_values;

/// Compile computation without opening or retaining deployment readers.
/// Input contracts identify explicit boundaries selected by maintenance planning.
pub fn compile(
    dag: &PostAsapDAG,
    inputs: BTreeMap<NodeId, InputContract>,
    roots: &[NodeId],
) -> Result<CompiledPhysicalDAG, Error> {
    compile_internal(dag, inputs, roots)
}

/// Convenience for callers that already resolved inputs. Lowering still uses
/// only their contracts, and instantiation checks those contracts again.
pub fn bind<'a>(
    dag: &PostAsapDAG,
    sources: BTreeMap<NodeId, Source<'a>>,
    roots: &[NodeId],
) -> Result<PhysicalDAG<'a, Batch, SchemaRef>, Error> {
    let inputs = sources
        .iter()
        .map(|(&id, source)| (id, InputContract::from_source(source.as_ref())))
        .collect();
    compile(dag, inputs, roots)?.instantiate(sources)
}

/// Resolve raw scan connectors before invoking the reader-independent compiler.
pub fn bind_with_data_sources<'a>(
    dag: &PostAsapDAG,
    mut sources: BTreeMap<NodeId, Source<'a>>,
    roots: &[NodeId],
    data_sources: &crate::sources::DataSources,
) -> Result<PhysicalDAG<'a, Batch, SchemaRef>, Error> {
    let restored = logical::restore(dag)?;
    // Only resolve scans reachable below the selected input boundaries.
    let mut pending = roots.to_vec();
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) || sources.contains_key(&id) {
            continue;
        }
        let _node = dag
            .nodes
            .iter()
            .find(|n| u64::from(n.id.0) == id)
            .ok_or_else(|| invalid(format!("missing node {id}")))?;
        if matches!(restored[&id].non_asap(), Some(NonASAPOp::Scan { .. })) {
            sources.insert(id, Box::new(data_sources.bind(&restored[&id])?));
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
/// Planner ID range, so every boundary choice yields a sub-DAG of the same
/// lowering and candidate cuts need not renumber operators. A node lowering to
/// several helpers takes consecutive indices below its base.
fn helper_id(node: NodeId, index: u64) -> NodeId {
    debug_assert!(node <= u64::from(u32::MAX) && index < 1 << 16);
    u64::MAX - (node << 16) - index
}

fn compile_internal(
    dag: &PostAsapDAG,
    mut sources: BTreeMap<NodeId, InputContract>,
    roots: &[NodeId],
) -> Result<CompiledPhysicalDAG, Error> {
    preflight_depth(dag)?;
    let restored = logical::restore(dag)?;
    dag.validate().map_err(|e| invalid(e.to_string()))?;
    let nodes = dag
        .nodes
        .iter()
        .map(|node| (u64::from(node.id.0), node))
        .collect::<BTreeMap<_, _>>();
    let mut dependencies = BTreeMap::<NodeId, Vec<NodeId>>::new();
    // Binary input order is semantic; serialized edge order is not.
    let mut edges = dag.edges.iter().collect::<Vec<_>>();
    edges.sort_by_key(|edge| {
        (
            edge.consumer.0,
            match edge.role {
                planner_types::ir::export::EdgeRole::Left => 0,
                planner_types::ir::export::EdgeRole::Input => 1,
                planner_types::ir::export::EdgeRole::Right => 2,
                planner_types::ir::export::EdgeRole::ScalarRef => 3,
            },
        )
    });
    let literals = BTreeMap::<NodeId, (f64, bool)>::new();
    for edge in edges {
        dependencies
            .entry(u64::from(edge.consumer.0))
            .or_default()
            .push(u64::from(edge.producer.0));
    }
    let mut fallback = BTreeMap::new();
    for (&id, root) in &restored {
        let raw_summary_input = matches!(root.non_asap(), Some(NonASAPOp::TimeRange { .. }))
            && dag.edges.iter().any(|e| {
                u64::from(e.producer.0) == id
                    && matches!(
                        nodes[&u64::from(e.consumer.0)].payload,
                        Payload::SummaryAgg { .. }
                    )
            });
        if !root.contains_asap() && !raw_summary_input {
            if let Ok(lowered) = promql_fallback::lower(root) {
                fallback.insert(id, lowered);
            }
        }
    }
    let known = |id: &NodeId| {
        nodes.contains_key(id)
            || promql_fallback::raw_series_owner(*id).is_some_and(|owner| {
                matches!(
                    nodes.get(&owner),
                    Some(PostAsapDAGNode {
                        payload: Payload::Relational { .. },
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
        if !sources.contains_key(&id) && !fallback.contains_key(&id) {
            for &input in dependencies.get(&id).into_iter().flatten() {
                pending.push((input, false));
            }
        }
    }
    let mut physical_dag = CompiledPhysicalDAG::new(roots.to_vec());
    for id in ordered {
        let node = nodes[&id];
        let mut auxiliary = helper_id(id, 0);
        let output = Arc::new(node.output_schema.clone());
        crate::values::validate_schema(&output)?;
        if let Some(source) = sources.remove(&id) {
            if source.schema != output {
                return Err(invalid("frontier does not have the declared schema"));
            }
            physical_dag.add_input(id, source)?;
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
                physical_dag.add(
                    auxiliary,
                    inputs,
                    Operator::union(schemas[0].clone(), schemas.len())?,
                )?;
                inputs = vec![auxiliary];
                schemas.truncate(1);
            }
            if let Some(promql_fallback::Lowering {
                selectors,
                mut steps,
            }) = fallback.remove(&id)
            {
                let mut slots = Vec::new();
                for (i, (_, schema)) in selectors.iter().enumerate() {
                    let slot = promql_fallback::raw_series_input(id, i);
                    match sources.remove(&slot) {
                        Some(contract) if &contract.schema == schema => {
                            physical_dag.add_input(slot, contract)?
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
                    physical_dag.add(auxiliary, resolve(inputs, &ids), operator)?;
                    ids.push(auxiliary);
                    auxiliary -= 1;
                }
                physical_dag.add(
                    id,
                    resolve(last_inputs, &ids),
                    last.with_output_schema(output)?,
                )?;
                continue;
            }
            if let Payload::MaintainPopulation { population } = &node.payload {
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
                physical_dag.add(
                    id,
                    inputs,
                    Operator::current_series(input.clone(), identity, coordinate, value, lookback)?
                        .with_output_schema(output)?,
                )?;
                continue;
            }
            if let Payload::EvaluatePopulation { evaluation } = &node.payload {
                use planner_types::post_asap::maintained_population::{
                    PopulationInput, PopulationStatistic,
                };
                let [producer] = inputs.as_slice() else {
                    return Err(invalid("population evaluation requires one input"));
                };
                let Payload::MaintainPopulation { population } = &nodes[producer].payload else {
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
                    for operator in chain {
                        physical_dag.add(auxiliary, inputs, operator)?;
                        inputs = vec![auxiliary];
                        auxiliary -= 1;
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
            // A closed row must include either all source labels or the explicit
            // complete-label identity. Projected labels alone are insufficient.
            if let Payload::SummaryAgg {
                family,
                input: update,
                reduction: PlannerReduction::PerEntity,
                grouping,
                filter: None,
            } = &node.payload
            {
                let [input_id] = inputs.as_slice() else {
                    return Err(invalid("per-entity summary requires one input"));
                };
                let Some(NonASAPOp::TimeRange { child, .. }) = restored[input_id].non_asap() else {
                    return Err(invalid(
                        "per-entity summary requires a resolved raw time range",
                    ));
                };
                let Some(NonASAPOp::Scan { schema, .. }) = child.non_asap() else {
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
                physical_dag.add(auxiliary, inputs, build)?;
                physical_dag.add(
                    id,
                    vec![auxiliary],
                    Operator::scope_timestamp(compact, output)?,
                )?;
                continue;
            }
            if let Payload::Relational {
                operator:
                    NonASAPOpKind::BinaryOp {
                        operator,
                        return_bool,
                    },
            } = &node.payload
            {
                let operator = crate::expressions::binary::BinaryOperator::from_logical(
                    operator,
                    *return_bool,
                );
                let query_time = node.output_state.timing
                    == planner_types::post_asap::ExecutionTiming::QueryTime;
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
                    physical_dag.add(auxiliary, vec![], scalar)?;
                    physical_dag.add(id, operands, binary.with_output_schema(output)?)?;
                    auxiliary -= 1;
                    continue;
                }
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
            if let Payload::FinalizeExactAccumulator = &node.payload {
                // Exact counts read out as Int64; PromQL declares a Float64 sample.
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
                    auxiliary -= 1;
                    continue;
                }
            }
            let mut operator = compile_node(node, &schemas)
                .map_err(|error| invalid(format!("node {id}: {error}")))?;
            if operator.is_counter_evaluation() {
                let mut pending = vec![id];
                let mut visited = BTreeSet::new();
                let mut ranges = BTreeSet::new();
                while let Some(ancestor) = pending.pop() {
                    if !visited.insert(ancestor) {
                        continue;
                    }
                    if let Payload::Relational {
                        operator: NonASAPOpKind::TimeRange { range, .. },
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
                    return Err(invalid("counter evaluation has ambiguous logical windows"));
                }
                if let Some(lookback) = ranges.into_iter().next() {
                    operator = operator.with_counter_lookback(lookback)?;
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

// Temporal summary evaluations produce PromQL vectors, whose range functions drop
// the metric name before matching/filtering. Stored state retains its full identity.
fn temporal_evaluation_drops_name(node: &PostAsapDAGNode) -> bool {
    node.output_schema
        .fields
        .iter()
        .any(|field| field.name == promql_rows::SERIES_IDENTITY_COLUMN)
        && matches!(
            &node.payload,
            Payload::FinalizeExactAccumulator
                | Payload::SummaryEstimate {
                    query: SketchStatistic::Quantile { .. }
                        | SketchStatistic::Cardinality
                        | SketchStatistic::PointCount { .. }
                        | SketchStatistic::FrequencyL2
                        | SketchStatistic::FrequencyEntropy
                }
        )
}

/// Bind a Planner node against the schemas supplied by its deployment edges.
/// This is the same checked path used by complete DAG binding.
pub fn compile_node(node: &PostAsapDAGNode, inputs: &[SchemaRef]) -> Result<Operator, Error> {
    for schema in inputs {
        crate::values::validate_schema(schema)?;
    }
    bind_operation(node, inputs)?.with_output_schema(Arc::new(node.output_schema.clone()))
}

fn bind_operation(node: &PostAsapDAGNode, inputs: &[SchemaRef]) -> Result<Operator, Error> {
    if let Payload::Relational {
        operator: NonASAPOpKind::BinaryOp {
            operator,
            return_bool,
        },
    } = &node.payload
    {
        let operator =
            crate::expressions::binary::BinaryOperator::from_logical(operator, *return_bool);
        let [left, right] = inputs else {
            return Err(invalid("binary requires two inputs"));
        };
        if node.output_state.timing == planner_types::post_asap::ExecutionTiming::IngestionTime {
            let value = |schema: &SchemaRef| -> Result<usize, Error> {
                let columns = schema
                    .fields
                    .iter()
                    .enumerate()
                    .filter(|(_, field)| {
                        field.dtype
                            == FieldDataType::Plain(planner_types::pre_asap::DataType::Float64)
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
    if let Payload::Relational {
        operator: NonASAPOpKind::Join { join_kind, pred },
    } = &node.payload
    {
        let [left, right] = inputs else {
            return Err(invalid("join requires two inputs"));
        };
        let pred = planner_types::ir::Predicate(local_scalar(&pred.0)?);
        if *join_kind == planner_types::pre_asap::JoinKind::Semi {
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
    if let Payload::Relational {
        operator: NonASAPOpKind::Values { rows, schema },
    } = &node.payload
    {
        if !inputs.is_empty() {
            return Err(invalid("Values takes no relational inputs"));
        }
        let empty = Arc::new(planner_types::pre_asap::Schema::default());
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
        Payload::FinalizeExactAccumulator => {
            let state = summary_column(input)?;
            use crate::Statistic as S;
            use planner_types::post_asap::ExactKind as E;
            let statistic = match &input.fields[state].dtype {
                FieldDataType::ExactAggregate(kind, _) => match kind {
                    E::Sum => S::Sum,
                    E::Count => S::Count,
                    E::Min => S::Min,
                    E::Max => S::Max,
                    E::Rate => S::Rate,
                    E::Increase => S::Increase,
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
                }),
            )
        }

        Payload::Relational { operator } => match operator {
            NonASAPOpKind::Project { cols, .. } => Operator::project(
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
                                WireScalarExpr::Column(index) => Expression::Column(*index),
                                expr => expression(expr, input)?,
                            },
                        ))
                    })
                    .collect::<Result<_, Error>>()?,
            ),
            NonASAPOpKind::Filter { pred } => {
                Operator::filter(input.clone(), expression(&pred.0, input)?)
            }
            NonASAPOpKind::Sort { keys, partition_by } => Operator::sort(
                input.clone(),
                keys.iter()
                    .map(|key| {
                        let WireScalarExpr::Column(column) = key.expr else {
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
            NonASAPOpKind::Limit {
                n,
                offset,
                partition_by,
            } => Operator::limit(
                input.clone(),
                n.unwrap_or(usize::MAX) as u64,
                *offset as u64,
                groups(input, partition_by)?,
            ),
            NonASAPOpKind::Aggregate {
                reduction,
                measures,
                output_names,
                filters,
                having: None,
            } => {
                if filters.iter().any(Option::is_some) {
                    return Err(invalid("filtered aggregate has no native implementation"));
                }
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
                            AggIntent::Cardinality { cols, .. } => {
                                Reduction::Cardinality(if cols.is_empty() {
                                    vec![column(None)?]
                                } else {
                                    cols.clone()
                                })
                            }
                            AggIntent::Sum { col } => Reduction::Sum(column(*col)?),
                            AggIntent::Avg { col } => Reduction::Avg(column(*col)?),
                            AggIntent::FrequencyL2 { col, .. } => {
                                Reduction::FrequencyL2(column(*col)?)
                            }
                            AggIntent::FrequencyEntropy { col, .. } => {
                                Reduction::FrequencyEntropy(column(*col)?)
                            }
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
            _ => Err(invalid("value operation has no native implementation")),
        },
        Payload::SummaryAgg {
            family,
            input: update,
            reduction,
            grouping,
            filter,
        } => {
            if filter.is_some() {
                return Err(invalid(
                    "filtered summary update has no native implementation",
                ));
            }
            if let Some(item) = &update.item {
                let PlannerReduction::Reduce(keys) = reduction else {
                    return Err(invalid("keyed summary requires explicit partitions"));
                };
                let SummaryInputExpr::Column(weight) = &update.weight else {
                    return Err(invalid(
                        "keyed summary weight must be a finalized value column",
                    ));
                };
                if matches!(family, FieldDataType::Sketch(kind, _) if kind.algorithm() == &planner_types::post_asap::SketchAlgorithm::CmsWithHeap)
                    && !matches!(
                        update.weight_domain,
                        planner_types::post_asap::WeightDomain::NonNegative { .. }
                    )
                {
                    return Err(invalid("CMS requires a nonnegative weight contract"));
                }
                fn columns(
                    expr: &SummaryInputExpr,
                    input: &SchemaRef,
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
            if let SketchStatistic::TopK { k } = query {
                return Operator::keyed_evaluation(
                    input.clone(),
                    summary_column(input)?,
                    *k,
                    Arc::new(node.output_schema.clone()),
                );
            }
            Operator::evaluation(
                input.clone(),
                summary_column(input)?,
                SummaryEvaluation::Sketch(query.clone()),
            )
        }
        _ => Err(invalid(
            "physical operation has no native binding; no fallback is installed",
        )),
    }
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
fn expression(expr: &WireScalarExpr, input: &SchemaRef) -> Result<Expression, Error> {
    let expr = local_scalar(expr)?;
    Ok(Expression::planner(
        crate::expressions::CompiledExpression::compile(&expr, input)?,
    ))
}

struct CheckedSource<'a> {
    source: Source<'a>,
    output: SchemaRef,
}
impl PhysicalOperator<Batch, SchemaRef> for CheckedSource<'_> {
    fn properties(&self, inputs: &[crate::plan::PlanProperties]) -> crate::plan::PlanProperties {
        self.source.properties(inputs)
    }

    fn name(&self) -> &str {
        self.source.name()
    }
    fn input_schemas(&self) -> Vec<SchemaRef> {
        vec![]
    }
    fn output_schema(&self) -> SchemaRef {
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
fn preflight_depth(dag: &PostAsapDAG) -> Result<(), Error> {
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
    left: &planner_types::post_asap::Schema,
    right: &planner_types::post_asap::Schema,
) -> Result<Vec<(usize, usize)>, Error> {
    let mut keys = Vec::new();
    semi_join_keys(&pred.0, left.fields.len(), right.fields.len(), &mut keys)?;
    if keys.is_empty() {
        return Err(invalid("semi-join requires explicit matching keys"));
    }
    Ok(keys)
}

fn local_scalar(expr: &WireScalarExpr) -> Result<ScalarExpr, Error> {
    let mut missing = false;
    let result = logical::scalar(expr, &mut |_| {
        missing = true;
        std::rc::Rc::new(OperatorNode::with_schema(
            LogicalOperator::NonASAP(NonASAPOp::Values {
                rows: vec![],
                schema: Default::default(),
            }),
            Default::default(),
        ))
    });
    if missing {
        Err(invalid(
            "scalar plan reads require explicit execution bindings",
        ))
    } else {
        Ok(result)
    }
}
