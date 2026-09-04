use super::*;

/// Physical evidence that is not represented by [`DataWorkload`] for one
/// incrementally maintained summary deployment. Window counts describe the
/// already-selected physical deployment; this layer does not define another
/// tumbling/sliding policy enum.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StreamingPhysicalInputEvidence {
    /// Logical bytes in the snapshot used to bootstrap the state.
    pub initial_input_bytes: u64,
    /// Source bytes read while bootstrapping. Arriving stream bytes are not a
    /// disk scan and are therefore excluded.
    pub initial_source_scan_bytes: u64,
    /// Simultaneously open windows receiving each arriving item.
    pub active_window_count: u64,
    /// Window/state partitions receiving each bootstrap row.
    pub bootstrap_window_count: u64,
    /// Completed windows retained for query coverage.
    pub retained_window_count: u64,
    /// Independent state instances per window: one for shared
    /// multi-subpopulation state, otherwise the resolved group count.
    pub physical_summary_count: u64,
    /// Resident bytes of one concrete state instance.
    pub state_bytes_per_summary: u64,
}

/// Workload-normalized inputs for incremental maintenance over one finite
/// comparison horizon.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StreamingSummaryInputs {
    pub initial_input_rows: u64,
    pub initial_input_bytes: u64,
    pub initial_source_scan_bytes: u64,
    pub ingestion_rate_per_second: f64,
    pub active_window_count: u64,
    pub bootstrap_window_count: u64,
    pub retained_window_count: u64,
    pub physical_summary_count: u64,
    pub state_bytes_per_summary: u64,
}

impl StreamingSummaryInputs {
    /// Resolve snapshot size, arriving rows, and reads from the canonical
    /// workload. Positive fractional expected work rounds up conservatively.
    ///
    /// `Mixed` fails closed because today's workload schema cannot distinguish
    /// its at-rest backlog from its continuing-arrival cardinality.
    pub fn from_workload(
        physical: StreamingPhysicalInputEvidence,
        data: &DataWorkload,
        scope: &ComparisonScope,
    ) -> Result<Self, AnalyticalCostError> {
        let _ = scope.validate()?;
        if data.arrival != DataArrival::ContinuouslyIngesting || scope.data_arrival != data.arrival
        {
            return Err(AnalyticalCostError::UnsupportedDataArrival(data.arrival));
        }
        let initial_input_rows = data
            .input_cardinality
            .value_at(scope.planning_time.0)
            .copied()
            .ok_or(AnalyticalCostError::MissingOrStale("input_cardinality"))?;
        let ingestion_rate = data
            .ingestion_rate
            .value_at(scope.planning_time.0)
            .copied()
            .ok_or(AnalyticalCostError::MissingOrStale("ingestion_rate"))?;
        if !ingestion_rate.0.is_finite() || ingestion_rate.0 < 0.0 {
            return Err(AnalyticalCostError::InvalidIngestionRate(ingestion_rate.0));
        }
        Self {
            initial_input_rows,
            initial_input_bytes: physical.initial_input_bytes,
            initial_source_scan_bytes: physical.initial_source_scan_bytes,
            ingestion_rate_per_second: ingestion_rate.0,
            active_window_count: physical.active_window_count,
            bootstrap_window_count: physical.bootstrap_window_count,
            retained_window_count: physical.retained_window_count,
            physical_summary_count: physical.physical_summary_count,
            state_bytes_per_summary: physical.state_bytes_per_summary,
        }
        .validate()
    }

    pub fn validate(self) -> Result<Self, AnalyticalCostError> {
        for (name, value) in [
            ("active_window_count", self.active_window_count),
            ("bootstrap_window_count", self.bootstrap_window_count),
            ("physical_summary_count", self.physical_summary_count),
            ("state_bytes_per_summary", self.state_bytes_per_summary),
        ] {
            if value == 0 {
                return Err(AnalyticalCostError::MissingOrZero(name));
            }
        }
        if (self.initial_input_rows == 0) != (self.initial_input_bytes == 0)
            || (self.initial_input_rows == 0 && self.initial_source_scan_bytes != 0)
        {
            return Err(AnalyticalCostError::InconsistentBootstrapEvidence);
        }
        if !self.ingestion_rate_per_second.is_finite() || self.ingestion_rate_per_second < 0.0 {
            return Err(AnalyticalCostError::InvalidIngestionRate(
                self.ingestion_rate_per_second,
            ));
        }
        Ok(self)
    }
}

