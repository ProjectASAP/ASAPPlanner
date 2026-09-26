//! Lower a selected temporal maintenance contract; deployment supplies readers.
use super::*;
use planner_types::post_asap::{
    EvaluationSchedule, OutputRepresentation, PaneLayout, SketchAlgorithm,
    SummaryMaintenanceLifecycle, SummaryMaintenanceLifecycleGuarantee, SummaryMaintenanceMode,
    SummaryWindowFramework,
};

/// Resolved source identity, supplied with physical capability evidence.
/// A schemaless PromQL projection cannot establish the complete label set.
#[derive(Clone, Debug)]
pub enum TemporalEntityIdentity {
    /// The input resolver guarantees that the slot contains one entity.
    SingleEntity,
    /// All entity keys are represented by these columns; there are no hidden
    /// labels distinguishing two rows with the same key.
    Columns(Vec<usize>),
}

/// Planner-selected lifecycle/window requirements for one temporal producer.
/// Pane geometry is semantic input, not a storage identity or scheduling policy.
#[derive(Clone, Debug)]
pub struct TemporalPaneMaintenance {
    pub summary_node: NodeId,
    pub lifecycle: SummaryMaintenanceLifecycleGuarantee,
    pub framework: SummaryWindowFramework,
    pub layout: PaneLayout,
    pub entity_identity: TemporalEntityIdentity,
}

/// Generated maintenance and query computation. `pane_inputs` is ordered from
/// the oldest complete pane to the newest; each run checks actual timestamps.
#[derive(Clone)]
pub struct TemporalPaneCandidate {
    pub physical: PhysicalCandidate,
    pub maintenance: TemporalPaneMaintenance,
    pub pane_inputs: Vec<NodeId>,
    pub merged_state: NodeId,
    pub window_width_ms: u64,
}

