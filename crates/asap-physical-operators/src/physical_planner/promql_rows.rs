//! A bounded PromQL source row carries the entire label set, not just labels
//! mentioned by the query. The source adapter owns this lossless encoding.
use super::*;
use planner_types::pre_asap::DataType;
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

/// Resolve the row representation before candidate search; see
/// [`planner_types::pre_asap::schema::with_promql_series_identity`].
pub fn with_series_identity(root: &QueryExpr) -> Result<QueryExpr, Error> {
    planner_types::pre_asap::schema::with_promql_series_identity(root).map_err(invalid)
}

/// Construct source rows only from full identities. The named label columns
/// are projections of that same identity and cannot independently redefine it.
pub fn series_row(
    schema: &SchemaRef,
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
                if field.dtype != FieldDataType::Plain(DataType::Utf8) || field.nullable || found {
                    return Err(invalid("invalid series identity column"));
                }
                found = true;
                Ok(Value::Utf8(identity.clone().into()))
            } else if Some(index) == schema.time_index {
                Ok(Value::Timestamp(timestamp))
            } else if field.name == "value"
                && field.dtype == FieldDataType::Plain(DataType::Float64)
            {
                Ok(Value::Float64(value))
            } else if field.dtype == FieldDataType::Plain(DataType::Utf8) {
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
) -> Result<CompiledPhysicalDAG, Error> {
    use planner_types::post_asap::{
        compile_post_asap_dag, maintained_population::PopulationStatistic, Field,
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
                            readout: PopulationStatistic::TopK { .. }
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
                        readout: PopulationStatistic::TopK { .. },
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
        node.output_schema.fields.push(Field {
            table: None,
            name: SERIES_IDENTITY_COLUMN.into(),
            dtype: FieldDataType::Plain(DataType::Utf8),
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
        CompiledPhysicalDAG,
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
                    family: FieldDataType::ExactAggregate(ExactKind::Rate, _),
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

/// Compile a lifecycle-timed DAG whose heap or grouped Sum over per-series
/// Rate readouts runs at ingestion time: fresh aggregate state per closed
/// window. The input is the complete collection of per-series counter states.
pub fn compile_fixed_window_rate_aggregation(
    dag: &planner_types::post_asap::PostAsapDAG,
) -> Result<PhysicalASAPDAG, Error> {
    use planner_types::post_asap::{ExactKind, ExecutionTiming, SketchAlgorithm};
    let sources = dag
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                &n.payload,
                Payload::SummaryAgg {
                    family: FieldDataType::ExactAggregate(ExactKind::Rate, _),
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
                        family: FieldDataType::Sketch(kind, _),
                        ..
                    } => matches!(
                        kind.algorithm(),
                        SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap
                    ),
                    Payload::SummaryAgg {
                        family: FieldDataType::ExactAggregate(ExactKind::Sum, _),
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
        dag,
        BTreeMap::from([(
            u64::from(source.id.0),
            InputContract::bounded(Arc::new(source.output_schema.clone())),
        )]),
        &[u64::from(dag.root.0)],
        &[u64::from(heap.id.0)],
    )
}