/// CPU operations for one concrete state operation on one state instance.
/// Missing evidence is legal only when the selected summary DAG does not use
/// that operation.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SummaryOperationCpuEvidence {
    pub insert_cpu_ops: Option<f64>,
    pub merge_cpu_ops: Option<f64>,
    pub subtract_cpu_ops: Option<f64>,
    pub delete_cpu_ops: Option<f64>,
    /// Expirations/retractions routed to this DAG per second. Required only
    /// when an explicit `SummaryDelete` is present.
    pub delete_events_per_second: Option<f64>,
    /// Concrete state instances touched by one delete event.
    pub delete_routing_fanout: Option<u64>,
    pub readout_cpu_ops: Option<f64>,
}

/// Physical evidence for one `SummaryJoin` implementation. Total work,
/// cardinality, and memory cannot be inferred from the logical join key alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryJoinEvidence {
    pub physical_id: String,
    pub inputs: Vec<EdgeStatistics>,
    pub output: EdgeStatistics,
    /// Total build, probe, match-production, and output CPU for one complete
    /// execution of the selected physical join algorithm.
    pub cpu_ops_per_execution: f64,
    pub working_memory_bytes: u64,
    pub output_buffer_bytes: u64,
    pub executions_per_evaluation: u64,
    pub io_bytes_per_execution: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamingAggregateEvidence {
    pub physical_id: String,
    pub input: EdgeStatistics,
    pub output: EdgeStatistics,
    /// Index into `ComparisonScope.sources` when this state bootstraps directly
    /// from storage. `None` means its input is an already-materialized child
    /// edge and therefore has no additional source read.
    pub source_coverage_index: Option<usize>,
    /// Provider-owned identity of the physical bootstrap read. Equal source
    /// coverage alone does not prove two independent builds share I/O.
    pub bootstrap_read_identity: String,
    pub inputs: StreamingSummaryInputs,
    /// CPU operations to insert one routed row into one state instance.
    pub insert_cpu_ops: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SummaryOperatorResourceEvidence {
    pub physical_id: String,
    pub inputs: Vec<EdgeStatistics>,
    pub output: EdgeStatistics,
    pub cpu_ops: f64,
    pub working_memory_bytes: u64,
    pub output_buffer_bytes: u64,
    /// Executions of this physical operator for one query evaluation.
    /// This is provider evidence, not inferred from a descendant state.
    pub executions_per_evaluation: u64,
    pub io_bytes_per_execution: Option<u64>,
}

/// Evidence is structured by logical summary operation so delete-only facts
/// cannot be attached to merge, subtract, or readout nodes.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamingSummaryOperatorEvidence {
    /// Exact query-time arithmetic over two independently realized operands.
    Binary(SummaryOperatorResourceEvidence),
    Merge(SummaryOperatorResourceEvidence),
    Subtract(SummaryOperatorResourceEvidence),
    Delete {
        resource: SummaryOperatorResourceEvidence,
        events_per_second: f64,
        routing_fanout: u64,
    },
    Readout(SummaryOperatorResourceEvidence),
}

impl StreamingSummaryOperatorEvidence {
    pub(super) fn resource(&self) -> &SummaryOperatorResourceEvidence {
        match self {
            Self::Binary(resource)
            | Self::Merge(resource)
            | Self::Subtract(resource)
            | Self::Delete { resource, .. }
            | Self::Readout(resource) => resource,
        }
    }

    #[cfg(test)]
    pub(super) fn resource_mut(&mut self) -> &mut SummaryOperatorResourceEvidence {
        match self {
            Self::Binary(resource)
            | Self::Merge(resource)
            | Self::Subtract(resource)
            | Self::Delete { resource, .. }
            | Self::Readout(resource) => resource,
        }
    }
}

/// Non-aggregation work for a retained pre-ASAP subtree over the comparison
/// horizon. Bootstrap/source I/O belongs exclusively to the owning aggregate,
/// and summary insertion belongs exclusively to its insert evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamingRetainedQueryEvidence {
    pub physical_id: String,
    /// Logical output edge consumed by the parent summary operator.
    pub output: EdgeStatistics,
    pub preprocessing_cpu_ops_over_horizon: f64,
    /// Execution workspace, excluding the separately declared output buffer.
    pub working_memory_bytes: u64,
    pub output_buffer_bytes: u64,
}

