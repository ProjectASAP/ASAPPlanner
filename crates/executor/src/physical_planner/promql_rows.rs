//! A bounded PromQL source row carries the entire label set, not just labels
//! mentioned by the query. The source adapter owns this lossless encoding.
// Some helpers here serve only the Planner's own tests (`test-support`).
#![cfg_attr(
    not(any(test, feature = "test-support")),
    allow(dead_code, unused_imports)
)]
use super::*;
use planner_types::ir::physical_export::{
    compile_physical_asap_dag, compile_physical_asap_dag_with_node_ids,
};
use planner_types::ir::schema::DataType;
use planner_types::ir::schema::FieldDataType as SummaryFamilyType;
use planner_types::ir::ASAPOp;
use planner_types::ir::NonASAPOp;
use std::rc::Rc;

/// Not a legal PromQL label name, so it cannot shadow a user label.
pub use planner_types::ir::schema::PROMQL_SERIES_IDENTITY as SERIES_IDENTITY_COLUMN;

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
/// [`planner_types::ir::schema::schema_support::with_promql_series_identity`].
pub fn with_series_identity(root: &Rc<OperatorNode>) -> Result<Rc<OperatorNode>, Error> {
    planner_types::ir::schema::schema_support::with_promql_series_identity(root).map_err(invalid)
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

/// What a PromQL source column holds, as [`series_row`] fills it.
enum SeriesColumn {
    /// The series identity, [`encode_series_identity`] of all labels.
    Identity,
    /// The sample timestamp, in milliseconds.
    Timestamp,
    /// The sample value.
    SampleValue,
    /// One label's value; empty when the series lacks it.
    Label,
}

/// The columns of `samples` in `schema`: the batch [`series_row`] builds row by row, with each series'
/// identity and label cells computed once per distinct label set and stored
/// dictionary encoded. The batch is labelled `output`, which must have the
/// same columns. Bad input as for [`series_row`].
pub(crate) fn series_batch(
    schema: &SchemaRef,
    output: SchemaRef,
    samples: &[&crate::Sample],
) -> Result<crate::values::Batch, Error> {
    use crate::values::{Batch, Value};
    use arrow::array::{
        ArrayRef, DictionaryArray, Float64Array, StringArray, TimestampMillisecondArray,
        UInt32Array,
    };
    use planner_types::ir::schema::FieldDataType;
    if samples.is_empty() {
        return Batch::try_new(output, vec![]);
    }
    // Series ids in order of first appearance.
    let mut series_ids: ahash::AHashMap<&BTreeMap<String, String>, u32> = ahash::AHashMap::new();
    let mut series_cells: Vec<Vec<Value>> = Vec::new();
    let mut sample_series = Vec::with_capacity(samples.len());
    for sample in samples {
        let labels = &sample.labels;
        let next = series_cells.len() as u32;
        let id = match series_ids.get(labels) {
            Some(&id) => id,
            None => {
                series_cells.push(series_row(schema, labels, 0, 0.)?);
                series_ids.insert(labels, next);
                next
            }
        };
        sample_series.push(id);
    }
    let kinds = schema
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            if field.name == SERIES_IDENTITY_COLUMN {
                SeriesColumn::Identity
            } else if Some(index) == schema.time_index {
                SeriesColumn::Timestamp
            } else if field.name == "value"
                && field.dtype == FieldDataType::Plain(DataType::Float64)
            {
                SeriesColumn::SampleValue
            } else {
                SeriesColumn::Label
            }
        })
        .collect::<Vec<_>>();
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(kinds.len());
    for (index, kind) in kinds.iter().enumerate() {
        arrays.push(match kind {
            SeriesColumn::Timestamp => Arc::new(TimestampMillisecondArray::from_iter_values(
                samples.iter().map(|sample| sample.timestamp_ms),
            )),
            SeriesColumn::SampleValue => Arc::new(Float64Array::from_iter_values(
                samples.iter().map(|sample| sample.value),
            )),
            SeriesColumn::Identity | SeriesColumn::Label => {
                // Distinct texts of this column, and each series' code.
                let mut codes: ahash::AHashMap<&str, u32> = ahash::AHashMap::new();
                let mut texts: Vec<&str> = Vec::new();
                let series_codes = series_cells
                    .iter()
                    .map(|cells| {
                        let Value::Utf8(text) = &cells[index] else {
                            unreachable!("series_row fills labels with text");
                        };
                        let next = texts.len() as u32;
                        *codes.entry(text.as_ref()).or_insert_with(|| {
                            texts.push(text.as_ref());
                            next
                        })
                    })
                    .collect::<Vec<_>>();
                let keys = UInt32Array::from_iter_values(
                    sample_series.iter().map(|&id| series_codes[id as usize]),
                );
                Arc::new(
                    DictionaryArray::try_new(keys, Arc::new(StringArray::from(texts)))
                        .map_err(|error| invalid(error.to_string()))?,
                )
            }
        });
    }
    let states = vec![None; arrays.len()];
    Batch::from_parts(output, arrays, states, samples.len())
}

