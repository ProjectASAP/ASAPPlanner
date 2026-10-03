//! Compile immutable summary-input computation with explicit population and pane identity.
use super::promql_rows::SERIES_IDENTITY_COLUMN as SERIES_IDENTITY;
use super::*;
use planner_types::ir::properties::ExecutionTiming;
use planner_types::ir::schema::FieldDataType as SummaryFamilyType;
use planner_types::ir::schema::{DataType, GroupingStrategy, Schema};

/// Physical rows carry the population and pane coordinate alongside the logical value.
/// These fields preserve identities which are implicit in a stored summary instance.
pub fn population_schema(family: SummaryFamilyType) -> SchemaRef {
    Arc::new(Schema {
        fields: vec![
            planner_types::ir::schema::Field {
                name: "$population".into(),
                dtype: SummaryFamilyType::Plain(DataType::Map {
                    key: Box::new(DataType::Utf8),
                    value: Box::new(DataType::Utf8),
                    value_nullable: false,
                }),
                nullable: false,
                table: None,
            },
            planner_types::ir::schema::Field {
                name: "$window_end".into(),
                dtype: SummaryFamilyType::Plain(DataType::Timestamp),
                nullable: false,
                table: None,
            },
            planner_types::ir::schema::Field {
                name: "value".into(),
                dtype: family,
                nullable: false,
                table: None,
            },
        ],
        time_index: Some(1),
        unique_keys: vec![],
        closed: false,
    })
}

/// Raw sample rows at a precompute boundary. `$population` holds the series'
/// complete label set, so it is the complete source identity of per-series
/// summaries; `$timestamp` is the sample time and `value` a finite sample
/// (stale markers are not samples). Rows are what the boundary's source scan
/// selected; the deployment decides which rows and panes they are. Label sets
/// must be canonical (sorted, unique, no empty values), since they are the
/// population identity: build rows with [`raw_sample_row`].
pub fn raw_sample_schema() -> SchemaRef {
    let mut schema = (*population_schema(SummaryFamilyType::Plain(DataType::Float64))).clone();
    schema.fields[1].name = "$timestamp".into();
    Arc::new(schema)
}

/// A raw sample row whose label set is sorted, unique and omits empty values,
/// so one series always has one population identity.
pub fn raw_sample_row(
    labels: &BTreeMap<String, String>,
    timestamp_ms: i64,
    value: f64,
) -> Vec<crate::values::Value> {
    use crate::values::Value;
    vec![
        Value::Map(
            labels
                .iter()
                .filter(|(_, v)| !v.is_empty())
                .map(|(k, v)| {
                    (
                        Value::Utf8(k.as_str().into()),
                        Value::Utf8(v.as_str().into()),
                    )
                })
                .collect::<Vec<_>>()
                .into(),
        ),
        Value::Timestamp(timestamp_ms),
        Value::Float64(value),
    ]
}

/// Input contract of a precompute boundary: raw sample rows for a raw time
/// series scan, otherwise the stored population of its summary state.
pub fn boundary_schema(node: &PhysicalASAPDAGNode) -> Result<SchemaRef, Error> {
    if !matches!(
        &node.payload,
        Payload::Relational {
            operator: NonASAPOpKind::Scan {
                source: planner_types::ir::operator::Source::TimeSeries { .. },
                ..
            } | NonASAPOpKind::TimeRange { .. }
        }
    ) {
        return source_schema(&node.output_schema);
    }
    let logical = &node.output_schema;
    // Labels may be absent from a series; its label map then omits them.
    let valid = logical
        .fields
        .iter()
        .enumerate()
        .all(|(i, field)| match &field.dtype {
            SummaryFamilyType::Plain(DataType::Timestamp) => {
                Some(i) == logical.time_index && !field.nullable
            }
            SummaryFamilyType::Plain(DataType::Float64) => field.name == "value" && !field.nullable,
            SummaryFamilyType::Plain(DataType::Utf8) => true,
            _ => false,
        })
        && !logical
            .fields
            .iter()
            .any(|f| f.name.starts_with('$') && f.name != SERIES_IDENTITY)
        && logical.time_index.is_some()
        && logical.fields.iter().filter(|f| f.name == "value").count() == 1;
    if !valid {
        return Err(invalid(
            "raw sample boundary requires labels, a timestamp and one Float64 value",
        ));
    }
    Ok(raw_sample_schema())
}