/// Compile bounded pane construction and a shared pane merge for temporal KLL
/// quantile roots. The selected contract remains authoritative; unsupported
/// lifecycle/framework/operator shapes fail rather than being substituted.
/// This initial realization consumes complete pane populations and emits full
/// state snapshots. Cross-run delta accumulation belongs to other candidates.
pub fn compile_temporal_pane_candidate(
    dag: &ExecutableDag,
    inputs: BTreeMap<NodeId, InputContract>,
    roots: &[NodeId],
    maintenance: &TemporalPaneMaintenance,
) -> Result<TemporalPaneCandidate, Error> {
    dag.validate().map_err(|error| invalid(error.to_string()))?;
    if maintenance.lifecycle.summary_maintenance_lifecycle
        != SummaryMaintenanceLifecycle::ContinuouslyMaintained
        || maintenance.lifecycle.summary_maintenance_mode != SummaryMaintenanceMode::Incremental
        || maintenance.lifecycle.evaluation_schedule != EvaluationSchedule::PerUpdate
        || maintenance.lifecycle.output_representation != OutputRepresentation::SummaryState
    {
        return Err(invalid(
            "pane candidate requires continuous incremental summary maintenance",
        ));
    }
    let build = dag
        .nodes
        .iter()
        .find(|node| u64::from(node.id.0) == maintenance.summary_node)
        .ok_or_else(|| invalid("unknown maintained producer"))?;
    let Payload::SummaryAgg {
        family,
        input: update,
        reduction: PlannerReduction::PerEntity,
        grouping,
    } = &build.payload
    else {
        return Err(invalid(
            "pane candidate requires a temporal per-entity summary",
        ));
    };
    if !matches!(family, SummaryFamilyType::Sketch(kind, _) if kind.algorithm() == &SketchAlgorithm::Kll)
        || update.item.is_some()
    {
        return Err(invalid("pane candidate supports unkeyed temporal KLL only"));
    }
    crate::capability::validate_summary_kernel(family, update, grouping).map_err(Error::Invalid)?;
    let dependencies: Vec<_> = dag
        .edges
        .iter()
        .filter(|edge| edge.consumer == build.id)
        .map(|edge| edge.producer)
        .collect();
    let [raw_id] = dependencies.as_slice() else {
        return Err(invalid("temporal producer requires one raw input"));
    };
    let raw = dag
        .nodes
        .iter()
        .find(|node| node.id == *raw_id)
        .ok_or_else(|| invalid("missing raw input"))?;
    let Payload::Fallback {
        expression: QueryExpr::TimeRange { range, child },
    } = &raw.payload
    else {
        return Err(invalid(
            "temporal producer requires an explicit logical time range",
        ));
    };
    let QueryExpr::Scan { predicates, .. } = child.as_ref() else {
        return Err(invalid("temporal pane source requires a raw scan"));
    };
    let window_width_ms: u64 = range
        .as_millis()
        .try_into()
        .map_err(|_| invalid("temporal window overflows"))?;
    if window_width_ms == 0
        || window_width_ms > i64::MAX as u64
        || range.subsec_nanos() % 1_000_000 != 0
    {
        return Err(invalid(
            "temporal window requires positive integral milliseconds",
        ));
    }
    let width = maintenance.layout.pane_width_ms;
    if width == 0 || width > window_width_ms || !window_width_ms.is_multiple_of(width) {
        return Err(invalid("temporal window must contain whole panes"));
    }
    match maintenance.framework {
        SummaryWindowFramework::Sliding => {}
        SummaryWindowFramework::Tumbling if width == window_width_ms => {}
        _ => return Err(invalid("unsupported temporal window realization")),
    }
    let count = window_width_ms / width;
    if count > 4096 {
        return Err(invalid("temporal pane candidate exceeds input budget"));
    }
    let raw_id = u64::from(raw_id.0);
    if inputs.len() != 1 {
        return Err(invalid(
            "pane candidate requires exactly its raw input contract",
        ));
    }
    let contract = inputs
        .get(&raw_id)
        .ok_or_else(|| invalid("missing raw input contract"))?;
    let raw_schema = Arc::new(raw.output_schema.clone());
    if contract.schema != raw_schema || contract.properties.boundedness != Boundedness::Bounded {
        return Err(invalid("pane source requires its declared bounded schema"));
    }
    let coordinate = raw_schema
        .time_index
        .ok_or_else(|| invalid("temporal source requires a time index"))?;
    let SummaryInputExpr::Column(value) = &update.weight else {
        return Err(invalid("pane builder requires a value column"));
    };
    let value = named_column(&raw_schema, value)?;
    let groups: Vec<_> = (0..raw_schema.fields.len())
        .filter(|&index| index != coordinate && index != value)
        .collect();
    match &maintenance.entity_identity {
        TemporalEntityIdentity::SingleEntity if groups.is_empty() => {}
        TemporalEntityIdentity::Columns(columns)
            if !columns.is_empty()
                && columns.len() == columns.iter().collect::<BTreeSet<_>>().len()
                && columns.iter().copied().collect::<BTreeSet<_>>()
                    == groups.iter().copied().collect() => {}
        _ => {
            return Err(invalid(
                "pane input requires its complete resolved entity identity",
            ))
        }
    }
    let mut next = dag
        .nodes
        .iter()
        .map(|node| u64::from(node.id.0))
        .max()
        .unwrap_or(0)
        + 1;
    let mut allocate = || {
        let id = next;
        next += 1;
        id
    };
    let mut operators = BTreeMap::new();
    let guard = allocate();
    operators.insert(
        guard,
        (
            vec![raw_id],
            Operator::pane_input(
                raw_schema.clone(),
                coordinate,
                maintenance.layout.clone(),
                None,
            )?,
        ),
    );
    let mut previous = guard;
    for predicate in predicates {
        let id = allocate();
        operators.insert(
            id,
            (
                vec![previous],
                Operator::filter(raw_schema.clone(), expression(&predicate.0, &raw_schema)?)?,
            ),
        );
        previous = id;
    }
    let native =
        Operator::summary_build(raw_schema, family.clone(), value, Some(coordinate), groups)?;
    let compact_state = native.schema();
    let native_id = allocate();
    operators.insert(native_id, (vec![previous], native));
    let state_schema = Arc::new(build.output_schema.clone());
    let pane_output = allocate();
    operators.insert(
        pane_output,
        (
            vec![native_id],
            Operator::scope_timestamp(compact_state, state_schema.clone())?,
        ),
    );
    let precompute = CompiledPhysicalDag::from_operators(inputs, operators, vec![pane_output])?;
    let state_coordinate = state_schema
        .time_index
        .ok_or_else(|| invalid("pane state requires a time index"))?;
    let state_column = summary_column(&state_schema)?;
    let mut query_inputs = BTreeMap::new();
    let mut operators = BTreeMap::new();
    let mut pane_inputs = Vec::new();
    let mut guarded_inputs = Vec::new();
    for pane in 0..count {
        let input = allocate();
        let guard = allocate();
        query_inputs.insert(input, InputContract::bounded(state_schema.clone()));
        let offset = ((count - 1 - pane) * width) as i64;
        operators.insert(
            guard,
            (
                vec![input],
                Operator::pane_input(
                    state_schema.clone(),
                    state_coordinate,
                    maintenance.layout.clone(),
                    Some(offset),
                )?,
            ),
        );
        pane_inputs.push(input);
        guarded_inputs.push(guard);
    }
    let union = allocate();
    operators.insert(
        union,
        (
            guarded_inputs,
            Operator::union(state_schema.clone(), count as usize)?,
        ),
    );
    let merge = Operator::summary_merge(
        state_schema.clone(),
        state_column,
        (0..state_schema.fields.len())
            .filter(|&index| index != state_coordinate && index != state_column)
            .collect(),
    )?;
    let merged_schema = merge.schema();
    let merged_state = allocate();
    operators.insert(merged_state, (vec![union], merge));
    if roots.is_empty() || roots.iter().copied().collect::<BTreeSet<_>>().len() != roots.len() {
        return Err(invalid("temporal query requires distinct output roots"));
    }
    for &root in roots {
        let node = dag
            .nodes
            .iter()
            .find(|node| u64::from(node.id.0) == root)
            .ok_or_else(|| invalid("unknown temporal output root"))?;
        let Payload::SummaryEstimate {
            query: SketchQuery::Quantile { q },
        } = node.payload
        else {
            return Err(invalid("temporal pane root must be a KLL quantile"));
        };
        if !q.is_finite() || !(0. ..=1.).contains(&q) {
            return Err(invalid("invalid temporal quantile"));
        }
        let dependencies: Vec<_> = dag
            .edges
            .iter()
            .filter(|edge| edge.consumer == node.id)
            .map(|edge| u64::from(edge.producer.0))
            .collect();
        if dependencies != [maintenance.summary_node] {
            return Err(invalid(
                "temporal readout must consume the maintained producer",
            ));
        }
        let readout = Operator::readout(
            merged_schema.clone(),
            summary_column(&merged_schema)?,
            crate::Statistic::Quantile,
            std::collections::HashMap::from([("quantile".into(), q.to_string())]),
        )?;
        let readout_schema = readout.schema();
        let readout_id = allocate();
        operators.insert(readout_id, (vec![merged_state], readout));
        operators.insert(
            root,
            (
                vec![readout_id],
                Operator::scope_timestamp(readout_schema, Arc::new(node.output_schema.clone()))?,
            ),
        );
    }
    let query = CompiledPhysicalDag::from_operators(query_inputs, operators, roots.to_vec())?;
    let mut output = precompute.output_contract(pane_output)?;
    // Persisted readers have independent timing from the blocking builder.
    output.properties.emission = Emission::Unknown;
    Ok(TemporalPaneCandidate {
        physical: PhysicalCandidate {
            precompute: Some(precompute),
            query,
            materialized_outputs: BTreeMap::from([(pane_output, output)]),
        },
        maintenance: maintenance.clone(),
        pane_inputs,
        merged_state,
        window_width_ms,
    })
}