/// Compile the selected TopK computation above an existing maintained-population
/// source. The boundary supplies the complete eligible vector, not a truncated
/// TopK result; ranking remains a native physical operator.
pub fn compile_current_series_evaluation(
    selected: &Rc<OperatorNode>,
) -> Result<CompiledPhysicalDAG, Error> {
    use planner_types::ir::operator::maintained_population::PopulationStatistic;
    use planner_types::ir::schema::Field as SummaryField;
    // This compiler emits maintained precompute: every sub-DAG `MatAtIngestion`.
    let selected = planner_types::ir::apply_materialization_timings(
        selected,
        &planner_types::ir::MaterializationAssignment::with_default(
            planner_types::physical::Materialization::MatAtIngestion(
                planner_types::physical::Medium::Memory,
            ),
        ),
        &mut planner_types::ir::TimingMemo::new(),
    )
    .map_err(|e| invalid(e.to_string()))?;
    let mut dag =
        compile_physical_asap_dag(&selected).map_err(|error| invalid(error.to_string()))?;
    // Typed snapshot candidates already carry full identity throughout the DAG.
    // Cut at the population output, preserving all selected heap/evaluation nodes.
    let populations = dag.nodes.iter().filter(|node| matches!(&node.payload,
        Payload::ASAP(ASAPOp::MaintainPopulation { population, .. })
            if matches!(population.input, planner_types::ir::operator::maintained_population::PopulationInput::CurrentSeries(_))
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
                    population.id as u64,
                    InputContract::bounded(Arc::new(population.output_schema.clone())),
                )]),
                &dag.roots.iter().map(|r| *r as u64).collect::<Vec<_>>(),
            );
        }
    }
    let mut frontier = None;
    for node in &mut dag.nodes {
        match &mut node.payload {
            Payload::NonASAP(operator) => {
                if let NonASAPOp::Scan { schema, .. } = operator {
                    schema.fields.push(SummaryField::new(
                        SERIES_IDENTITY_COLUMN,
                        SummaryFamilyType::Plain(DataType::Utf8),
                        false,
                    ));
                    schema.closed = true;
                }
            }
            Payload::ASAP(ASAPOp::MaintainPopulation { .. }) => {
                frontier = Some(node.id as u64);
            }
            Payload::ASAP(ASAPOp::EvaluatePopulation {
                evaluation: PopulationStatistic::TopK { .. },
                ..
            }) => {}
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
            .find(|node| node.id as u64 == frontier)
            .unwrap()
            .output_schema
            .clone(),
    );
    compile(
        &dag,
        BTreeMap::from([(frontier, InputContract::bounded(schema))]),
        &dag.roots.iter().map(|r| *r as u64).collect::<Vec<_>>(),
    )
}

/// Compile selected ranking or aggregation above an exact per-series Rate
/// evaluation. Deployments bind complete window evaluations at this boundary;
/// the heap is rebuilt independently for each evaluation. This does not move
/// that frontier to ingestion time or authorize combining finalized rates.
pub fn compile_rate_ranking(
    selected: &Rc<OperatorNode>,
) -> Result<(Rc<OperatorNode>, CompiledPhysicalDAG), Error> {
    use planner_types::ir::schema::ExactKind;
    fn frontier(node: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
        if matches!(&node.operator, LogicalOperator::ASAP(ASAPOp::FinalizeExactAccumulator { child })
            if matches!(&child.operator, LogicalOperator::ASAP(ASAPOp::SummaryAgg {
                family: FieldDataType::ExactAggregate(ExactKind::Rate, _),
                reduction: planner_types::ir::operator::Reduction::PerEntity, child: raw, ..
            }) if matches!(raw.non_asap(), Some(NonASAPOp::TimeRange { .. }))))
        {
            return Some(Rc::clone(node));
        }
        node.children().into_iter().find_map(frontier)
    }
    // This compiler emits maintained precompute: every sub-DAG `MatAtIngestion`.
    let selected = planner_types::ir::apply_materialization_timings(
        selected,
        &planner_types::ir::MaterializationAssignment::with_default(
            planner_types::physical::Materialization::MatAtIngestion(
                planner_types::physical::Medium::Memory,
            ),
        ),
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
    let id = compiled
        .node_ids
        .node_id(&source)
        .ok_or_else(|| invalid("missing Rate frontier"))? as u64;
    let program = compile(
        &compiled.dag,
        BTreeMap::from([(id, InputContract::bounded(Arc::new(source.schema.clone())))]),
        &compiled
            .dag
            .roots
            .iter()
            .map(|r| *r as u64)
            .collect::<Vec<_>>(),
    )?;
    Ok((source, program))
}

/// Compile a materialization-timed DAG whose heap or grouped Sum over per-series
/// Rate evaluations runs at ingestion time: fresh aggregate state per closed
/// window. The input is the complete collection of per-series counter states.
pub fn compile_fixed_window_rate_aggregation(
    dag: &planner_types::ir::physical_export::PhysicalASAPDAG,
) -> Result<CompiledPhysicalPlan, Error> {
    use planner_types::ir::properties::ExecutionTiming;
    use planner_types::ir::schema::{ExactKind, SketchAlgorithm};
    let sources = dag
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                &n.payload,
                Payload::ASAP(ASAPOp::SummaryAgg {
                    family: SummaryFamilyType::ExactAggregate(ExactKind::Rate, _),
                    reduction: planner_types::ir::operator::Reduction::PerEntity,
                    ..
                })
            )
        })
        .collect::<Vec<_>>();
    let heaps = dag
        .nodes
        .iter()
        .filter(|n| {
            n.output_state.timing == ExecutionTiming::IngestionTime
                && match &n.payload {
                    Payload::ASAP(ASAPOp::SummaryAgg {
                        family: SummaryFamilyType::Sketch(kind, _),
                        ..
                    }) => matches!(
                        kind.algorithm(),
                        SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap
                    ),
                    Payload::ASAP(ASAPOp::SummaryAgg {
                        family: SummaryFamilyType::ExactAggregate(ExactKind::Sum, _),
                        ..
                    }) => true,
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
            source.id as u64,
            InputContract::bounded(Arc::new(source.output_schema.clone())),
        )]),
        &dag.roots.iter().map(|r| *r as u64).collect::<Vec<_>>(),
        &[heap.id as u64],
    )
}