/// Validate the adapter layout during installed-plan recovery without lowering operators.
/// Borrows shared schema metadata and returns an Arc-owned schema for the
/// population adapter layout.
pub fn source_schema(logical: &Schema) -> Result<SchemaRef, Error> {
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

pub fn is_population_schema(schema: &SchemaRef) -> bool {
    schema
        .fields
        .get(2)
        .is_some_and(|field| *schema == population_schema(field.dtype.clone()))
}

/// Compile a complete selected precompute sub-DAG. Inputs are already-computed
/// state boundaries; the deployment supplies groups, panes and states, never operations.
pub fn compile(
    dag: &PhysicalASAPDAG,
    frontiers: &[NodeId],
    roots: &[NodeId],
) -> Result<CompiledPhysicalDAG, Error> {
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
                planner_types::ir::export::EdgeRole::Left => 0,
                planner_types::ir::export::EdgeRole::Input => 1,
                planner_types::ir::export::EdgeRole::Right => 2,
                planner_types::ir::export::EdgeRole::ScalarRef => 3,
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
    let mut outputs = BTreeMap::<NodeId, SchemaRef>::new();
    for id in ordered {
        let node = nodes[&id];
        if frontier.contains(&id) {
            let schema = boundary_schema(node)?;
            sources.insert(id, InputContract::bounded(schema.clone()));
            outputs.insert(id, schema);
            continue;
        }
        if node.output_state.timing != ExecutionTiming::IngestionTime {
            return Err(invalid("precompute DAG contains a query-time operation"));
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
        let physical_dag = fragment(
            node,
            &schemas,
            &inputs.iter().map(|id| nodes[id]).collect::<Vec<_>>(),
        )?;
        outputs.insert(
            id,
            physical_dag
                .output_contract(physical_dag.roots()[0])?
                .schema,
        );
        fragments.insert(id, (inputs, physical_dag));
    }
    CompiledPhysicalDAG::compose(sources, fragments, roots.to_vec())
}

fn validate_value_output(node: &PhysicalASAPDAGNode) -> Result<(), Error> {
    let schema = &node.output_schema;
    // Physical population rows already carry the complete identity in `$population`.
    // Typed logical plans may expose its opaque series-identity column as metadata.
    let identity = planner_types::ir::schema::PROMQL_SERIES_IDENTITY;
    let identities = schema
        .fields
        .iter()
        .filter(|field| field.name == identity)
        .collect::<Vec<_>>();
    if identities.len() > 1
        || identities
            .iter()
            .any(|field| field.nullable || field.dtype != SummaryFamilyType::Plain(DataType::Utf8))
    {
        return Err(invalid(
            "precompute series identity requires one non-null Utf8 column",
        ));
    }
    let values = schema
        .fields
        .iter()
        .enumerate()
        .filter(|(i, field)| Some(*i) != schema.time_index && field.name != identity)
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
    node: &PhysicalASAPDAGNode,
    schemas: &[SchemaRef],
    parents: &[&PhysicalASAPDAGNode],
) -> Result<CompiledPhysicalDAG, Error> {
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
        Payload::Relational {
            operator:
                NonASAPOpKind::BinaryOp {
                    operator,
                    return_bool,
                },
        } => {
            let operator =
                crate::expressions::binary::BinaryOperator::from_logical(operator, *return_bool);
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
        Payload::FinalizeExactAccumulator => {
            let [input] = schemas else {
                return Err(invalid("finalize requires one state input"));
            };
            validate_value_output(node)?;
            let statistic = match &input.fields[2].dtype {
                SummaryFamilyType::ExactAggregate(planner_types::ir::schema::ExactKind::Sum, _) => {
                    crate::Statistic::Sum
                }
                SummaryFamilyType::ExactAggregate(
                    planner_types::ir::schema::ExactKind::Count,
                    _,
                ) => crate::Statistic::Count,
                _ => {
                    return Err(invalid(
                        "precompute finalization requires explicit Sum or Count semantics",
                    ))
                }
            };
            let read = Operator::evaluation(
                input.clone(),
                2,
                SummaryEvaluation::Exact(ExactEvaluation {
                    statistic,
                    lookback_ms: None,
                }),
            )?;
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
            filter,
        } => {
            if filter.is_some() {
                return Err(invalid(
                    "filtered summary update has no native implementation",
                ));
            }
            let [input] = schemas else {
                return Err(invalid("summary update requires one input"));
            };
            // Item identities resolve against the complete label set of raw
            // samples; finalized evaluations carry no such identity.
            let raw = *input == raw_sample_schema();
            // A unit-frequency summary (HLL) observes each raw sample value.
            let unit_frequency = raw
                && crate::capability::is_unit_sample_frequency(update)
                && matches!(family, SummaryFamilyType::Sketch(kind, _) if !matches!(
                    kind.algorithm(),
                    planner_types::ir::schema::SketchAlgorithm::Cms
                        | planner_types::ir::schema::SketchAlgorithm::CountSketch
                        | planner_types::ir::schema::SketchAlgorithm::CmsWithHeap
                        | planner_types::ir::schema::SketchAlgorithm::CountSketchWithHeap
                ));
            let keyed = update.item.is_some() && !unit_frequency;
            if (keyed && !raw) || !matches!(grouping, GroupingStrategy::PerSubpopulationInstance) {
                return Err(invalid(
                    "precompute keyed/shared update needs its dedicated physical candidate",
                ));
            }
            crate::capability::validate_summary_kernel(family, update, grouping)
                .map_err(Error::Invalid)?;
            if raw
                && matches!(
                    update.weight_domain,
                    planner_types::ir::schema::WeightDomain::NonNegative {
                        proof: planner_types::ir::schema::NonNegativeWeightProof::ResetAwareCounterDerivative
                    }
                )
            {
                return Err(invalid(
                    "a counter-derivative weight cannot be read from raw cumulative samples",
                ));
            }
            if keyed
                && matches!(family, SummaryFamilyType::Sketch(kind, _) if kind.algorithm() == &planner_types::ir::schema::SketchAlgorithm::CmsWithHeap)
                && !matches!(
                    update.weight_domain,
                    planner_types::ir::schema::WeightDomain::NonNegative { .. }
                )
            {
                return Err(invalid("CMS requires a nonnegative weight contract"));
            }
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
                                // A raw label map omits absent labels; the
                                // series identity is not one of its labels.
                                .filter(|field| {
                                    (raw || !field.nullable)
                                        && field.name != SERIES_IDENTITY
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
                _ if unit_frequency => Expression::Column(2),
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
            let mut columns = vec![
                ("$population".into(), labels),
                ("$window_end".into(), Expression::Column(1)),
                ("value".into(), Expression::FiniteFloat64(Box::new(weight))),
            ];
            let mut fields = population_schema(SummaryFamilyType::Plain(DataType::Float64))
                .fields
                .clone();
            if keyed {
                let mut items = Vec::new();
                raw_items(
                    update.item.as_ref().expect("keyed item"),
                    &parents[0].output_schema,
                    &mut items,
                )?;
                for (index, (expression, dtype)) in items.into_iter().enumerate() {
                    let name = format!("$item{index}");
                    fields.push(planner_types::ir::schema::Field {
                        name: name.clone(),
                        dtype: SummaryFamilyType::Plain(dtype),
                        nullable: false,
                        table: None,
                    });
                    columns.push((name, expression));
                }
            }
            let item_columns = (3..fields.len()).collect::<Vec<_>>();
            let project = Operator::project(input.clone(), columns)?.with_output_schema(
                Arc::new(Schema {
                    fields,
                    unique_keys: vec![],
                    closed: false,
                    time_index: Some(1),
                }),
            )?;
            let projected = project.schema();
            let project = add(vec![0], project)?;
            let build = if keyed {
                Operator::keyed_summary_build(projected, family.clone(), 2, item_columns, vec![0])?
            } else {
                Operator::summary_build(projected, family.clone(), 2, Some(1), vec![0])?
            };
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
    CompiledPhysicalDAG::from_operators(sources, operators, vec![root])
}

/// Resolve keyed item identities over raw sample rows: labels (absent labels
/// read as empty, as in PromQL), the sample value, or the canonical encoding
/// of the label set less excluded labels.
fn raw_items(
    expr: &SummaryInputExpr,
    scan: &Schema,
    items: &mut Vec<(Expression, DataType)>,
) -> Result<(), Error> {
    // Open PromQL scans need not list every label, so any name that is not
    // another scan column (value, time, series identity) reads as a label.
    let label = |column: &ColumnRef| match column {
        ColumnRef::Named(name) | ColumnRef::Qualified { name, .. }
            if !name.starts_with('$')
                && scan.fields.iter().all(|f| {
                    &f.name != name || f.dtype == SummaryFamilyType::Plain(DataType::Utf8)
                }) =>
        {
            Some(name.clone())
        }
        _ => None,
    };
    let identity = |excluding: Vec<String>| {
        (
            Expression::LabelIdentity {
                column: 0,
                excluding,
            },
            DataType::Utf8,
        )
    };
    match expr {
        SummaryInputExpr::Column(ColumnRef::SampleValue) => {
            items.push((Expression::Column(2), DataType::Float64))
        }
        SummaryInputExpr::Column(ColumnRef::Named(name) | ColumnRef::Qualified { name, .. })
            if name == "value" =>
        {
            items.push((Expression::Column(2), DataType::Float64))
        }
        SummaryInputExpr::Column(ColumnRef::Named(name) | ColumnRef::Qualified { name, .. })
            if name == SERIES_IDENTITY =>
        {
            items.push(identity(vec![]))
        }
        SummaryInputExpr::Column(column) if label(column).is_some() => items.push((
            Expression::Label {
                column: 0,
                name: label(column).expect("resolved label"),
            },
            DataType::Utf8,
        )),
        SummaryInputExpr::EntityIdentity(
            planner_types::ir::schema::EntityIdentity::PromqlLabelSet { excluding },
        ) => items.push(identity(
            excluding
                .iter()
                .map(|column| {
                    label(column).ok_or_else(|| invalid("excluded identity label is not a label"))
                })
                .collect::<Result<_, _>>()?,
        )),
        SummaryInputExpr::Tuple(parts) if !parts.is_empty() => {
            for part in parts {
                raw_items(part, scan, items)?;
            }
        }
        _ => {
            return Err(invalid(
                "keyed summary item does not resolve over raw samples",
            ))
        }
    }
    Ok(())
}
