//! Compile immutable summary-input computation with explicit population and pane identity.
use super::*;
use planner_types::{
    post_asap::{ExecutionTiming, GroupingStrategy, SummarySchema},
    pre_asap::DataType,
};

/// Physical rows carry the population and pane coordinate alongside the logical value.
/// These fields preserve identities which are implicit in a stored summary instance.
pub fn population_schema(family: SummaryFamilyType) -> Schema {
    Arc::new(SummarySchema {
        fields: vec![
            planner_types::post_asap::SummaryField {
                name: "$population".into(),
                dtype: SummaryFamilyType::Plain(DataType::Map {
                    key: Box::new(DataType::Utf8),
                    value: Box::new(DataType::Utf8),
                    value_nullable: false,
                }),
                nullable: false,
            },
            planner_types::post_asap::SummaryField {
                name: "$window_end".into(),
                dtype: SummaryFamilyType::Plain(DataType::Timestamp),
                nullable: false,
            },
            planner_types::post_asap::SummaryField {
                name: "value".into(),
                dtype: family,
                nullable: false,
            },
        ],
        time_index: Some(1),
    })
}

/// Validate the adapter layout during installed-plan recovery without lowering operators.
pub fn source_schema(logical: &SummarySchema) -> Result<Schema, Error> {
    let states = logical
        .fields
        .iter()
        .filter(|f| !matches!(f.dtype, SummaryFamilyType::Plain(_)))
        .collect::<Vec<_>>();
    let [state] = states.as_slice() else {
        return Err(invalid(
            "stored population requires one typed summary state",
        ));
    };
    if logical.fields.iter().enumerate().any(|(i, field)| matches!(&field.dtype, SummaryFamilyType::Plain(dtype)
        if field.nullable || !matches!(dtype, DataType::Utf8) && !(Some(i) == logical.time_index && *dtype == DataType::Timestamp))) {
        return Err(invalid("stored population metadata cannot reconstruct extra value columns"));
    }
    if state.nullable {
        return Err(invalid("stored population state cannot be null"));
    }
    Ok(population_schema(state.dtype.clone()))
}

pub fn is_population_schema(schema: &Schema) -> bool {
    schema
        .fields
        .get(2)
        .is_some_and(|field| *schema == population_schema(field.dtype.clone()))
}

