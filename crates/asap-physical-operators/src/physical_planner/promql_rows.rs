//! A bounded PromQL source row carries the entire label set, not just labels
//! mentioned by the query. The source adapter owns this lossless encoding.
use super::*;
use planner_types::pre_asap::{Column, DataType, Source as LogicalSource};
use std::rc::Rc;

/// Not a legal PromQL label name, so it cannot shadow a user label.
pub use planner_types::pre_asap::schema::PROMQL_SERIES_IDENTITY as SERIES_IDENTITY_COLUMN;

/// Canonical, reversible identity. JSON object encoding preserves label names,
/// empty values and escaping; sorting makes ingestion order irrelevant.
pub fn encode_series_identity(labels: &BTreeMap<String, String>) -> Result<String, Error> {
    serde_json::to_string(labels).map_err(|error| invalid(error.to_string()))
}

pub fn decode_series_identity(encoded: &str) -> Result<BTreeMap<String, String>, Error> {
    let labels: BTreeMap<String, String> =
        serde_json::from_str(encoded).map_err(|error| invalid(error.to_string()))?;
    if encode_series_identity(&labels)? != encoded {
        return Err(invalid("series identity is not canonically encoded"));
    }
    Ok(labels)
}

/// Resolve the row representation before candidate search. `closed` describes
/// physical columns here: the final column contains every dynamic source label.
/// It does not assert that the query's projected labels are the full label set.
///
/// This realization supports `by` and `without` grouping, per-series
/// computation, subqueries, `scalar()`, and one-to-one arithmetic. Operators
/// that rewrite or implicitly match dynamic label sets require their own
/// realization; they must not accidentally treat the opaque identity as a
/// user label or silently discard it. `promql_fallback` realizes `without`
/// and vector matching by rewriting the identity.
pub fn with_series_identity(root: &QueryExpr) -> Result<QueryExpr, Error> {
    let mut root = root.clone();
    fn visit(node: &mut QueryExpr) -> Result<(), Error> {
        match node {
            QueryExpr::Scan {
                source: LogicalSource::TimeSeries { .. },
                schema,
                ..
            } => {
                if schema
                    .columns
                    .iter()
                    .any(|column| column.name == SERIES_IDENTITY_COLUMN)
                {
                    return Err(invalid(
                        "source already contains a physical series identity",
                    ));
                }
                if schema.closed {
                    return Err(invalid(
                        "dynamic series identity requires an open PromQL source",
                    ));
                }
                schema
                    .columns
                    .push(Column::new(SERIES_IDENTITY_COLUMN, DataType::Utf8, false));
                schema.closed = true;
                Ok(())
            }
            QueryExpr::TimeRange { child, .. }
            | QueryExpr::Limit { child, .. }
            | QueryExpr::TimeShift { child, .. }
            | QueryExpr::PromqlSubquery { child, .. }
            | QueryExpr::PromqlScalarFromVector(child) => visit(Rc::make_mut(child)),
            // Constants read no series.
            QueryExpr::PromqlScalarBridge(_) => Ok(()),
            QueryExpr::PromqlVectorFromScalar(child)
                if super::row_values::scalar_literal(child).is_some() =>
            {
                Ok(())
            }
            QueryExpr::BinaryOp {
                op: planner_types::pre_asap::BinaryOpKind::Arithmetic(_),
                lhs,
                rhs,
                vector_match,
            } if vector_match.as_ref().is_none_or(|m| m.grouping.is_none())
                && ![&*lhs, &*rhs].into_iter().any(|side| {
                    matches!(
                        side.as_ref(),
                        QueryExpr::PromqlScalarFromVector(_) | QueryExpr::EvalTimestamp
                    )
                }) =>
            {
                visit(Rc::make_mut(lhs))?;
                visit(Rc::make_mut(rhs))
            }
            QueryExpr::Aggregate { child, .. } => visit(Rc::make_mut(child)),
            QueryExpr::Sort {
                child,
                partition_by,
                ..
            } => {
                if partition_by.is_without() {
                    return Err(invalid(
                        "dynamic without ranking requires label-set projection",
                    ));
                }
                visit(Rc::make_mut(child))
            }
            _ => Err(invalid(
                "operator has no dynamic series-identity realization",
            )),
        }
    }
    visit(&mut root)?;
    root.output_schema()
        .map_err(|error| invalid(error.to_string()))?;
    Ok(root)
}