/// Physical evidence bound to the selected DAG's `Rc` identity. A copied,
/// structurally equal node is not silently treated as the same deployment.
#[derive(Debug, Clone, Default)]
pub struct StreamingNodeEvidence {
    pub(super) aggregations: HashMap<*const SummaryNode, StreamingAggregateEvidence>,
    pub(super) joins: HashMap<*const SummaryNode, SummaryJoinEvidence>,
    pub(super) operations: HashMap<*const SummaryNode, StreamingSummaryOperatorEvidence>,
    pub(super) operation_state_owners: HashMap<*const SummaryNode, *const SummaryNode>,
    pub(super) retained_queries: HashMap<*const SummaryNode, StreamingRetainedQueryEvidence>,
}

impl StreamingNodeEvidence {
    pub fn insert_aggregation(
        &mut self,
        node: &Rc<SummaryNode>,
        evidence: StreamingAggregateEvidence,
    ) {
        self.aggregations.insert(Rc::as_ptr(node), evidence);
    }

    pub fn insert_join(&mut self, node: &Rc<SummaryNode>, evidence: SummaryJoinEvidence) {
        self.joins.insert(Rc::as_ptr(node), evidence);
    }

    pub fn insert_operation(
        &mut self,
        node: &Rc<SummaryNode>,
        evidence: StreamingSummaryOperatorEvidence,
    ) {
        self.operations.insert(Rc::as_ptr(node), evidence);
    }

    /// Bind a stateful operation (currently `SummaryDelete`) to the exact
    /// aggregation deployment whose active interval it follows.
    pub fn insert_state_operation(
        &mut self,
        node: &Rc<SummaryNode>,
        state: &Rc<SummaryNode>,
        evidence: StreamingSummaryOperatorEvidence,
    ) {
        self.operations.insert(Rc::as_ptr(node), evidence);
        self.operation_state_owners
            .insert(Rc::as_ptr(node), Rc::as_ptr(state));
    }

    pub fn insert_retained_query(
        &mut self,
        node: &Rc<SummaryNode>,
        evidence: StreamingRetainedQueryEvidence,
    ) {
        self.retained_queries.insert(Rc::as_ptr(node), evidence);
    }

    pub(super) fn aggregation(&self, node: &SummaryNode) -> Option<StreamingAggregateEvidence> {
        self.aggregations.get(&(node as *const _)).cloned()
    }
}

pub(super) fn summary_operation_evidence<'a>(
    node: &SummaryNode,
    evidence: &'a StreamingNodeEvidence,
) -> Result<&'a StreamingSummaryOperatorEvidence, AnalyticalCostError> {
    let operation = evidence
        .operations
        .get(&(node as *const _))
        .ok_or(AnalyticalCostError::MissingOrStale("summary operation"))?;
    let matches = matches!(
        (&node.expr, operation),
        (
            SummaryExpr::BinaryOp { .. },
            StreamingSummaryOperatorEvidence::Binary(_)
        ) | (
            SummaryExpr::SummaryMerge { .. },
            StreamingSummaryOperatorEvidence::Merge(_)
        ) | (
            SummaryExpr::SummarySubtract { .. },
            StreamingSummaryOperatorEvidence::Subtract(_)
        ) | (
            SummaryExpr::SummaryDelete { .. },
            StreamingSummaryOperatorEvidence::Delete { .. }
        ) | (
            SummaryExpr::SummaryEstimate { .. },
            StreamingSummaryOperatorEvidence::Readout(_)
        )
    );
    if matches {
        Ok(operation)
    } else {
        Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "summary operation evidence kind does not match SummaryExpr",
        ))
    }
}

/// Evidence for recomputing the raw target over the full comparison horizon.
/// Planning-time dimensions describe the initial snapshot. Each scheduled
/// evaluation adds arrivals since planning time; `physical_dag` is therefore
/// a once-counted DAG whose edge statistics already aggregate all evaluations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamingRawInputEvidence {
    pub planning_time_input_rows: u64,
    pub planning_time_input_bytes: u64,
    pub planning_time_source_scan_bytes: u64,
    /// Decoded logical bytes added to operator edges by one arriving row.
    pub arriving_logical_row_bytes: u64,
    /// Physical storage bytes read for one arriving row. Kept separate from
    /// logical width so compression and encoding are not silently conflated.
    pub arriving_source_row_bytes: u64,
    pub ingestion_rate_per_second: f64,
    pub physical_dag: EvidenceBackedPhysicalDag,
}

/// One complete provider-enumerated physical implementation of the selected
/// streaming summary DAG. The identifier is stable provenance; concrete
/// framework selection is performed by ranking these complete alternatives.
#[derive(Debug, Clone)]
pub struct StreamingPhysicalPlanAlternative {
    pub physical_plan_id: String,
    pub node_evidence: StreamingNodeEvidence,
}