/// Compile a complete selected precompute sub-DAG. Inputs are already-computed
/// state boundaries; the deployment supplies groups, panes and states, never operations.
pub fn compile(
    dag: &PostAsapDag,
    frontiers: &[NodeId],
    roots: &[NodeId],
) -> Result<CompiledPhysicalDag, Error> {
    preflight_depth(dag)?;
    dag.validate().map_err(|e| invalid(e.to_string()))?;
    let nodes = dag
        .nodes
        .iter()
        .map(|n| (u64::from(n.id.0), n))
        .collect::<BTreeMap<_, _>>();
    let frontier = frontiers.iter().copied().collect::<BTreeSet<_>>();
    if frontier.len() != frontiers.len() || roots.iter().any(|r| frontier.contains(r)) {
        return Err(invalid(
            "precompute boundaries must be distinct from outputs",
        ));
    }
    let mut dependencies = BTreeMap::<NodeId, Vec<NodeId>>::new();
    let mut edges = dag.edges.iter().collect::<Vec<_>>();
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
    for edge in edges {
        dependencies
            .entry(u64::from(edge.consumer.0))
            .or_default()
            .push(u64::from(edge.producer.0));
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
            return Err(invalid("missing precompute node"));
        }
        pending.push((id, true));
        if !frontier.contains(&id) {
            pending.extend(
                dependencies
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .map(|id| (*id, false)),
            );
        }
    }
    let mut sources = BTreeMap::new();
    let mut fragments = BTreeMap::new();
    let mut outputs = BTreeMap::<NodeId, Schema>::new();
    for id in ordered {
        let node = nodes[&id];
        if frontier.contains(&id) {
            let schema = source_schema(&node.output_schema)?;
            sources.insert(id, InputContract::bounded(schema.clone()));
            outputs.insert(id, schema);
            continue;
        }
        if node.output_state.timing != ExecutionTiming::IngestionTime {
            return Err(invalid("precompute graph contains a query-time operation"));
        }
        let inputs = dependencies.get(&id).cloned().unwrap_or_default();
        let schemas = inputs
            .iter()
            .map(|id| {
                outputs
                    .get(id)
                    .cloned()
                    .ok_or_else(|| invalid("missing precompute input"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let graph = fragment(
            node,
            &schemas,
            &inputs.iter().map(|id| nodes[id]).collect::<Vec<_>>(),
        )?;
        outputs.insert(id, graph.output_contract(graph.roots()[0])?.schema);
        fragments.insert(id, (inputs, graph));
    }
    CompiledPhysicalDag::compose(sources, fragments, roots.to_vec())
}

fn validate_value_output(node: &PostAsapDagNode) -> Result<(), Error> {
    let schema = &node.output_schema;
    let values = schema
        .fields
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != schema.time_index)
        .collect::<Vec<_>>();
    if !matches!(values.as_slice(), [(_, field)] if !field.nullable && field.dtype == SummaryFamilyType::Plain(DataType::Float64))
        || schema.time_index.is_some_and(|i| {
            schema.fields.get(i).is_none_or(|f| {
                f.nullable || f.dtype != SummaryFamilyType::Plain(DataType::Timestamp)
            })
        })
    {
        return Err(invalid(
            "precompute value schema requires Float64 and an optional declared timestamp",
        ));
    }
    Ok(())
}

fn fragment(
    node: &PostAsapDagNode,
    schemas: &[Schema],
    parents: &[&PostAsapDagNode],
) -> Result<CompiledPhysicalDag, Error> {
    let sources = schemas
        .iter()
        .enumerate()
        .map(|(id, schema)| (id as u64, InputContract::bounded(schema.clone())))
        .collect();
    let mut operators = BTreeMap::new();
    let mut next = schemas.len() as u64;
    let mut add = |inputs: Vec<NodeId>, op: Operator| -> Result<NodeId, Error> {
        let id = next;
        next += 1;
        operators.insert(id, (inputs, op));
        Ok(id)
    };
    let root = match &node.payload {
        Payload::Binary { operator } => {
            validate_value_output(node)?;
            if node.output_schema.time_index.is_none()
                || parents.iter().any(|p| p.output_schema.time_index.is_none())
            {
                return Err(invalid(
                    "precompute binary requires declared window timestamps",
                ));
            }
            let [left, right] = schemas else {
                return Err(invalid("precompute binary requires two inputs"));
            };
            add(
                vec![0, 1],
                Operator::aligned_binary(
                    left.clone(),
                    right.clone(),
                    vec![(0, 0), (1, 1)],
                    (2, 2),
                    operator.clone(),
                )?,
            )?
        }
        Payload::Value {
            operation: ValueOperation::FinalizeExactAccumulator,
        } => {
            let [input] = schemas else {
                return Err(invalid("finalize requires one state input"));
            };
            validate_value_output(node)?;
            let statistic = match &input.fields[2].dtype {
                SummaryFamilyType::ExactAggregate(planner_types::post_asap::ExactKind::Sum, _) => {
                    crate::Statistic::Sum
                }
                SummaryFamilyType::ExactAggregate(
                    planner_types::post_asap::ExactKind::Count,
                    _,
                ) => crate::Statistic::Count,
                _ => {
                    return Err(invalid(
                        "precompute finalization requires explicit Sum or Count semantics",
                    ))
                }
            };
            let read = Operator::readout(input.clone(), 2, statistic, Default::default())?;
            let output = read.schema();
            let read = add(vec![0], read)?;
            let project = Operator::project(
                output,
                vec![
                    ("$population".into(), Expression::Column(0)),
                    ("$window_end".into(), Expression::Column(1)),
                    (
                        "value".into(),
                        Expression::FiniteFloat64(Box::new(Expression::ExactFloat64(2))),
                    ),
                ],
            )?
            .with_output_schema(population_schema(SummaryFamilyType::Plain(
                DataType::Float64,
            )))?;
            add(vec![read], project)?
        }
        Payload::SummaryAgg {
            family,
            input: update,
            reduction,
            grouping,
        } => {
            let [input] = schemas else {
                return Err(invalid("summary update requires one input"));
            };
            if update.item.is_some()
                || !matches!(grouping, GroupingStrategy::PerSubpopulationInstance)
            {
                return Err(invalid(
                    "precompute keyed/shared update needs its dedicated physical candidate",
                ));
            }
            crate::capability::validate_summary_kernel(family, update, grouping)
                .map_err(Error::Invalid)?;
            let labels = match reduction {
                PlannerReduction::PerEntity => Expression::Column(0),
                PlannerReduction::Reduce(keys) => Expression::LabelSet {
                    column: 0,
                    labels: keys
                        .keys()
                        .iter()
                        .map(|key| {
                            parents[0]
                                .output_schema
                                .fields
                                .get(*key)
                                .filter(|field| {
                                    !field.nullable
                                        && field.dtype == SummaryFamilyType::Plain(DataType::Utf8)
                                })
                                .map(|f| f.name.clone())
                                .ok_or_else(|| {
                                    invalid("summary grouping must identify population labels")
                                })
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    without: keys.is_without(),
                },
            };
            let weight = match &update.weight {
                SummaryInputExpr::Constant(value) => Expression::Literal {
                    value: crate::values::Value::Float64(*value),
                    dtype: DataType::Float64,
                },
                SummaryInputExpr::Column(ColumnRef::SampleValue) => Expression::Column(2),
                SummaryInputExpr::Column(ColumnRef::Named(name))
                    if parents[0].output_schema.fields.iter().any(|f| {
                        f.name == *name && f.dtype == SummaryFamilyType::Plain(DataType::Float64)
                    }) =>
                {
                    Expression::Column(2)
                }
                _ => {
                    return Err(invalid(
                        "summary weight does not resolve to the input value",
                    ))
                }
            };
            let project = Operator::project(
                input.clone(),
                vec![
                    ("$population".into(), labels),
                    ("$window_end".into(), Expression::Column(1)),
                    ("value".into(), Expression::FiniteFloat64(Box::new(weight))),
                ],
            )?
            .with_output_schema(population_schema(SummaryFamilyType::Plain(
                DataType::Float64,
            )))?;
            let projected = project.schema();
            let project = add(vec![0], project)?;
            let build = Operator::summary_build(projected, family.clone(), 2, Some(1), vec![0])?;
            let built = build.schema();
            let build = add(vec![project], build)?;
            add(
                vec![build],
                Operator::scope_timestamp(built, population_schema(family.clone()))?,
            )?
        }
        Payload::SummaryMerge => {
            let Some(input) = schemas.first() else {
                return Err(invalid("summary merge requires inputs"));
            };
            if schemas.iter().any(|s| s != input) {
                return Err(invalid("summary merge inputs differ"));
            }
            let union = add(
                (0..schemas.len() as u64).collect(),
                Operator::union(input.clone(), schemas.len())?,
            )?;
            let merge = Operator::summary_merge(input.clone(), 2, vec![0])?;
            let merged = merge.schema();
            let merge = add(vec![union], merge)?;
            add(
                vec![merge],
                Operator::scope_timestamp(merged, input.clone())?,
            )?
        }
        _ => {
            return Err(invalid(
                "precompute operation has no native population implementation",
            ))
        }
    };
    CompiledPhysicalDag::from_operators(sources, operators, vec![root])
}