/// Construct source rows only from full identities. The named label columns
/// are projections of that same identity and cannot independently redefine it.
pub fn series_row(
    schema: &Schema,
    labels: &BTreeMap<String, String>,
    timestamp: i64,
    value: f64,
) -> Result<Vec<crate::values::Value>, Error> {
    use crate::values::Value;
    let identity = encode_series_identity(labels)?;
    let mut found = false;
    let row = schema
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            if field.name == SERIES_IDENTITY_COLUMN {
                if field.dtype != SummaryFamilyType::Plain(DataType::Utf8)
                    || field.nullable
                    || found
                {
                    return Err(invalid("invalid series identity column"));
                }
                found = true;
                Ok(Value::Utf8(identity.clone().into()))
            } else if Some(index) == schema.time_index {
                Ok(Value::Timestamp(timestamp))
            } else if field.name == "value"
                && field.dtype == SummaryFamilyType::Plain(DataType::Float64)
            {
                Ok(Value::Float64(value))
            } else if field.dtype == SummaryFamilyType::Plain(DataType::Utf8) {
                Ok(labels.get(&field.name).map_or_else(
                    || Value::Utf8("".into()),
                    |value| Value::Utf8(value.clone().into()),
                ))
            } else {
                Err(invalid("unsupported PromQL source column"))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !found {
        return Err(invalid("source lacks its full series identity"));
    }
    Ok(row)
}

/// Compile the selected TopK computation above an existing maintained-population
/// source. The boundary supplies the complete eligible vector, not a truncated
/// TopK result; ranking remains a native physical operator.
pub fn compile_current_series_readout(
    selected: &Rc<planner_types::post_asap::SummaryNode>,
) -> Result<CompiledPhysicalDag, Error> {
    use planner_types::post_asap::{
        compile_post_asap_dag, maintained_population::PopulationReadout, SummaryField,
    };
    let mut dag = compile_post_asap_dag(selected).map_err(|error| invalid(error.to_string()))?;
    // Typed snapshot candidates already carry full identity throughout the DAG.
    // Cut at the population output, preserving all selected heap/readout nodes.
    let populations = dag.nodes.iter().filter(|node| matches!(&node.payload,
        Payload::Value { operation: ValueOperation::MaintainPopulation { population } }
            if matches!(population.input, planner_types::post_asap::maintained_population::PopulationInput::CurrentSeries(_))
    )).collect::<Vec<_>>();
    if let [population] = populations.as_slice() {
        if population
            .output_schema
            .fields
            .iter()
            .any(|field| field.name == SERIES_IDENTITY_COLUMN)
        {
            return compile(
                &dag,
                BTreeMap::from([(
                    u64::from(population.id.0),
                    InputContract::bounded(Arc::new(population.output_schema.clone())),
                )]),
                &[u64::from(dag.root.0)],
            );
        }
    }
    if dag.nodes.len() != 3
        || !dag.nodes.iter().any(|node| {
            node.id == dag.root
                && matches!(
                    node.payload,
                    Payload::Value {
                        operation: ValueOperation::ReadPopulation {
                            readout: PopulationReadout::TopK { .. }
                        }
                    }
                )
        })
    {
        return Err(invalid(
            "expected one selected current-series TopK computation",
        ));
    }
    let mut frontier = None;
    for node in &mut dag.nodes {
        match &mut node.payload {
            Payload::Fallback { expression } => {
                *expression = with_series_identity(expression)?;
            }
            Payload::Value {
                operation: ValueOperation::MaintainPopulation { .. },
            } => {
                frontier = Some(u64::from(node.id.0));
            }
            Payload::Value {
                operation:
                    ValueOperation::ReadPopulation {
                        readout: PopulationReadout::TopK { .. },
                    },
            } => {}
            _ => return Err(invalid("unsupported current-series readout dependency")),
        }
        if node
            .output_schema
            .fields
            .iter()
            .any(|field| field.name == SERIES_IDENTITY_COLUMN)
        {
            return Err(invalid(
                "current-series input already has a physical identity column",
            ));
        }
        node.output_schema.fields.push(SummaryField {
            name: SERIES_IDENTITY_COLUMN.into(),
            dtype: SummaryFamilyType::Plain(DataType::Utf8),
            nullable: false,
        });
    }
    for edge in &mut dag.edges {
        edge.intermediate_schema = dag
            .nodes
            .iter()
            .find(|node| node.id == edge.producer)
            .unwrap()
            .output_schema
            .clone();
    }
    let frontier = frontier.ok_or_else(|| invalid("missing current-series population"))?;
    let schema = Arc::new(
        dag.nodes
            .iter()
            .find(|node| u64::from(node.id.0) == frontier)
            .unwrap()
            .output_schema
            .clone(),
    );
    compile(
        &dag,
        BTreeMap::from([(frontier, InputContract::bounded(schema))]),
        &[u64::from(dag.root.0)],
    )
}

/// Compile selected ranking or aggregation above an exact per-series Rate
/// readout. Deployments bind complete window readouts at this boundary;
/// the heap is rebuilt independently for each evaluation. This does not move
/// that frontier to ingestion time or authorize combining finalized rates.
pub fn compile_rate_ranking(
    selected: &Rc<planner_types::post_asap::SummaryNode>,
) -> Result<
    (
        Rc<planner_types::post_asap::SummaryNode>,
        CompiledPhysicalDag,
    ),
    Error,
> {
    use planner_types::post_asap::{
        compile_post_asap_dag_with_node_ids, ExactKind, SummaryExpr, SummaryNode,
    };
    fn frontier(node: &Rc<SummaryNode>) -> Option<Rc<SummaryNode>> {
        match &node.expr {
            SummaryExpr::ValueOperation {
                child,
                operation: ValueOperation::FinalizeExactAccumulator,
                timing: planner_types::post_asap::ExecutionTiming::QueryTime,
            } if matches!(&child.expr, SummaryExpr::SummaryAgg {
                    family: SummaryFamilyType::ExactAggregate(ExactKind::Rate, _),
                    reduction: planner_types::pre_asap::Reduction::PerEntity,
                    child: raw, ..
                } if matches!(&raw.expr, SummaryExpr::KeepPreAsap(expr) if matches!(expr.as_ref(), QueryExpr::TimeRange { .. }))) =>
            {
                Some(Rc::clone(node))
            }
            SummaryExpr::ValueOperation { child, .. } | SummaryExpr::SummaryAgg { child, .. } => {
                frontier(child)
            }
            SummaryExpr::SummaryEstimate { summary_input, .. } => frontier(summary_input),
            _ => None,
        }
    }
    let source = frontier(selected)
        .ok_or_else(|| invalid("ranking requires one exact per-series Rate frontier"))?;
    if !source
        .schema
        .fields
        .iter()
        .any(|field| field.name == SERIES_IDENTITY_COLUMN)
    {
        return Err(invalid("Rate ranking requires complete series identity"));
    }
    let compiled = compile_post_asap_dag_with_node_ids(selected)
        .map_err(|error| invalid(error.to_string()))?;
    let id = u64::from(
        compiled
            .node_ids
            .node_id(&source)
            .ok_or_else(|| invalid("missing Rate frontier"))?
            .0,
    );
    let program = compile(
        &compiled.dag,
        BTreeMap::from([(id, InputContract::bounded(Arc::new(source.schema.clone())))]),
        &[u64::from(compiled.dag.root.0)],
    )?;
    Ok((source, program))
}

/// The selected logical placement requires fresh aggregate state per closed window.
/// Compile both physical graphs before deployment chooses storage or scheduling.
/// The input is the complete collection of per-series exact counter states.
pub fn compile_fixed_window_rate_aggregation(
    selected: &Rc<planner_types::post_asap::SummaryNode>,
) -> Result<PhysicalCandidate, Error> {
    use planner_types::post_asap::{
        compile_post_asap_dag, ExactKind, ExecutionTiming, SketchAlgorithm,
    };
    let dag = compile_post_asap_dag(selected).map_err(|e| invalid(e.to_string()))?;
    let sources = dag
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                &n.payload,
                Payload::SummaryAgg {
                    family: SummaryFamilyType::ExactAggregate(ExactKind::Rate, _),
                    reduction: planner_types::pre_asap::Reduction::PerEntity,
                    ..
                }
            )
        })
        .collect::<Vec<_>>();
    let heaps = dag
        .nodes
        .iter()
        .filter(|n| {
            n.output_state.timing == ExecutionTiming::IngestionTime
                && match &n.payload {
                    Payload::SummaryAgg {
                        family: SummaryFamilyType::Sketch(kind, _),
                        ..
                    } => matches!(
                        kind.algorithm(),
                        SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap
                    ),
                    Payload::SummaryAgg {
                        family: SummaryFamilyType::ExactAggregate(ExactKind::Sum, _),
                        ..
                    } => true,
                    _ => false,
                }
        })
        .collect::<Vec<_>>();
    let ([source], [heap]) = (sources.as_slice(), heaps.as_slice()) else {
        return Err(invalid(
            "expected one selected fixed-window Rate aggregation",
        ));
    };
    if !source
        .output_schema
        .fields
        .iter()
        .any(|f| f.name == SERIES_IDENTITY_COLUMN)
    {
        return Err(invalid(
            "fixed-window Rate aggregation requires complete series identity",
        ));
    }
    compile_candidate(
        &dag,
        BTreeMap::from([(
            u64::from(source.id.0),
            InputContract::bounded(Arc::new(source.output_schema.clone())),
        )]),
        &[u64::from(dag.root.0)],
        &[u64::from(heap.id.0)],
    )
}
