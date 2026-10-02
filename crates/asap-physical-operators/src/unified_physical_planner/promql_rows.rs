//! A bounded PromQL source row carries the entire label set, not just labels
//! mentioned by the query. The source adapter owns this lossless encoding.
use super::*;
use planner_types::ir::export::{
    compile_physical_asap_dag, compile_physical_asap_dag_with_node_ids,
};
use planner_types::post_asap::FieldDataType as SummaryFamilyType;
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
/// [`planner_types::ir::schema_support::with_promql_series_identity`].
pub fn with_series_identity(root: &Rc<OperatorNode>) -> Result<Rc<OperatorNode>, Error> {
    planner_types::ir::schema_support::with_promql_series_identity(root).map_err(invalid)
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
pub fn compile_current_series_evaluation(
    selected: &Rc<OperatorNode>,
) -> Result<CompiledPhysicalDAG, Error> {
    use planner_types::post_asap::{
        maintained_population::PopulationStatistic, Field as SummaryField,
    };
    let selected = planner_types::ir::apply_lifecycle_timings(
        selected,
        &planner_types::ir::LifecycleAssignment::default_maintained(),
        &mut planner_types::ir::TimingMemo::new(),
    )
    .map_err(|e| invalid(e.to_string()))?;
    let mut dag =
        compile_physical_asap_dag(&selected).map_err(|error| invalid(error.to_string()))?;
    // Typed snapshot candidates already carry full identity throughout the DAG.
    // Cut at the population output, preserving all selected heap/evaluation nodes.
    let populations = dag.nodes.iter().filter(|node| matches!(&node.payload,
        Payload::MaintainPopulation { population }
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
    let mut frontier = None;
    for node in &mut dag.nodes {
        match &mut node.payload {
            Payload::Relational { operator } => {
                if let NonASAPOpKind::Scan { schema, .. } = operator {
                    schema.fields.push(SummaryField::new(
                        SERIES_IDENTITY_COLUMN,
                        SummaryFamilyType::Plain(DataType::Utf8),
                        false,
                    ));
                    schema.closed = true;
                }
            }
            Payload::MaintainPopulation { .. } => {
                frontier = Some(u64::from(node.id.0));
            }
            Payload::EvaluatePopulation {
                evaluation: PopulationStatistic::TopK { .. },
            } => {}
            _ => return Err(invalid("unsupported current-series evaluation dependency")),
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
            table: None,
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
/// evaluation. Deployments bind complete window evaluations at this boundary;
/// the heap is rebuilt independently for each evaluation. This does not move
/// that frontier to ingestion time or authorize combining finalized rates.
pub fn compile_rate_ranking(
    selected: &Rc<OperatorNode>,
) -> Result<(Rc<OperatorNode>, CompiledPhysicalDAG), Error> {
    use planner_types::post_asap::ExactKind;
    fn frontier(node: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
        if matches!(&node.operator, LogicalOperator::ASAP(ASAPOp::FinalizeExactAccumulator { child })
            if matches!(&child.operator, LogicalOperator::ASAP(ASAPOp::SummaryAgg {
                family: FieldDataType::ExactAggregate(ExactKind::Rate, _),
                reduction: planner_types::pre_asap::Reduction::PerEntity, child: raw, ..
            }) if matches!(raw.non_asap(), Some(NonASAPOp::TimeRange { .. }))))
        {
            return Some(Rc::clone(node));
        }
        node.children().into_iter().find_map(frontier)
    }
    let selected = planner_types::ir::apply_lifecycle_timings(
        selected,
        &planner_types::ir::LifecycleAssignment::default_maintained(),
        &mut planner_types::ir::TimingMemo::new(),
    )
    .map_err(|e| invalid(e.to_string()))?;
    let source = frontier(&selected)
        .ok_or_else(|| invalid("ranking requires one exact per-series Rate frontier"))?;
    if !source
        .schema
        .fields
        .iter()
        .any(|field| field.name == SERIES_IDENTITY_COLUMN)
    {
        return Err(invalid("Rate ranking requires complete series identity"));
    }
    let compiled = compile_physical_asap_dag_with_node_ids(&selected)
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
/// Rate evaluations runs at ingestion time: fresh aggregate state per closed
/// window. The input is the complete collection of per-series counter states.
pub fn compile_fixed_window_rate_aggregation(
    dag: &planner_types::ir::export::PhysicalASAPDAG,
) -> Result<CompiledPhysicalPlan, Error> {
    use planner_types::post_asap::{ExactKind, ExecutionTiming, SketchAlgorithm};
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
        dag,
        BTreeMap::from([(
            u64::from(source.id.0),
            InputContract::bounded(Arc::new(source.output_schema.clone())),
        )]),
        &[u64::from(dag.root.0)],
        &[u64::from(heap.id.0)],
    )
}
