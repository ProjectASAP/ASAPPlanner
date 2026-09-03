use super::*;
/// Adapter that supplies the existing lifecycle planner with analytical
/// streaming costs. It does not define lifecycle policy: the planner's
/// existing enums and legality checks remain authoritative.
#[derive(Debug, Clone)]
pub struct SummaryMaintenanceCostModel {
    pub node_evidence: StreamingNodeEvidence,
    pub calibration: ResourceCalibration,
    pub capabilities: SummaryMaintenanceCapabilities,
    target_comparisons: HashMap<*const QueryExpr, StreamingTargetComparison>,
    candidate_comparisons: HashMap<CandidateComparisonKey, BoundCandidateIdentity>,
    physical_plan_alternatives:
        HashMap<CandidateComparisonKey, Vec<StreamingPhysicalPlanAlternative>>,
    window_framework_candidates:
        HashMap<CandidateComparisonKey, Vec<StreamingWindowFrameworkCandidate>>,
}

type CandidateComparisonKey = (*const QueryExpr, *const SummaryNode);

#[derive(Debug, Clone)]
struct BoundCandidateIdentity {
    _target: Rc<QueryExpr>,
    _root: Rc<SummaryNode>,
}

#[derive(Debug, Clone)]
struct StreamingTargetComparison {
    _target: Rc<QueryExpr>,
    scope: ComparisonScope,
    raw: StreamingRawInputEvidence,
}

type LogicalSourceSelection = (Source, Vec<Predicate>, Vec<InfoMatcher>);

fn deduplicate_source_selections(
    values: Vec<LogicalSourceSelection>,
) -> Vec<LogicalSourceSelection> {
    values.into_iter().fold(Vec::new(), |mut unique, value| {
        if !unique.contains(&value) {
            unique.push(value);
        }
        unique
    })
}

fn info_source(selector: &[InfoMatcher]) -> Result<Source, AnalyticalCostError> {
    let mut metric: Option<&str> = None;
    for matcher in selector
        .iter()
        .filter(|matcher| matcher.label == "__name__")
    {
        if matcher.op != CompareOpKind::Eq || metric.is_some_and(|value| value != matcher.value) {
            return Err(AnalyticalCostError::UnsupportedQueryOperator);
        }
        metric = Some(&matcher.value);
    }
    Ok(Source::TimeSeries {
        metric: metric.unwrap_or("target_info").into(),
    })
}

fn query_source_selections(
    query: &QueryExpr,
    out: &mut Vec<LogicalSourceSelection>,
) -> Result<(), AnalyticalCostError> {
    use QueryExpr::*;
    match query {
        Scan {
            source, predicates, ..
        } => out.push((source.clone(), predicates.clone(), vec![])),
        PromqlVectorFromScalar(child) | PromqlScalarFromVector(child) => {
            query_source_selections(child, out)?
        }
        PromqlInfoEnrich { selector, child } => {
            query_source_selections(child, out)?;
            out.push((info_source(selector)?, vec![], selector.clone()));
        }
        PromqlRelabel { child, .. }
        | Filter { child, .. }
        | Project { child, .. }
        | Aggregate { child, .. }
        | Dedup { child, .. }
        | PromqlSubquery { child, .. }
        | TimeRange { child, .. }
        | TimeShift { child, .. }
        | SQLWindowFunc { child, .. }
        | PromqlSeriesSample { child, .. }
        | Sort { child, .. }
        | Limit { child, .. } => query_source_selections(child, out)?,
        Concat { children } => {
            for child in children {
                query_source_selections(child, out)?;
            }
        }
        Join { left, right, .. } | SetOp { left, right, .. } => {
            query_source_selections(left, out)?;
            query_source_selections(right, out)?;
        }
        BinaryOp { lhs, rhs, .. } => {
            query_source_selections(lhs, out)?;
            query_source_selections(rhs, out)?;
        }
        PromqlScalarBridge(_)
        | EvalTimestamp
        | CurrentTimestamp
        | Column(_)
        | Literal(_)
        | Compare { .. }
        | BoolAnd(_)
        | BoolOr(_)
        | Not(_)
        | IsNull(_)
        | IsNotNull(_)
        | Cast { .. }
        | InList { .. }
        | FunctionCall { .. }
        | Arithmetic { .. }
        | Case { .. } => {}
    }
    Ok(())
}

fn validate_query_scope(
    target: &QueryExpr,
    scope: &ComparisonScope,
) -> Result<(), AnalyticalCostError> {
    let mut actual = Vec::new();
    query_source_selections(target, &mut actual)?;
    let actual = deduplicate_source_selections(actual);
    let mut declared: Vec<_> = scope
        .sources
        .iter()
        .map(|coverage| {
            (
                coverage.source.clone(),
                coverage.predicates.clone(),
                coverage.info_matchers.clone(),
            )
        })
        .collect();
    for selection in actual {
        let Some(index) = declared.iter().position(|value| value == &selection) else {
            return Err(AnalyticalCostError::ComparisonScopeMismatch(
                "raw target source lineage",
            ));
        };
        declared.swap_remove(index);
    }
    if !declared.is_empty() {
        return Err(AnalyticalCostError::ComparisonScopeMismatch(
            "raw target source lineage",
        ));
    }
    Ok(())
}

fn validate_physical_scope_coverage(
    physical: &EvidenceBackedPhysicalDag,
    scope: &ComparisonScope,
) -> Result<(), AnalyticalCostError> {
    let nodes = reachable_physical_nodes(physical)?;
    let mut covered = HashSet::new();
    for node in nodes
        .into_iter()
        .filter(|node| node.operator == PhysicalOperator::Scan)
    {
        let coverage = node
            .source_coverage
            .as_ref()
            .ok_or_else(|| AnalyticalCostError::MissingScanSourceCoverage(node.id.clone()))?;
        let Some(index) = scope.sources.iter().position(|value| value == coverage) else {
            return Err(AnalyticalCostError::ComparisonScopeMismatch(
                "physical source coverage",
            ));
        };
        covered.insert(index);
    }
    if covered.len() != scope.sources.len() {
        return Err(AnalyticalCostError::ComparisonScopeMismatch(
            "physical source coverage",
        ));
    }
    Ok(())
}

fn reachable_physical_nodes(
    physical: &EvidenceBackedPhysicalDag,
) -> Result<Vec<&PhysicalDagNode>, AnalyticalCostError> {
    let by_id: HashMap<_, _> = physical
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    if by_id.len() != physical.nodes.len() {
        return Err(AnalyticalCostError::InvalidPhysicalDag("duplicate node id"));
    }
    fn visit<'a>(
        id: &'a str,
        by_id: &HashMap<&'a str, &'a PhysicalDagNode>,
        visiting: &mut HashSet<&'a str>,
        visited: &mut HashSet<&'a str>,
        nodes: &mut Vec<&'a PhysicalDagNode>,
    ) -> Result<(), AnalyticalCostError> {
        if visited.contains(id) {
            return Ok(());
        }
        if !visiting.insert(id) {
            return Err(AnalyticalCostError::InvalidPhysicalDag("cycle"));
        }
        let node = by_id
            .get(id)
            .copied()
            .ok_or(AnalyticalCostError::InvalidPhysicalDag("missing node"))?;
        for child in &node.children {
            visit(child, by_id, visiting, visited, nodes)?;
        }
        visiting.remove(id);
        visited.insert(id);
        nodes.push(node);
        Ok(())
    }
    let mut nodes = Vec::new();
    visit(
        physical.root.as_str(),
        &by_id,
        &mut HashSet::new(),
        &mut HashSet::new(),
        &mut nodes,
    )?;
    Ok(nodes)
}

fn validate_raw_snapshot_dimensions(
    raw: &StreamingRawInputEvidence,
    scope: &ComparisonScope,
) -> Result<(), AnalyticalCostError> {
    if scope.sources.len() != 1 {
        return Err(AnalyticalCostError::MissingComparisonScope(
            "single-source raw evolution",
        ));
    }
    if !raw.ingestion_rate_per_second.is_finite() || raw.ingestion_rate_per_second < 0.0 {
        return Err(AnalyticalCostError::InvalidIngestionRate(
            raw.ingestion_rate_per_second,
        ));
    }
    let bootstrap_is_consistent = if raw.planning_time_input_rows == 0 {
        raw.planning_time_input_bytes == 0 && raw.planning_time_source_scan_bytes == 0
    } else {
        raw.planning_time_input_bytes > 0 && raw.planning_time_source_scan_bytes > 0
    };
    if !bootstrap_is_consistent
        || (raw.ingestion_rate_per_second > 0.0
            && (raw.arriving_logical_row_bytes == 0 || raw.arriving_source_row_bytes == 0))
    {
        return Err(AnalyticalCostError::InconsistentBootstrapEvidence);
    }
    let mut rows = 0_u64;
    let mut bytes = 0_u64;
    let mut scan = 0_u64;
    for offset in evaluation_offsets_ms(scope)? {
        let arrivals = (raw.ingestion_rate_per_second * offset as f64 / 1_000.0).ceil();
        if !arrivals.is_finite() || arrivals < 0.0 || arrivals > u64::MAX as f64 {
            return Err(AnalyticalCostError::Overflow);
        }
        let arrivals = arrivals as u64;
        rows = rows
            .checked_add(raw.planning_time_input_rows)
            .and_then(|value| value.checked_add(arrivals))
            .ok_or(AnalyticalCostError::Overflow)?;
        bytes = bytes
            .checked_add(raw.planning_time_input_bytes)
            .and_then(|value| {
                arrivals
                    .checked_mul(raw.arriving_logical_row_bytes)
                    .and_then(|arriving| value.checked_add(arriving))
            })
            .ok_or(AnalyticalCostError::Overflow)?;
        scan = scan
            .checked_add(raw.planning_time_source_scan_bytes)
            .and_then(|value| {
                arrivals
                    .checked_mul(raw.arriving_source_row_bytes)
                    .and_then(|arriving| value.checked_add(arriving))
            })
            .ok_or(AnalyticalCostError::Overflow)?;
    }
    let reachable = reachable_physical_nodes(&raw.physical_dag)?;
    if reachable
        .iter()
        .any(|node| node.execution != ExecutionMultiplicity::Once)
    {
        return Err(AnalyticalCostError::InvalidPhysicalDag(
            "streaming raw horizon evidence must use once-counted aggregate statistics",
        ));
    }
    let expected = EdgeStatistics { rows, bytes };
    let mut scan_count = 0;
    for scan_node in reachable
        .into_iter()
        .filter(|node| node.operator == PhysicalOperator::Scan)
    {
        scan_count += 1;
        let evidence = raw
            .physical_dag
            .evidence
            .get(&scan_node.id)
            .ok_or_else(|| AnalyticalCostError::MissingOperatorStatistics(scan_node.id.clone()))?;
        let statistics = &evidence.statistics;
        let OperatorStatistics::Scan {
            edges,
            source_read_bytes,
        } = statistics
        else {
            return Err(AnalyticalCostError::InvalidOperatorStatistics {
                node: scan_node.id.clone(),
                reason: "raw scan evidence uses the wrong statistics variant",
            });
        };
        if edges.input != expected || edges.output != expected || *source_read_bytes != scan {
            return Err(AnalyticalCostError::ComparisonScopeMismatch(
                "raw source evolution",
            ));
        }
    }
    if scan_count == 0 {
        return Err(AnalyticalCostError::MissingComparisonScope("raw scan"));
    }
    Ok(())
}

fn ephemeral_rows_over_horizon(
    inputs: StreamingSummaryInputs,
    scope: &ComparisonScope,
) -> Result<u64, AnalyticalCostError> {
    evaluation_offsets_ms(scope)?
        .into_iter()
        .try_fold(0_u64, |total, offset| {
            let arrivals = (inputs.ingestion_rate_per_second * offset as f64 / 1_000.0).ceil();
            if !arrivals.is_finite() || arrivals < 0.0 || arrivals > u64::MAX as f64 {
                return Err(AnalyticalCostError::Overflow);
            }
            total
                .checked_add(inputs.initial_input_rows)
                .and_then(|value| value.checked_add(arrivals as u64))
                .ok_or(AnalyticalCostError::Overflow)
        })
}

fn ephemeral_scan_bytes_over_horizon(
    inputs: StreamingSummaryInputs,
    raw: &StreamingRawInputEvidence,
    scope: &ComparisonScope,
) -> Result<u64, AnalyticalCostError> {
    if inputs.initial_source_scan_bytes == 0 {
        return Ok(0);
    }
    evaluation_offsets_ms(scope)?
        .into_iter()
        .try_fold(0_u64, |total, offset| {
            let arrivals = (inputs.ingestion_rate_per_second * offset as f64 / 1_000.0).ceil();
            if !arrivals.is_finite() || arrivals < 0.0 || arrivals > u64::MAX as f64 {
                return Err(AnalyticalCostError::Overflow);
            }
            total
                .checked_add(inputs.initial_source_scan_bytes)
                .and_then(|value| {
                    (arrivals as u64)
                        .checked_mul(raw.arriving_source_row_bytes)
                        .and_then(|arriving| value.checked_add(arriving))
                })
                .ok_or(AnalyticalCostError::Overflow)
        })
}

fn evaluation_offsets_ms(scope: &ComparisonScope) -> Result<Vec<u64>, AnalyticalCostError> {
    let count = scope.validate()?;
    match &scope.recurrence {
        QueryRecurrence::OneTime {
            invocations,
            execute_at,
        } => Ok(vec![
            execute_at.map_or(0, |at| at
                .0
                .saturating_sub(scope.planning_time.0));
            *invocations as usize
        ]),
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(interval)) => {
            Ok((1..=count).map(|n| n * u64::from(interval.0)).collect())
        }
        QueryRecurrence::Repeated(RepeatedDemand::Scheduled(schedule)) => Ok(schedule
            .iter()
            .filter(|at| {
                at.0 >= scope.planning_time.0
                    && at.0 <= scope.planning_time.0.saturating_add(scope.horizon.0)
            })
            .map(|at| at.0 - scope.planning_time.0)
            .collect()),
        QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(_)) => Ok((1..=count)
            .map(|n| scope.horizon.0.saturating_mul(n) / count)
            .collect()),
        QueryRecurrence::Unknown => Err(AnalyticalCostError::InvalidRecurrence),
    }
}

impl SummaryMaintenanceCostModel {
    pub fn new(
        calibration: ResourceCalibration,
        capabilities: SummaryMaintenanceCapabilities,
    ) -> Self {
        Self {
            node_evidence: StreamingNodeEvidence::default(),
            calibration,
            capabilities,
            target_comparisons: HashMap::new(),
            candidate_comparisons: HashMap::new(),
            physical_plan_alternatives: HashMap::new(),
            window_framework_candidates: HashMap::new(),
        }
    }

    /// Bind one candidate and its raw baseline to the same target-specific
    /// comparison context. Rebinding a target to different evidence is
    /// rejected rather than silently replacing the canonical context.
    pub fn bind_candidate_comparison(
        &mut self,
        target: &Rc<QueryExpr>,
        root: &Rc<SummaryNode>,
        scope: ComparisonScope,
        raw: StreamingRawInputEvidence,
    ) -> Result<(), AnalyticalCostError> {
        scope.validate()?;
        validate_query_scope(target, &scope)?;
        validate_physical_scope_coverage(&raw.physical_dag, &scope)?;
        validate_raw_snapshot_dimensions(&raw, &scope)?;
        estimate_physical_dag(
            &raw.physical_dag.nodes,
            &raw.physical_dag.root,
            &scope,
            &raw.physical_dag,
        )?;
        let target_ptr = Rc::as_ptr(target);
        if let Some(existing) = self.target_comparisons.get(&target_ptr) {
            if existing.scope != scope || existing.raw != raw {
                return Err(AnalyticalCostError::ComparisonScopeMismatch(
                    "target comparison",
                ));
            }
        }
        // Commit only after every validation above succeeds. Shared nodes do
        // not carry one owning target; context identity is `(target, root)`.
        self.target_comparisons
            .entry(target_ptr)
            .or_insert(StreamingTargetComparison {
                _target: Rc::clone(target),
                scope,
                raw,
            });
        self.candidate_comparisons.insert(
            (target_ptr, Rc::as_ptr(root)),
            BoundCandidateIdentity {
                _target: Rc::clone(target),
                _root: Rc::clone(root),
            },
        );
        Ok(())
    }

    /// Add one complete physical implementation for an already-bound logical
    /// candidate. Duplicate or empty provider identities are rejected.
    pub fn bind_physical_plan_alternative(
        &mut self,
        target: &Rc<QueryExpr>,
        root: &Rc<SummaryNode>,
        alternative: StreamingPhysicalPlanAlternative,
    ) -> Result<(), AnalyticalCostError> {
        let key = (Rc::as_ptr(target), Rc::as_ptr(root));
        if !self.candidate_comparisons.contains_key(&key) {
            return Err(AnalyticalCostError::MissingOrStale(
                "candidate comparison binding",
            ));
        }
        if alternative.physical_plan_id.trim().is_empty() {
            return Err(AnalyticalCostError::MissingOrZero("physical_plan_id"));
        }
        let alternatives = self.physical_plan_alternatives.entry(key).or_default();
        if alternatives
            .iter()
            .any(|existing| existing.physical_plan_id == alternative.physical_plan_id)
        {
            return Err(AnalyticalCostError::ComparisonScopeMismatch(
                "physical plan identity",
            ));
        }
        alternatives.push(alternative);
        Ok(())
    }

    /// Add one complete abstract window assignment to Planner candidate search.
    ///
    /// The provider may bind multiple executor-feasible implementations for
    /// the same framework assignment; their stable identities and complete
    /// evidence keep the implementations distinct during ranking.
    pub fn bind_window_framework_candidate(
        &mut self,
        target: &Rc<QueryExpr>,
        root: &Rc<SummaryNode>,
        candidate: StreamingWindowFrameworkCandidate,
    ) -> Result<(), AnalyticalCostError> {
        let key = (Rc::as_ptr(target), Rc::as_ptr(root));
        if !self.candidate_comparisons.contains_key(&key) {
            return Err(AnalyticalCostError::MissingOrStale(
                "candidate comparison binding",
            ));
        }
        if candidate.physical_plan_id.trim().is_empty() {
            return Err(AnalyticalCostError::MissingOrZero("physical_plan_id"));
        }
        if candidate.assignments.is_empty() {
            return Err(AnalyticalCostError::MissingOrZero(
                "window framework assignments",
            ));
        }
        let mut assigned = HashSet::new();
        if candidate.assignments.iter().any(|assignment| {
            !assigned.insert(Rc::as_ptr(&assignment.summary))
                || matches!(
                    &assignment.framework,
                    Some(SummaryWindowFramework::Extension(name)) if name.trim().is_empty()
                )
        }) {
            return Err(AnalyticalCostError::MissingOrZero(
                "unique window framework assignments",
            ));
        }
        let candidates = self.window_framework_candidates.entry(key).or_default();
        if candidates
            .iter()
            .any(|existing| existing.physical_plan_id == candidate.physical_plan_id)
        {
            return Err(AnalyticalCostError::ComparisonScopeMismatch(
                "window framework candidate",
            ));
        }
        candidates.push(candidate);
        Ok(())
    }

    fn comparison_context(
        &self,
        root: &SummaryNode,
        target: Option<&QueryExpr>,
        horizon: Option<crate::recurrence::Horizon>,
        expected_reads: Option<f64>,
    ) -> Option<(CandidateComparisonKey, &StreamingTargetComparison)> {
        let root_ptr = root as *const _;
        let target_ptr = match target {
            Some(target) => target as *const _,
            None => {
                let mut targets = self
                    .candidate_comparisons
                    .keys()
                    .filter_map(|(target, candidate)| (*candidate == root_ptr).then_some(*target));
                let only = targets.next()?;
                if targets.next().is_some() {
                    return None;
                }
                only
            }
        };
        let key = (target_ptr, root_ptr);
        if !self.candidate_comparisons.contains_key(&key) {
            return None;
        }
        let comparison = self.target_comparisons.get(&target_ptr)?;
        if horizon.map(|value| value.0 * 1_000.0) != Some(comparison.scope.horizon.0 as f64)
            || expected_reads != Some(comparison.scope.validate().ok()? as f64)
        {
            return None;
        }
        Some((key, comparison))
    }

    fn complete_cost_with_evidence(
        &self,
        root: &SummaryNode,
        deployments: &[CostedSummaryDeployment<'_>],
        comparison: &StreamingTargetComparison,
        evidence: &StreamingNodeEvidence,
        window_frameworks: &[Option<SummaryWindowFramework>],
    ) -> Option<Cost> {
        self.calibrated(
            estimate_heterogeneous_summary(
                root,
                deployments,
                evidence,
                &comparison.scope,
                &comparison.raw,
                window_frameworks,
            )
            .ok()?,
        )
    }

    fn canonical_inputs(&self, summary: &SummaryNode) -> Option<StreamingAggregateEvidence> {
        let evidence = self.node_evidence.aggregation(summary)?;
        evidence.inputs.validate().ok()?;
        Some(evidence)
    }

    fn calibrated(&self, estimate: ResourceEstimate) -> Option<Cost> {
        estimate.calibrated_cost(&self.calibration).ok().map(Cost)
    }

    fn lifecycle_inputs(
        &self,
        summary: &SummaryNode,
        horizon: Option<crate::recurrence::Horizon>,
    ) -> Option<SummaryMaintenanceLifecycleCostInputs> {
        let evidence = self.canonical_inputs(summary)?;
        let inputs = evidence.inputs;
        let insert = validated_operator_cpu("insert_cpu_ops", evidence.insert_cpu_ops).ok()?;
        let build = self.calibrated(ResourceEstimate {
            cpu_ops: inputs.initial_input_rows as f64
                * inputs.bootstrap_window_count as f64
                * insert,
            peak_memory_bytes: 0,
            scan_bytes: inputs.initial_source_scan_bytes,
        })?;
        let maintenance = self.calibrated(ResourceEstimate {
            cpu_ops: inputs.active_window_count as f64 * insert,
            peak_memory_bytes: 0,
            scan_bytes: 0,
        })?;
        let retained = inputs
            .active_window_count
            .checked_add(inputs.retained_window_count)?
            .checked_mul(inputs.physical_summary_count)?
            .checked_mul(inputs.state_bytes_per_summary)?;
        let retention_total = self.calibrated(ResourceEstimate {
            cpu_ops: 0.0,
            peak_memory_bytes: retained,
            scan_bytes: 0,
        })?;
        let horizon_seconds = horizon.filter(|value| value.0 > 0.0)?.0;
        Some(SummaryMaintenanceLifecycleCostInputs {
            build_cost: Some(build),
            maintenance_cost_per_update: Some(maintenance),
            // Readout is a separate physical operator in the complete DAG.
            // A state-only candidate therefore does not fabricate readout
            // evidence merely to keep a lifecycle alternative selectable.
            summary_read_cost: Some(Cost::ZERO),
            retention_cost_rate: Some(CostRate(retention_total.0 / horizon_seconds)),
            // Releasing memory has no modeled CPU or I/O. This is not an
            // implicit expiration/rebuild policy; those require an explicit
            // SummaryDelete or future authoritative lifecycle evidence.
            retirement_cost: Some(Cost::ZERO),
        })
    }
}

impl CostModel for SummaryMaintenanceCostModel {
    fn candidate_cost(
        &self,
        candidate: &ReplacementSubDAG,
        _target: &TargetSubDAG<'_>,
    ) -> Option<Cost> {
        match &candidate.replacement {
            // Lifecycle selection supplies a complete override. If it cannot,
            // the candidate remains unavailable rather than receiving this
            // trait's structural fallback.
            Replacement::Summary(_) => None,
            Replacement::Rewrite(_) => None,
        }
    }

    fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm> {
        DefaultCostModel.rank_candidates(intent, candidates)
    }

    fn estimate_cost(&self, _candidate: &ReplacementSubDAG, _target: &TargetSubDAG<'_>) -> f64 {
        f64::INFINITY
    }

    fn summary_maintenance_lifecycle_cost_inputs(
        &self,
        _summary: &SummaryNode,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        SummaryMaintenanceLifecycleCostInputs::default()
    }

    fn summary_maintenance_lifecycle_cost_inputs_for_horizon(
        &self,
        summary: &SummaryNode,
        horizon: Option<crate::recurrence::Horizon>,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        self.lifecycle_inputs(summary, horizon).unwrap_or_default()
    }

    fn summary_maintenance_capabilities(
        &self,
        _summary: &SummaryNode,
    ) -> SummaryMaintenanceCapabilities {
        self.capabilities
    }

    fn complete_summary_candidate_cost(
        &self,
        root: &SummaryNode,
        target: Option<&QueryExpr>,
        deployments: &[CostedSummaryDeployment<'_>],
        horizon: Option<crate::recurrence::Horizon>,
        expected_reads: Option<f64>,
        required_accuracy: &[AccuracyTarget],
    ) -> Option<Cost> {
        self.complete_summary_candidate_estimate(
            root,
            target,
            deployments,
            horizon,
            expected_reads,
            required_accuracy,
        )
        .map(|estimate| estimate.cost)
    }

    fn complete_summary_candidate_estimate(
        &self,
        root: &SummaryNode,
        target: Option<&QueryExpr>,
        deployments: &[CostedSummaryDeployment<'_>],
        horizon: Option<crate::recurrence::Horizon>,
        expected_reads: Option<f64>,
        required_accuracy: &[AccuracyTarget],
    ) -> Option<CompleteSummaryCandidateEstimate> {
        let (key, comparison) = self.comparison_context(root, target, horizon, expected_reads)?;
        if let Some(candidates) = self.window_framework_candidates.get(&key) {
            return candidates
                .iter()
                .filter_map(|candidate| {
                    if candidate.assignments.len() != deployments.len() {
                        return None;
                    }
                    if !candidate
                        .accuracy
                        .matches_assignments(&candidate.assignments)
                    {
                        return None;
                    }
                    let window_frameworks = deployments
                        .iter()
                        .map(|deployment| {
                            candidate
                                .assignments
                                .iter()
                                .find(|assignment| {
                                    std::ptr::eq(assignment.summary.as_ref(), deployment.summary)
                                })
                                .map(|assignment| assignment.framework.clone())
                        })
                        .collect::<Option<Vec<_>>>()?;
                    let uses_exponential_histogram = window_frameworks.iter().any(|framework| {
                        matches!(
                            framework,
                            Some(SummaryWindowFramework::ExponentialHistogram)
                        )
                    });
                    let window_accuracy_guarantee =
                        candidate.accuracy.guarantee(uses_exponential_histogram)?;
                    if !required_accuracy.iter().all(|target| {
                        DefaultAccuracyModel.satisfies(&window_accuracy_guarantee, target)
                    }) {
                        return None;
                    }
                    self.complete_cost_with_evidence(
                        root,
                        deployments,
                        comparison,
                        &candidate.node_evidence,
                        &window_frameworks,
                    )
                    .map(|cost| CompleteSummaryCandidateEstimate {
                        cost,
                        physical_plan_id: Some(candidate.physical_plan_id.clone()),
                        window_frameworks,
                        window_accuracy_guarantee: Some(window_accuracy_guarantee),
                    })
                })
                .min_by(|left, right| left.cost.0.total_cmp(&right.cost.0));
        }
        if let Some(alternatives) = self.physical_plan_alternatives.get(&key) {
            return alternatives
                .iter()
                .filter_map(|alternative| {
                    self.complete_cost_with_evidence(
                        root,
                        deployments,
                        comparison,
                        &alternative.node_evidence,
                        &vec![None; deployments.len()],
                    )
                    .map(|cost| CompleteSummaryCandidateEstimate {
                        cost,
                        physical_plan_id: Some(alternative.physical_plan_id.clone()),
                        window_frameworks: vec![None; deployments.len()],
                        window_accuracy_guarantee: None,
                    })
                })
                .min_by(|left, right| left.cost.0.total_cmp(&right.cost.0));
        }
        let frameworks = vec![None; deployments.len()];
        self
            .complete_cost_with_evidence(
                root,
                deployments,
                comparison,
                &self.node_evidence,
                &frameworks,
            )
            .map(|cost| CompleteSummaryCandidateEstimate {
                cost,
                physical_plan_id: None,
                window_frameworks: frameworks,
                window_accuracy_guarantee: None,
            })
    }

    fn complete_summary_candidate_estimate_covers_lifecycle_costs(&self) -> bool {
        true
    }

    fn raw_query_recompute_cost(&self, target: &QueryExpr) -> Option<Cost> {
        let _ = target;
        None
    }

    fn raw_query_recompute_total_cost(
        &self,
        target: &QueryExpr,
        expected_reads: f64,
    ) -> Option<Cost> {
        let target_ptr = target as *const _;
        let comparison = self.target_comparisons.get(&target_ptr)?;
        let evaluations = comparison.scope.validate().ok()?;
        if expected_reads != evaluations as f64 {
            return None;
        }
        self.calibrated(
            estimate_physical_dag(
                &comparison.raw.physical_dag.nodes,
                &comparison.raw.physical_dag.root,
                &comparison.scope,
                &comparison.raw.physical_dag,
            )
            .ok()?,
        )
    }
}

fn estimate_heterogeneous_summary(
    root: &SummaryNode,
    deployments: &[CostedSummaryDeployment<'_>],
    evidence: &StreamingNodeEvidence,
    scope: &ComparisonScope,
    raw: &StreamingRawInputEvidence,
    window_frameworks: &[Option<SummaryWindowFramework>],
) -> Result<ResourceEstimate, AnalyticalCostError> {
    if window_frameworks.len() != deployments.len() {
        return Err(AnalyticalCostError::ComparisonScopeMismatch(
            "window framework assignments",
        ));
    }
    let frameworks_by_node: HashMap<_, _> = deployments
        .iter()
        .zip(window_frameworks)
        .map(|(deployment, framework)| (deployment.summary as *const _, framework))
        .collect();
    validate_summary_edges_and_physical_ids(root, evidence, &frameworks_by_node)?;
    fn summary_source_selections(
        node: &SummaryNode,
        seen: &mut HashSet<*const SummaryNode>,
        out: &mut Vec<LogicalSourceSelection>,
    ) -> Result<(), AnalyticalCostError> {
        if !seen.insert(node as *const _) {
            return Ok(());
        }
        match &node.expr {
            SummaryExpr::KeepPreAsap(query) => query_source_selections(query, out)?,
            SummaryExpr::SummaryAgg { child, .. } => summary_source_selections(child, seen, out)?,
            SummaryExpr::SummaryMerge { children } => {
                for child in children {
                    summary_source_selections(child, seen, out)?;
                }
            }
            SummaryExpr::SummarySubtract { left, right }
            | SummaryExpr::SummaryJoin {
                outer: left,
                inner: right,
                ..
            } => {
                summary_source_selections(left, seen, out)?;
                summary_source_selections(right, seen, out)?;
            }
            SummaryExpr::SummaryDelete { summary_input, .. }
            | SummaryExpr::SummaryEstimate { summary_input, .. } => {
                summary_source_selections(summary_input, seen, out)?
            }
        }
        Ok(())
    }
    let evaluation_count = scope.validate()?;
    let by_node: HashMap<_, _> = deployments
        .iter()
        .map(|deployment| (deployment.summary as *const _, deployment))
        .collect();
    let mut cpu_ops = 0.0;
    let mut persistent_bytes = 0_u64;
    let mut ephemeral_state_bytes = 0_u64;
    let mut scans = HashMap::<String, (usize, u64)>::new();
    let mut physical_states = HashMap::<
        String,
        (
            StreamingAggregateEvidence,
            SummaryMaintenanceLifecycleGuarantee,
            String,
            Option<SummaryWindowFramework>,
        ),
    >::new();
    for deployment in deployments {
        let node_evidence = evidence
            .aggregation(deployment.summary)
            .ok_or(AnalyticalCostError::MissingOrStale("summary_agg"))?;
        let SummaryExpr::SummaryAgg { child, .. } = &deployment.summary.expr else {
            return Err(AnalyticalCostError::UnsupportedCandidate);
        };
        let inputs = node_evidence.inputs.validate()?;
        match node_evidence.source_coverage_index {
            Some(index) => {
                let declared =
                    scope
                        .sources
                        .get(index)
                        .ok_or(AnalyticalCostError::MissingComparisonScope(
                            "summary source coverage",
                        ))?;
                if !matches!(&child.expr, SummaryExpr::KeepPreAsap(_))
                    || inputs.initial_input_rows != raw.planning_time_input_rows
                    || inputs.initial_input_bytes != raw.planning_time_input_bytes
                    || inputs.initial_source_scan_bytes != raw.planning_time_source_scan_bytes
                    || inputs.ingestion_rate_per_second != raw.ingestion_rate_per_second
                {
                    return Err(AnalyticalCostError::ComparisonScopeMismatch(
                        "source-root bootstrap evolution",
                    ));
                }
                let mut actual_selections = Vec::new();
                summary_source_selections(child, &mut HashSet::new(), &mut actual_selections)?;
                let actual_selections = deduplicate_source_selections(actual_selections);
                let expected = (
                    declared.source.clone(),
                    declared.predicates.clone(),
                    declared.info_matchers.clone(),
                );
                if actual_selections.as_slice() != [expected] {
                    return Err(AnalyticalCostError::ComparisonScopeMismatch(
                        "summary source lineage",
                    ));
                }
            }
            None => {
                if matches!(&child.expr, SummaryExpr::KeepPreAsap(_))
                    || inputs.initial_source_scan_bytes != 0
                    || !node_evidence.bootstrap_read_identity.is_empty()
                {
                    return Err(AnalyticalCostError::ComparisonScopeMismatch(
                        "intermediate bootstrap source ownership",
                    ));
                }
            }
        }
        validate_guarantee(deployment.guarantee, scope.data_arrival)?;
        let logical_state = format!("{:?}", deployment.summary.expr);
        let window_framework = (*frameworks_by_node
            .get(&(deployment.summary as *const _))
            .ok_or(AnalyticalCostError::MissingOrStale(
                "window framework assignment",
            ))?)
        .clone();
        match physical_states.entry(node_evidence.physical_id.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((
                    node_evidence.clone(),
                    deployment.guarantee.clone(),
                    logical_state,
                    window_framework,
                ));
            }
            std::collections::hash_map::Entry::Occupied(entry)
                if entry.get()
                    != &(
                        node_evidence.clone(),
                        deployment.guarantee.clone(),
                        logical_state,
                        window_framework,
                    ) =>
            {
                return Err(AnalyticalCostError::ComparisonScopeMismatch(
                    "summary physical identity",
                ));
            }
            std::collections::hash_map::Entry::Occupied(_) => continue,
        }
        let ephemeral = matches!(
            deployment.guarantee.summary_maintenance_lifecycle,
            SummaryMaintenanceLifecycle::Ephemeral
        );
        let (bootstrap, updates, source_scan_bytes) = if ephemeral {
            (
                ephemeral_rows_over_horizon(inputs, scope)?,
                0,
                ephemeral_scan_bytes_over_horizon(inputs, raw, scope)?,
            )
        } else {
            let (bootstrap, updates, _) =
                lifecycle_row_counts(inputs, deployment.guarantee, scope)?;
            let bootstrap_extra_rows = bootstrap
                .checked_sub(inputs.initial_input_rows)
                .ok_or(AnalyticalCostError::Overflow)?;
            let source_scan_bytes = if node_evidence.source_coverage_index.is_some() {
                inputs
                    .initial_source_scan_bytes
                    .checked_add(
                        bootstrap_extra_rows
                            .checked_mul(raw.arriving_source_row_bytes)
                            .ok_or(AnalyticalCostError::Overflow)?,
                    )
                    .ok_or(AnalyticalCostError::Overflow)?
            } else {
                0
            };
            (bootstrap, updates, source_scan_bytes)
        };
        let insert = validated_operator_cpu("insert_cpu_ops", node_evidence.insert_cpu_ops)?;
        let insert_calls = bootstrap
            .checked_mul(inputs.bootstrap_window_count)
            .and_then(|calls| {
                updates
                    .checked_mul(inputs.active_window_count)
                    .and_then(|updates| calls.checked_add(updates))
            })
            .ok_or(AnalyticalCostError::Overflow)?;
        cpu_ops += insert_calls as f64 * insert;
        let live_window_count = if ephemeral {
            inputs.bootstrap_window_count
        } else {
            inputs
                .active_window_count
                .checked_add(inputs.retained_window_count)
                .ok_or(AnalyticalCostError::Overflow)?
        };
        let state_bytes = live_window_count
            .checked_mul(inputs.physical_summary_count)
            .and_then(|states| states.checked_mul(inputs.state_bytes_per_summary))
            .ok_or(AnalyticalCostError::Overflow)?;
        if ephemeral {
            ephemeral_state_bytes = ephemeral_state_bytes
                .checked_add(state_bytes)
                .ok_or(AnalyticalCostError::Overflow)?;
        } else {
            persistent_bytes = persistent_bytes
                .checked_add(state_bytes)
                .ok_or(AnalyticalCostError::Overflow)?;
        }
        if let Some(source_index) = node_evidence.source_coverage_index {
            if node_evidence.bootstrap_read_identity.is_empty() {
                return Err(AnalyticalCostError::MissingOrStale(
                    "bootstrap_read_identity",
                ));
            }
            match scans.entry(node_evidence.bootstrap_read_identity.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert((source_index, source_scan_bytes));
                }
                std::collections::hash_map::Entry::Occupied(entry)
                    if *entry.get() != (source_index, source_scan_bytes) =>
                {
                    return Err(AnalyticalCostError::ComparisonScopeMismatch(
                        "bootstrap source bytes",
                    ));
                }
                _ => {}
            }
        }
    }
    let covered_sources: HashSet<_> = scans.values().map(|(index, _)| *index).collect();
    if covered_sources.len() != scope.sources.len()
        || !(0..scope.sources.len()).all(|index| covered_sources.contains(&index))
    {
        return Err(AnalyticalCostError::ComparisonScopeMismatch("sources"));
    }

    #[expect(clippy::too_many_arguments, reason = "CPU and I/O traversal state")]
    fn visit_ops(
        node: &SummaryNode,
        seen: &mut HashSet<String>,
        by_node: &HashMap<*const SummaryNode, &CostedSummaryDeployment<'_>>,
        evidence: &StreamingNodeEvidence,
        scope: &ComparisonScope,
        evaluation_count: u64,
        cpu_ops: &mut f64,
        io_bytes: &mut u64,
    ) -> Result<(), AnalyticalCostError> {
        let physical_id = summary_physical_id(node, evidence)?;
        if !seen.insert(physical_id) {
            return Ok(());
        }
        match &node.expr {
            SummaryExpr::KeepPreAsap(_) => {
                let retained = evidence
                    .retained_queries
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("keep_pre_asap"))?;
                if !retained.preprocessing_cpu_ops_over_horizon.is_finite()
                    || retained.preprocessing_cpu_ops_over_horizon < 0.0
                {
                    return Err(AnalyticalCostError::InvalidOperationCost(
                        "keep_pre_asap",
                        retained.preprocessing_cpu_ops_over_horizon,
                    ));
                }
                *cpu_ops += retained.preprocessing_cpu_ops_over_horizon;
            }
            SummaryExpr::SummaryAgg { child, .. } => {
                visit_ops(
                    child,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluation_count,
                    cpu_ops,
                    io_bytes,
                )?;
            }
            SummaryExpr::SummaryMerge { children } => {
                let operation = summary_operation_evidence(node, evidence)?.resource();
                let merge = validated_operator_cpu("summary_merge", operation.cpu_ops)?;
                *cpu_ops += evaluation_count as f64
                    * validated_operator_executions("summary_merge", operation)? as f64
                    * merge;
                add_operator_io(io_bytes, operation, evaluation_count)?;
                for child in children {
                    visit_ops(
                        child,
                        seen,
                        by_node,
                        evidence,
                        scope,
                        evaluation_count,
                        cpu_ops,
                        io_bytes,
                    )?;
                }
            }
            SummaryExpr::SummarySubtract { left, right } => {
                let operation = summary_operation_evidence(node, evidence)?.resource();
                *cpu_ops += evaluation_count as f64
                    * validated_operator_executions("summary_subtract", operation)? as f64
                    * validated_operator_cpu("summary_subtract", operation.cpu_ops)?;
                add_operator_io(io_bytes, operation, evaluation_count)?;
                visit_ops(
                    left,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluation_count,
                    cpu_ops,
                    io_bytes,
                )?;
                visit_ops(
                    right,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluation_count,
                    cpu_ops,
                    io_bytes,
                )?;
            }
            SummaryExpr::SummaryDelete { summary_input, .. } => {
                let delete = summary_operation_evidence(node, evidence)?;
                let StreamingSummaryOperatorEvidence::Delete {
                    resource: operation,
                    events_per_second,
                    routing_fanout,
                } = delete
                else {
                    unreachable!("operation kind was validated")
                };
                let state_ptr = evidence
                    .operation_state_owners
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_delete_owner"))?;
                fn collect_aggs(
                    node: &SummaryNode,
                    seen: &mut HashSet<*const SummaryNode>,
                    out: &mut Vec<*const SummaryNode>,
                ) {
                    if !seen.insert(node as *const _) {
                        return;
                    }
                    match &node.expr {
                        SummaryExpr::SummaryAgg { child, .. } => {
                            out.push(node as *const _);
                            collect_aggs(child, seen, out);
                        }
                        SummaryExpr::SummaryMerge { children } => {
                            children
                                .iter()
                                .for_each(|child| collect_aggs(child, seen, out));
                        }
                        SummaryExpr::SummarySubtract { left, right }
                        | SummaryExpr::SummaryJoin {
                            outer: left,
                            inner: right,
                            ..
                        } => {
                            collect_aggs(left, seen, out);
                            collect_aggs(right, seen, out);
                        }
                        SummaryExpr::SummaryDelete { summary_input, .. }
                        | SummaryExpr::SummaryEstimate { summary_input, .. } => {
                            collect_aggs(summary_input, seen, out)
                        }
                        SummaryExpr::KeepPreAsap(_) => {}
                    }
                }
                let mut reachable = Vec::new();
                collect_aggs(summary_input, &mut HashSet::new(), &mut reachable);
                if reachable.as_slice() != [*state_ptr] {
                    return Err(AnalyticalCostError::ComparisonScopeMismatch(
                        "summary delete owner",
                    ));
                }
                let deployment = by_node
                    .get(state_ptr)
                    .copied()
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_delete_owner"))?;
                let state = evidence
                    .aggregation(deployment.summary)
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_delete_owner"))?;
                let (_, _, active_ms) =
                    lifecycle_row_counts(state.inputs, deployment.guarantee, scope)?;
                if !events_per_second.is_finite() || *events_per_second < 0.0 {
                    return Err(AnalyticalCostError::InvalidIngestionRate(
                        *events_per_second,
                    ));
                }
                if *routing_fanout == 0 {
                    return Err(AnalyticalCostError::MissingOrZero("delete_routing_fanout"));
                }
                let delete_events = (events_per_second * active_ms as f64 / 1_000.0).ceil()
                    * *routing_fanout as f64;
                if !delete_events.is_finite() || delete_events > u64::MAX as f64 {
                    return Err(AnalyticalCostError::Overflow);
                }
                let delete_events = delete_events as u64;
                *cpu_ops += delete_events as f64
                    * validated_operator_executions("summary_delete", operation)? as f64
                    * validated_operator_cpu("summary_delete", operation.cpu_ops)?;
                add_operator_io(io_bytes, operation, delete_events)?;
                visit_ops(
                    summary_input,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluation_count,
                    cpu_ops,
                    io_bytes,
                )?;
            }
            SummaryExpr::SummaryEstimate { summary_input, .. } => {
                let operation = summary_operation_evidence(node, evidence)?.resource();
                *cpu_ops += evaluation_count as f64
                    * validated_operator_executions("summary_readout", operation)? as f64
                    * validated_operator_cpu("summary_readout", operation.cpu_ops)?;
                add_operator_io(io_bytes, operation, evaluation_count)?;
                visit_ops(
                    summary_input,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluation_count,
                    cpu_ops,
                    io_bytes,
                )?;
            }
            SummaryExpr::SummaryJoin { outer, inner, .. } => {
                let join = evidence
                    .joins
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_join"))?;
                if !join.cpu_ops_per_execution.is_finite()
                    || join.cpu_ops_per_execution <= 0.0
                    || join.working_memory_bytes == 0
                    || join.executions_per_evaluation == 0
                {
                    return Err(AnalyticalCostError::MissingOrStale("summary_join"));
                }
                *cpu_ops += evaluation_count as f64
                    * join.executions_per_evaluation as f64
                    * join.cpu_ops_per_execution;
                let join_io = join
                    .io_bytes_per_execution
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_join_io"))?;
                *io_bytes = io_bytes
                    .checked_add(
                        join_io
                            .checked_mul(join.executions_per_evaluation)
                            .and_then(|bytes| bytes.checked_mul(evaluation_count))
                            .ok_or(AnalyticalCostError::Overflow)?,
                    )
                    .ok_or(AnalyticalCostError::Overflow)?;
                visit_ops(
                    outer,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluation_count,
                    cpu_ops,
                    io_bytes,
                )?;
                visit_ops(
                    inner,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluation_count,
                    cpu_ops,
                    io_bytes,
                )?;
            }
        }
        Ok(())
    }

    let mut operator_io_bytes = 0;
    visit_ops(
        root,
        &mut HashSet::new(),
        &by_node,
        evidence,
        scope,
        evaluation_count,
        &mut cpu_ops,
        &mut operator_io_bytes,
    )?;
    let transient_bytes = estimate_transient_liveness(root, evidence)?;
    if !cpu_ops.is_finite() {
        return Err(AnalyticalCostError::Overflow);
    }
    Ok(ResourceEstimate {
        cpu_ops,
        peak_memory_bytes: persistent_bytes
            .checked_add(transient_bytes)
            .and_then(|bytes| bytes.checked_add(ephemeral_state_bytes))
            .ok_or(AnalyticalCostError::Overflow)?,
        scan_bytes: scans
            .values()
            .try_fold(operator_io_bytes, |sum, (_, bytes)| {
                sum.checked_add(*bytes).ok_or(AnalyticalCostError::Overflow)
            })?,
    })
}

fn add_operator_io(
    total: &mut u64,
    operation: &SummaryOperatorResourceEvidence,
    execution_units: u64,
) -> Result<(), AnalyticalCostError> {
    let bytes = operation
        .io_bytes_per_execution
        .ok_or(AnalyticalCostError::MissingOrStale("summary operator io"))?;
    if operation.executions_per_evaluation == 0 {
        return Err(AnalyticalCostError::MissingOrStale(
            "summary operator executions",
        ));
    }
    *total = total
        .checked_add(
            bytes
                .checked_mul(operation.executions_per_evaluation)
                .and_then(|value| value.checked_mul(execution_units))
                .ok_or(AnalyticalCostError::Overflow)?,
        )
        .ok_or(AnalyticalCostError::Overflow)?;
    Ok(())
}

fn validate_summary_edges_and_physical_ids(
    root: &SummaryNode,
    evidence: &StreamingNodeEvidence,
    frameworks_by_node: &HashMap<*const SummaryNode, &Option<SummaryWindowFramework>>,
) -> Result<(), AnalyticalCostError> {
    fn children(node: &SummaryNode) -> Vec<&SummaryNode> {
        match &node.expr {
            SummaryExpr::KeepPreAsap(_) => vec![],
            SummaryExpr::SummaryAgg { child, .. } => vec![child],
            SummaryExpr::SummaryMerge { children } => {
                children.iter().map(|child| child.as_ref()).collect()
            }
            SummaryExpr::SummarySubtract { left, right }
            | SummaryExpr::SummaryJoin {
                outer: left,
                inner: right,
                ..
            } => vec![left, right],
            SummaryExpr::SummaryDelete { summary_input, .. }
            | SummaryExpr::SummaryEstimate { summary_input, .. } => vec![summary_input],
        }
    }
    fn metadata(
        node: &SummaryNode,
        evidence: &StreamingNodeEvidence,
    ) -> Result<(String, Vec<EdgeStatistics>, EdgeStatistics), AnalyticalCostError> {
        match &node.expr {
            SummaryExpr::KeepPreAsap(_) => {
                let retained = evidence
                    .retained_queries
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("keep_pre_asap"))?;
                Ok((retained.physical_id.clone(), vec![], retained.output))
            }
            SummaryExpr::SummaryAgg { .. } => {
                let value = evidence
                    .aggregations
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_agg"))?;
                Ok((value.physical_id.clone(), vec![value.input], value.output))
            }
            SummaryExpr::SummaryJoin { .. } => {
                let value = evidence
                    .joins
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_join"))?;
                Ok((
                    value.physical_id.clone(),
                    value.inputs.clone(),
                    value.output,
                ))
            }
            _ => {
                let value = summary_operation_evidence(node, evidence)?.resource();
                Ok((
                    value.physical_id.clone(),
                    value.inputs.clone(),
                    value.output,
                ))
            }
        }
    }
    fn visit(
        node: &SummaryNode,
        evidence: &StreamingNodeEvidence,
        frameworks_by_node: &HashMap<*const SummaryNode, &Option<SummaryWindowFramework>>,
        seen: &mut HashSet<*const SummaryNode>,
        physical: &mut HashMap<String, (Vec<EdgeStatistics>, EdgeStatistics, String)>,
    ) -> Result<EdgeStatistics, AnalyticalCostError> {
        if !seen.insert(node as *const _) {
            return metadata(node, evidence).map(|(_, _, output)| output);
        }
        let child_nodes = children(node);
        let child_outputs = child_nodes
            .iter()
            .map(|child| visit(child, evidence, frameworks_by_node, seen, physical))
            .collect::<Result<Vec<_>, _>>()?;
        let child_physical_ids = child_nodes
            .iter()
            .map(|child| summary_physical_id(child, evidence))
            .collect::<Result<Vec<_>, _>>()?;
        let (id, inputs, output) = metadata(node, evidence)?;
        let local_fingerprint = match &node.expr {
            SummaryExpr::KeepPreAsap(_) => {
                format!("{:?}", evidence.retained_queries.get(&(node as *const _)))
            }
            SummaryExpr::SummaryAgg { .. } => {
                format!("{:?}", evidence.aggregations.get(&(node as *const _)))
            }
            SummaryExpr::SummaryJoin { .. } => {
                format!("{:?}", evidence.joins.get(&(node as *const _)))
            }
            _ => format!("{:?}", evidence.operations.get(&(node as *const _))),
        };
        // A provider identity names the complete physical operator, including
        // its inputs. Equal local widths/costs do not make operators consuming
        // different physical children the same deployment.
        let framework = frameworks_by_node.get(&(node as *const _));
        let fingerprint = format!(
            "logical={:?}|framework={framework:?}|{local_fingerprint}|children={child_physical_ids:?}",
            node.expr
        );
        if id.is_empty()
            || inputs != child_outputs
            || !output.is_consistent()
            || inputs.iter().any(|edge| !edge.is_consistent())
        {
            return Err(AnalyticalCostError::ComparisonScopeMismatch(
                "summary physical edge statistics",
            ));
        }
        match physical.entry(id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((inputs, output, fingerprint));
            }
            std::collections::hash_map::Entry::Occupied(entry)
                if entry.get() != &(inputs, output, fingerprint) =>
            {
                return Err(AnalyticalCostError::ComparisonScopeMismatch(
                    "summary physical identity",
                ));
            }
            _ => {}
        }
        Ok(output)
    }
    visit(
        root,
        evidence,
        frameworks_by_node,
        &mut HashSet::new(),
        &mut HashMap::new(),
    )
    .map(|_| ())
}

fn summary_physical_id(
    node: &SummaryNode,
    evidence: &StreamingNodeEvidence,
) -> Result<String, AnalyticalCostError> {
    match &node.expr {
        SummaryExpr::KeepPreAsap(_) => evidence
            .retained_queries
            .get(&(node as *const _))
            .map(|value| value.physical_id.clone()),
        SummaryExpr::SummaryAgg { .. } => evidence
            .aggregations
            .get(&(node as *const _))
            .map(|value| value.physical_id.clone()),
        SummaryExpr::SummaryJoin { .. } => evidence
            .joins
            .get(&(node as *const _))
            .map(|value| value.physical_id.clone()),
        _ => summary_operation_evidence(node, evidence)
            .ok()
            .map(|value| value.resource().physical_id.clone()),
    }
    .ok_or(AnalyticalCostError::MissingOrStale(
        "summary physical identity",
    ))
}

/// Simulate a deterministic child-before-parent physical schedule. Completed
/// child output buffers remain live until their final consumer executes;
/// operator workspace and its output buffer coexist during that execution.
fn estimate_transient_liveness(
    root: &SummaryNode,
    evidence: &StreamingNodeEvidence,
) -> Result<u64, AnalyticalCostError> {
    fn children(node: &SummaryNode) -> Vec<&SummaryNode> {
        match &node.expr {
            SummaryExpr::KeepPreAsap(_) => vec![],
            SummaryExpr::SummaryAgg { child, .. } => vec![child],
            SummaryExpr::SummaryMerge { children } => {
                children.iter().map(|child| child.as_ref()).collect()
            }
            SummaryExpr::SummarySubtract { left, right }
            | SummaryExpr::SummaryJoin {
                outer: left,
                inner: right,
                ..
            } => vec![left, right],
            SummaryExpr::SummaryDelete { summary_input, .. }
            | SummaryExpr::SummaryEstimate { summary_input, .. } => vec![summary_input],
        }
    }
    fn visit<'a>(
        node: &'a SummaryNode,
        evidence: &StreamingNodeEvidence,
        seen: &mut HashSet<String>,
        uses: &mut HashMap<String, usize>,
        order: &mut Vec<&'a SummaryNode>,
    ) -> Result<(), AnalyticalCostError> {
        if !seen.insert(summary_physical_id(node, evidence)?) {
            return Ok(());
        }
        for child in children(node) {
            *uses
                .entry(summary_physical_id(child, evidence)?)
                .or_default() += 1;
            visit(child, evidence, seen, uses, order)?;
        }
        order.push(node);
        Ok(())
    }
    fn memory(
        node: &SummaryNode,
        evidence: &StreamingNodeEvidence,
    ) -> Result<(u64, u64), AnalyticalCostError> {
        match &node.expr {
            SummaryExpr::KeepPreAsap(_) => evidence
                .retained_queries
                .get(&(node as *const _))
                .map(|value| (value.working_memory_bytes, value.output_buffer_bytes))
                .ok_or(AnalyticalCostError::MissingOrStale("keep_pre_asap")),
            SummaryExpr::SummaryAgg { .. } => Ok((0, 0)),
            SummaryExpr::SummaryJoin { .. } => evidence
                .joins
                .get(&(node as *const _))
                .map(|value| (value.working_memory_bytes, value.output_buffer_bytes))
                .ok_or(AnalyticalCostError::MissingOrStale("summary_join")),
            SummaryExpr::SummaryMerge { .. }
            | SummaryExpr::SummarySubtract { .. }
            | SummaryExpr::SummaryDelete { .. }
            | SummaryExpr::SummaryEstimate { .. } => {
                let value = summary_operation_evidence(node, evidence)?.resource();
                Ok((value.working_memory_bytes, value.output_buffer_bytes))
            }
        }
    }

    let mut uses = HashMap::new();
    let mut order = Vec::new();
    visit(root, evidence, &mut HashSet::new(), &mut uses, &mut order)?;
    let outputs: HashMap<_, _> = order
        .iter()
        .map(|node| {
            memory(node, evidence)
                .and_then(|(_, output)| summary_physical_id(node, evidence).map(|id| (id, output)))
        })
        .collect::<Result<_, _>>()?;
    let mut live = 0_u64;
    let mut peak = 0_u64;
    for node in order {
        let (workspace, output) = memory(node, evidence)?;
        peak = peak.max(
            live.checked_add(workspace)
                .and_then(|bytes| bytes.checked_add(output))
                .ok_or(AnalyticalCostError::Overflow)?,
        );
        live = live
            .checked_add(output)
            .ok_or(AnalyticalCostError::Overflow)?;
        for child in children(node) {
            let child_id = summary_physical_id(child, evidence)?;
            let remaining =
                uses.get_mut(&child_id)
                    .ok_or(AnalyticalCostError::InvalidPhysicalDag(
                        "missing summary consumer count",
                    ))?;
            *remaining -= 1;
            if *remaining == 0 {
                live = live
                    .checked_sub(outputs[&child_id])
                    .ok_or(AnalyticalCostError::Overflow)?;
            }
        }
    }
    Ok(peak)
}

#[cfg(test)]
fn evidence_nodes(root: &SummaryNode) -> (Vec<&SummaryNode>, Vec<&SummaryNode>) {
    fn visit<'a>(
        node: &'a SummaryNode,
        seen: &mut HashSet<*const SummaryNode>,
        aggregations: &mut Vec<&'a SummaryNode>,
        joins: &mut Vec<&'a SummaryNode>,
    ) {
        if !seen.insert(node as *const _) {
            return;
        }
        match &node.expr {
            SummaryExpr::SummaryAgg { child, .. } => {
                aggregations.push(node);
                visit(child, seen, aggregations, joins);
            }
            SummaryExpr::SummaryMerge { children } => {
                for child in children {
                    visit(child, seen, aggregations, joins);
                }
            }
            SummaryExpr::SummarySubtract { left, right }
            | SummaryExpr::SummaryJoin {
                outer: left,
                inner: right,
                ..
            } => {
                if matches!(&node.expr, SummaryExpr::SummaryJoin { .. }) {
                    joins.push(node);
                }
                visit(left, seen, aggregations, joins);
                visit(right, seen, aggregations, joins);
            }
            SummaryExpr::SummaryDelete { summary_input, .. }
            | SummaryExpr::SummaryEstimate { summary_input, .. } => {
                visit(summary_input, seen, aggregations, joins);
            }
            SummaryExpr::KeepPreAsap(_) => {}
        }
    }
    let mut aggregations = Vec::new();
    let mut joins = Vec::new();
    visit(root, &mut HashSet::new(), &mut aggregations, &mut joins);
    (aggregations, joins)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg(test)]
struct SummaryOperationCounts {
    state_builds: u64,
    merges_per_read: u64,
    subtracts_per_read: u64,
    deletes_per_update: u64,
    readouts_per_read: u64,
    joins_per_read: u64,
}

/// Low-level diagnostic for a homogeneous deployment. Final planner ranking
/// uses the per-node whole-DAG estimator above. Shared `Rc` nodes are visited
/// once; explicit delete frequency comes from deletion evidence.
#[cfg(test)]
fn estimate_incremental_summary_maintenance(
    root: &SummaryNode,
    guarantee: &SummaryMaintenanceLifecycleGuarantee,
    inputs: StreamingSummaryInputs,
    cpu: SummaryOperationCpuEvidence,
    scope: &ComparisonScope,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    estimate_incremental_summary_maintenance_with_join(root, guarantee, inputs, cpu, None, scope)
}

#[cfg(test)]
fn estimate_incremental_summary_maintenance_with_join(
    root: &SummaryNode,
    guarantee: &SummaryMaintenanceLifecycleGuarantee,
    inputs: StreamingSummaryInputs,
    cpu: SummaryOperationCpuEvidence,
    join: Option<SummaryJoinEvidence>,
    scope: &ComparisonScope,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    let inputs = inputs.validate()?;
    let evaluation_count = scope.validate()?;
    validate_guarantee(guarantee, scope.data_arrival)?;
    let (bootstrap_input_rows, arriving_input_rows, active_ms) =
        lifecycle_row_counts(inputs, guarantee, scope)?;
    let counts = count_operations(root)?;
    if counts.state_builds == 0 {
        return Err(AnalyticalCostError::UnsupportedCandidate);
    }

    let insert = required_cpu("insert_cpu_ops", cpu.insert_cpu_ops)?;
    let merge = required_cpu_when(counts.merges_per_read, "merge_cpu_ops", cpu.merge_cpu_ops)?;
    let subtract = required_cpu_when(
        counts.subtracts_per_read,
        "subtract_cpu_ops",
        cpu.subtract_cpu_ops,
    )?;
    let delete = required_cpu_when(
        counts.deletes_per_update,
        "delete_cpu_ops",
        cpu.delete_cpu_ops,
    )?;
    let delete_events = if counts.deletes_per_update == 0 {
        0_u64
    } else {
        let rate = cpu
            .delete_events_per_second
            .filter(|rate| rate.is_finite() && *rate >= 0.0)
            .ok_or(AnalyticalCostError::MissingOrStale(
                "delete_events_per_second",
            ))?;
        let fanout = cpu
            .delete_routing_fanout
            .filter(|fanout| *fanout > 0)
            .ok_or(AnalyticalCostError::MissingOrStale("delete_routing_fanout"))?;
        let events = (rate * active_ms as f64 / 1_000.0).ceil();
        if !events.is_finite() || events > u64::MAX as f64 {
            return Err(AnalyticalCostError::Overflow);
        }
        (events as u64)
            .checked_mul(fanout)
            .ok_or(AnalyticalCostError::Overflow)?
    };
    let readout = required_cpu_when(
        counts.readouts_per_read,
        "readout_cpu_ops",
        cpu.readout_cpu_ops,
    )?;
    let join_cpu = match (counts.joins_per_read, join.as_ref()) {
        (0, _) => 0.0,
        (_, Some(evidence))
            if evidence.cpu_ops_per_execution.is_finite()
                && evidence.cpu_ops_per_execution > 0.0
                && evidence.working_memory_bytes > 0 =>
        {
            evidence.cpu_ops_per_execution
        }
        (_, Some(evidence))
            if !evidence.cpu_ops_per_execution.is_finite()
                || evidence.cpu_ops_per_execution <= 0.0 =>
        {
            return Err(AnalyticalCostError::InvalidOperationCost(
                "summary_join_cpu_ops_per_execution",
                evidence.cpu_ops_per_execution,
            ));
        }
        _ => return Err(AnalyticalCostError::MissingOrStale("summary_join")),
    };

    let build_inserts = bootstrap_input_rows
        .checked_mul(inputs.bootstrap_window_count)
        .ok_or(AnalyticalCostError::Overflow)?
        .checked_mul(counts.state_builds)
        .ok_or(AnalyticalCostError::Overflow)?;
    let update_inserts = arriving_input_rows
        .checked_mul(inputs.active_window_count)
        .and_then(|n| n.checked_mul(counts.state_builds))
        .ok_or(AnalyticalCostError::Overflow)?;
    let instances = inputs.physical_summary_count as f64;
    let evaluations = evaluation_count as f64;
    let cpu_ops = (build_inserts as f64 + update_inserts as f64) * insert
        + evaluations * counts.merges_per_read as f64 * instances * merge
        + evaluations * counts.subtracts_per_read as f64 * instances * subtract
        + delete_events as f64 * counts.deletes_per_update as f64 * delete
        + evaluations * counts.readouts_per_read as f64 * instances * readout
        + evaluations * counts.joins_per_read as f64 * join_cpu;
    if !cpu_ops.is_finite() {
        return Err(AnalyticalCostError::Overflow);
    }

    let state_instances = inputs
        .active_window_count
        .checked_add(inputs.retained_window_count)
        .and_then(|n| n.checked_mul(inputs.physical_summary_count))
        .and_then(|n| n.checked_mul(counts.state_builds))
        .ok_or(AnalyticalCostError::Overflow)?;
    let retained_bytes = state_instances
        .checked_mul(inputs.state_bytes_per_summary)
        .ok_or(AnalyticalCostError::Overflow)?;
    // Merge/subtract may stream over persistent inputs but still needs one
    // result state per physical instance. Persistent retained windows are
    // already included above and are not loaded a second time.
    let transient_bytes = if counts.merges_per_read > 0 || counts.subtracts_per_read > 0 {
        inputs
            .physical_summary_count
            .checked_mul(inputs.state_bytes_per_summary)
            .ok_or(AnalyticalCostError::Overflow)?
    } else {
        0
    };
    let join_bytes = match (counts.joins_per_read, join.as_ref()) {
        (0, _) => 0,
        (_, Some(evidence)) => evidence.working_memory_bytes,
        _ => return Err(AnalyticalCostError::MissingOrStale("summary_join")),
    };
    let bootstrap_row_buffer = if inputs.initial_input_rows == 0 {
        0
    } else {
        inputs
            .initial_input_bytes
            .div_ceil(inputs.initial_input_rows)
    };
    Ok(ResourceEstimate {
        cpu_ops,
        peak_memory_bytes: retained_bytes
            .checked_add(transient_bytes)
            .and_then(|bytes| bytes.checked_add(join_bytes))
            .ok_or(AnalyticalCostError::Overflow)?
            .max(bootstrap_row_buffer),
        scan_bytes: inputs.initial_source_scan_bytes,
    })
}

fn lifecycle_row_counts(
    inputs: StreamingSummaryInputs,
    guarantee: &SummaryMaintenanceLifecycleGuarantee,
    scope: &ComparisonScope,
) -> Result<(u64, u64, u64), AnalyticalCostError> {
    let horizon_end = scope
        .planning_time
        .0
        .checked_add(scope.horizon.0)
        .ok_or(AnalyticalCostError::Overflow)?;
    let (bootstrap_extra_ms, active_ms) = match guarantee.summary_maintenance_lifecycle {
        SummaryMaintenanceLifecycle::Prepared {
            activate_at,
            retire_at,
        } => {
            if activate_at.0 >= retire_at.0 {
                return Err(AnalyticalCostError::IncompatibleLifecycleGuarantee);
            }
            let covers_every_evaluation = evaluation_offsets_ms(scope)?.into_iter().all(|offset| {
                scope
                    .planning_time
                    .0
                    .checked_add(offset)
                    .is_some_and(|at| at >= activate_at.0 && at < retire_at.0)
            });
            if !covers_every_evaluation {
                return Err(AnalyticalCostError::IncompatibleLifecycleGuarantee);
            }
            let activation = activate_at.0.max(scope.planning_time.0).min(horizon_end);
            let bootstrap_extra_ms = activation.saturating_sub(scope.planning_time.0);
            let start = activation;
            let end = retire_at.0.min(horizon_end);
            (bootstrap_extra_ms, end.saturating_sub(start))
        }
        SummaryMaintenanceLifecycle::Shared { .. } => (0, scope.horizon.0),
        SummaryMaintenanceLifecycle::ContinuouslyMaintained => (0, scope.horizon.0),
        SummaryMaintenanceLifecycle::Ephemeral => {
            return Err(AnalyticalCostError::IncompatibleLifecycleGuarantee)
        }
    };
    let bootstrap_extra = inputs.ingestion_rate_per_second * bootstrap_extra_ms as f64 / 1000.0;
    let updates = inputs.ingestion_rate_per_second * active_ms as f64 / 1000.0;
    if !bootstrap_extra.is_finite()
        || !updates.is_finite()
        || bootstrap_extra > u64::MAX as f64
        || updates > u64::MAX as f64
    {
        return Err(AnalyticalCostError::Overflow);
    }
    Ok((
        inputs
            .initial_input_rows
            .checked_add(bootstrap_extra.ceil() as u64)
            .ok_or(AnalyticalCostError::Overflow)?,
        updates.ceil() as u64,
        active_ms,
    ))
}

fn validate_guarantee(
    guarantee: &SummaryMaintenanceLifecycleGuarantee,
    arrival: DataArrival,
) -> Result<(), AnalyticalCostError> {
    if guarantee.output_representation != asap_types::post_asap::OutputRepresentation::SummaryState
        || guarantee.summary_maintenance_mode
            != maintenance_mode(&guarantee.summary_maintenance_lifecycle, arrival)
        || guarantee.evaluation_schedule
            != evaluation_schedule(&guarantee.summary_maintenance_lifecycle, arrival)
    {
        return Err(AnalyticalCostError::IncompatibleLifecycleGuarantee);
    }
    Ok(())
}

#[cfg(test)]
fn required_cpu(name: &'static str, value: Option<f64>) -> Result<f64, AnalyticalCostError> {
    let value = value.ok_or(AnalyticalCostError::MissingOrStale(name))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(AnalyticalCostError::InvalidOperationCost(name, value));
    }
    Ok(value)
}

fn validated_operator_cpu(name: &'static str, value: f64) -> Result<f64, AnalyticalCostError> {
    if !value.is_finite() || value <= 0.0 {
        Err(AnalyticalCostError::InvalidOperationCost(name, value))
    } else {
        Ok(value)
    }
}

fn validated_operator_executions(
    name: &'static str,
    evidence: &SummaryOperatorResourceEvidence,
) -> Result<u64, AnalyticalCostError> {
    if evidence.executions_per_evaluation == 0 {
        return Err(AnalyticalCostError::MissingOrZero(name));
    }
    Ok(evidence.executions_per_evaluation)
}

#[cfg(test)]
fn required_cpu_when(
    count: u64,
    name: &'static str,
    value: Option<f64>,
) -> Result<f64, AnalyticalCostError> {
    if count == 0 {
        return Ok(0.0);
    }
    required_cpu(name, value)
}

#[cfg(test)]
fn count_operations(root: &SummaryNode) -> Result<SummaryOperationCounts, AnalyticalCostError> {
    fn visit(
        node: &SummaryNode,
        seen: &mut HashSet<*const SummaryNode>,
        counts: &mut SummaryOperationCounts,
    ) -> Result<(), AnalyticalCostError> {
        if !seen.insert(node as *const SummaryNode) {
            return Ok(());
        }
        match &node.expr {
            SummaryExpr::KeepPreAsap(_) => {}
            SummaryExpr::SummaryAgg { child, .. } => {
                counts.state_builds = counts
                    .state_builds
                    .checked_add(1)
                    .ok_or(AnalyticalCostError::Overflow)?;
                visit(child, seen, counts)?;
            }
            SummaryExpr::SummaryMerge { children } => {
                if children.is_empty() {
                    return Err(AnalyticalCostError::InvalidPhysicalDag(
                        "summary merge has no children",
                    ));
                }
                counts.merges_per_read = counts
                    .merges_per_read
                    .checked_add(children.len().saturating_sub(1) as u64)
                    .ok_or(AnalyticalCostError::Overflow)?;
                for child in children {
                    visit(child, seen, counts)?;
                }
            }
            SummaryExpr::SummarySubtract { left, right } => {
                counts.subtracts_per_read = counts
                    .subtracts_per_read
                    .checked_add(1)
                    .ok_or(AnalyticalCostError::Overflow)?;
                visit(left, seen, counts)?;
                visit(right, seen, counts)?;
            }
            SummaryExpr::SummaryDelete { summary_input, .. } => {
                counts.deletes_per_update = counts
                    .deletes_per_update
                    .checked_add(1)
                    .ok_or(AnalyticalCostError::Overflow)?;
                visit(summary_input, seen, counts)?;
            }
            SummaryExpr::SummaryEstimate { summary_input, .. } => {
                counts.readouts_per_read = counts
                    .readouts_per_read
                    .checked_add(1)
                    .ok_or(AnalyticalCostError::Overflow)?;
                visit(summary_input, seen, counts)?;
            }
            SummaryExpr::SummaryJoin { outer, inner, .. } => {
                counts.joins_per_read = counts
                    .joins_per_read
                    .checked_add(1)
                    .ok_or(AnalyticalCostError::Overflow)?;
                visit(outer, seen, counts)?;
                visit(inner, seen, counts)?;
            }
        }
        Ok(())
    }

    let mut counts = SummaryOperationCounts::default();
    visit(root, &mut HashSet::new(), &mut counts)?;
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use asap_types::post_asap::{
        EvaluationSchedule, ExactKind, ExactParams, GroupingStrategy, OutputRepresentation,
        SummaryExpr, SummaryFamilyType, SummaryField, SummaryMaintenanceLifecycle,
        SummaryMaintenanceLifecycleGuarantee, SummaryMaintenanceMode, SummarySchema,
    };
    use asap_types::pre_asap::{
        agg_intent::AggIntent, Column, ColumnRef, DataType, QueryExpr, Reduction, Schema, Source,
    };
    use asap_types::workload::{
        DataWorkload, Evidence, EvidenceSource, Predictability, Query, QueryLanguage,
        QueryRecurrence, QueryRequirements, QueryTimeScope, QueryWorkload, QueryWorkloadEntry,
        Rate, RepeatedDemand, RepeatingEntry, RepetitionInterval, TimeSelection,
    };

    use super::*;
    use crate::recurrence::Horizon;
    use crate::summary_maintenance_lifecycle::{
        global_selection_with_summary_maintenance_lifecycles,
        materialize_with_summary_maintenance_lifecycles, plan_summary_maintenance_lifecycles,
        SummaryMaintenanceLifecycleCapabilities, WorkloadDemand,
    };

    fn estimate_test(
        root: &SummaryNode,
        guarantee: &SummaryMaintenanceLifecycleGuarantee,
        inputs: StreamingSummaryInputs,
        cpu: SummaryOperationCpuEvidence,
    ) -> Result<ResourceEstimate, AnalyticalCostError> {
        estimate_incremental_summary_maintenance(root, guarantee, inputs, cpu, &streaming_scope())
    }

    fn estimate_join_test(
        root: &SummaryNode,
        guarantee: &SummaryMaintenanceLifecycleGuarantee,
        inputs: StreamingSummaryInputs,
        cpu: SummaryOperationCpuEvidence,
        join: Option<SummaryJoinEvidence>,
    ) -> Result<ResourceEstimate, AnalyticalCostError> {
        estimate_incremental_summary_maintenance_with_join(
            root,
            guarantee,
            inputs,
            cpu,
            join,
            &streaming_scope(),
        )
    }

    fn scope_for(
        data: &DataWorkload,
        query: &QueryWorkloadEntry,
        planning_time_ms: u64,
        horizon_ms: u64,
    ) -> ComparisonScope {
        ComparisonScope::from_workload(
            data,
            query,
            asap_types::workload::TimestampMs(planning_time_ms),
            asap_types::workload::DurationMs(horizon_ms),
            vec![crate::physical_operator_statistics::SourceCoverage {
                source: Source::TimeSeries {
                    metric: "metrics".into(),
                },
                source_snapshot_id: "stream-start".into(),
                predicates: vec![],
                info_matchers: vec![],
            }],
        )
        .unwrap()
    }

    fn physical() -> StreamingPhysicalInputEvidence {
        StreamingPhysicalInputEvidence {
            initial_input_bytes: 640,
            initial_source_scan_bytes: 640,
            active_window_count: 2,
            bootstrap_window_count: 1,
            retained_window_count: 3,
            physical_summary_count: 2,
            state_bytes_per_summary: 100,
        }
    }

    fn query() -> QueryWorkloadEntry {
        QueryWorkloadEntry {
            query: Query("streaming count".into()),
            requirements: QueryRequirements::default(),
            predictability: Predictability::Unknown,
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                RepetitionInterval(1_000),
            )),
            time_selection: TimeSelection {
                scope: QueryTimeScope::Unknown,
                lookback: None,
                as_of: None,
            },
        }
    }

    fn continuous_guarantee() -> SummaryMaintenanceLifecycleGuarantee {
        SummaryMaintenanceLifecycleGuarantee {
            summary_maintenance_lifecycle: SummaryMaintenanceLifecycle::ContinuouslyMaintained,
            summary_maintenance_mode: SummaryMaintenanceMode::Incremental,
            evaluation_schedule: EvaluationSchedule::PerUpdate,
            output_representation: OutputRepresentation::SummaryState,
        }
    }

    #[test]
    fn workload_adapter_derives_updates_and_reads_over_one_horizon() {
        let data = DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            ingestion_rate: Evidence {
                value: Some(Rate(2.0)),
                source: EvidenceSource::Observed,
                observed_at_ms: Some(100),
                valid_for_ms: Some(10_000),
            },
            input_cardinality: Evidence {
                value: Some(10),
                source: EvidenceSource::Observed,
                observed_at_ms: Some(100),
                valid_for_ms: Some(10_000),
            },
            ..DataWorkload::default()
        };

        let scope = scope_for(&data, &query(), 100, 5_000);
        let inputs = StreamingSummaryInputs::from_workload(physical(), &data, &scope).unwrap();
        assert_eq!(inputs.initial_input_rows, 10);
        assert_eq!(
            lifecycle_row_counts(inputs, &continuous_guarantee(), &scope)
                .unwrap()
                .1,
            10
        );
        assert_eq!(scope.validate().unwrap(), 5);
    }

    #[test]
    fn pure_streaming_can_bootstrap_from_an_empty_state() {
        let data = DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            ingestion_rate: Evidence {
                value: Some(Rate(2.0)),
                source: EvidenceSource::Declared,
                observed_at_ms: None,
                valid_for_ms: None,
            },
            input_cardinality: Evidence {
                value: Some(0),
                source: EvidenceSource::Declared,
                observed_at_ms: None,
                valid_for_ms: None,
            },
            ..DataWorkload::default()
        };
        let mut empty = physical();
        empty.initial_input_bytes = 0;
        empty.initial_source_scan_bytes = 0;
        let scope = scope_for(&data, &query(), 0, 5_000);
        let inputs = StreamingSummaryInputs::from_workload(empty, &data, &scope).unwrap();
        let estimate = estimate_test(
            &summary_with_operations(false, false, false),
            &continuous_guarantee(),
            inputs,
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(2.0),
                readout_cpu_ops: Some(1.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .unwrap();
        // 10 arrivals * 2 active windows * 2 insert ops + 5 reads * 2 summaries.
        assert_eq!(estimate.cpu_ops, 50.0);
        assert_eq!(estimate.scan_bytes, 0);
    }

    #[test]
    fn bootstrap_rows_and_bytes_must_be_present_together() {
        let mut inputs = StreamingSummaryInputs {
            initial_input_rows: 0,
            initial_input_bytes: 8,
            initial_source_scan_bytes: 0,
            ingestion_rate_per_second: 1.0,
            active_window_count: 1,
            bootstrap_window_count: 1,
            retained_window_count: 1,
            physical_summary_count: 1,
            state_bytes_per_summary: 8,
        };
        assert_eq!(
            inputs.validate(),
            Err(AnalyticalCostError::InconsistentBootstrapEvidence)
        );
        inputs.initial_input_rows = 1;
        inputs.initial_input_bytes = 0;
        assert_eq!(
            inputs.validate(),
            Err(AnalyticalCostError::InconsistentBootstrapEvidence)
        );
    }

    #[test]
    fn no_completed_windows_is_a_valid_streaming_deployment() {
        let mut inputs = streaming_inputs();
        inputs.retained_window_count = 0;
        assert!(inputs.validate().is_ok());
    }

    #[test]
    fn bootstrap_rows_are_routed_to_declared_window_assignments() {
        let mut inputs = streaming_inputs();
        inputs.ingestion_rate_per_second = 0.0;
        inputs.bootstrap_window_count = 3;
        let estimate = estimate_test(
            &summary_with_operations(false, false, false),
            &continuous_guarantee(),
            inputs,
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(2.0),
                readout_cpu_ops: Some(1.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .unwrap();
        // 10 bootstrap rows * 3 windows * 2 insert ops + 5 reads * 2 summaries.
        assert_eq!(estimate.cpu_ops, 70.0);
    }

    #[test]
    fn lifecycle_output_must_remain_summary_state() {
        let mut guarantee = continuous_guarantee();
        guarantee.output_representation = OutputRepresentation::FinalizedValue;
        assert_eq!(
            estimate_test(
                &summary_with_operations(false, false, false),
                &guarantee,
                streaming_inputs(),
                streaming_cpu(),
            ),
            Err(AnalyticalCostError::IncompatibleLifecycleGuarantee)
        );
    }

    #[test]
    fn existing_lifecycle_planner_selects_a_fully_costed_streaming_alternative() {
        let inputs = StreamingSummaryInputs {
            initial_input_rows: 10,
            initial_input_bytes: 640,
            initial_source_scan_bytes: 640,
            ingestion_rate_per_second: 2.0,
            active_window_count: 2,
            bootstrap_window_count: 1,
            retained_window_count: 3,
            physical_summary_count: 2,
            state_bytes_per_summary: 100,
        };
        let mut model = streaming_model();
        let workload = QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![RepeatingEntry {
                query: Query("streaming count".into()),
                demand: RepeatedDemand::FixedInterval(RepetitionInterval(1_000)),
                requirements: QueryRequirements::default(),
                predictability: Predictability::Predictable { known_at: None },
                time_selection: TimeSelection::default(),
            }]),
            data_workload: Some(DataWorkload {
                arrival: DataArrival::ContinuouslyIngesting,
                ingestion_rate: Evidence {
                    value: Some(Rate(2.0)),
                    source: EvidenceSource::Declared,
                    observed_at_ms: None,
                    valid_for_ms: None,
                },
                ..DataWorkload::default()
            }),
        };
        let root = summary_with_operations(false, false, false);
        let target = streaming_sum_query();
        bind_aggregations(&mut model, &target, &root, inputs, streaming_cpu());
        let plan = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        let selected = plan.deployments[0]
            .summary_maintenance_lifecycle_guarantee
            .as_ref()
            .unwrap();
        assert_eq!(
            selected.summary_maintenance_mode,
            SummaryMaintenanceMode::Incremental
        );
        assert!(matches!(
            selected.summary_maintenance_lifecycle,
            SummaryMaintenanceLifecycle::Shared { .. }
                | SummaryMaintenanceLifecycle::ContinuouslyMaintained
        ));
        assert!(plan.summary_total_cost.is_some());
        assert_eq!(model.raw_query_recompute_cost(&target), None);
    }

    #[test]
    fn complete_streaming_cost_can_select_an_ephemeral_direct_build() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = summary_with_operations(false, false, false);
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );

        let plan = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities {
                supports_ephemeral: true,
                supports_prepared: false,
                supports_shared: false,
                supports_continuously_maintained: false,
            },
            &model,
        )
        .unwrap();

        assert!(plan.summary_total_cost.is_some());
        assert!(matches!(
            plan.deployments[0]
                .summary_maintenance_lifecycle_guarantee
                .as_ref()
                .map(|guarantee| &guarantee.summary_maintenance_lifecycle),
            Some(SummaryMaintenanceLifecycle::Ephemeral)
        ));
    }

    #[test]
    fn complete_streaming_cost_ranks_provider_owned_physical_plans() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = summary_with_operations(false, false, false);
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );

        let mut high_retention = model.node_evidence.clone();
        for aggregate in high_retention.aggregations.values_mut() {
            aggregate.inputs.retained_window_count = 20;
        }
        let mut low_retention = model.node_evidence.clone();
        for aggregate in low_retention.aggregations.values_mut() {
            aggregate.inputs.retained_window_count = 2;
        }
        for alternative in [
            StreamingPhysicalPlanAlternative {
                physical_plan_id: "high-retention-layout".into(),
                node_evidence: high_retention,
            },
            StreamingPhysicalPlanAlternative {
                physical_plan_id: "low-retention-layout".into(),
                node_evidence: low_retention,
            },
        ] {
            model
                .bind_physical_plan_alternative(&target, &root, alternative)
                .unwrap();
        }

        let plan = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities {
                supports_ephemeral: false,
                supports_prepared: false,
                supports_shared: false,
                supports_continuously_maintained: true,
            },
            &model,
        )
        .unwrap();

        assert_eq!(
            plan.selected_physical_plan_id.as_deref(),
            Some("low-retention-layout")
        );
        assert_eq!(
            crate::summary_maintenance_dag_export::export_summary_maintenance_plan(&plan)
                .selected_physical_plan_id
                .as_deref(),
            Some("low-retention-layout")
        );
    }

    #[test]
    fn global_selection_compares_streaming_summary_and_raw_over_one_horizon() {
        let target = streaming_sum_query();
        let space = crate::replacement::search_workload(vec![("q", Rc::clone(&target))]);
        let workload = streaming_workload();
        let mut model = streaming_model();
        for group in space.groups() {
            for candidate in &group.candidates {
                if let Replacement::Summary(root) = &candidate.replacement {
                    bind_aggregations(
                        &mut model,
                        &group.target,
                        root,
                        streaming_inputs(),
                        streaming_cpu(),
                    );
                }
            }
        }
        let selection = global_selection_with_summary_maintenance_lifecycles(
            &space,
            &workload,
            &[0],
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        let plan = materialize_with_summary_maintenance_lifecycles(
            &selection,
            &space.roots[0].1,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap()
        .unwrap();
        assert!(!plan.selected_raw_recompute);
        assert_eq!(plan.raw_recompute_total_cost, Some(Cost(5_264.0)));

        let mut missing_baseline = model.clone();
        missing_baseline
            .target_comparisons
            .get_mut(&Rc::as_ptr(&space.roots[0].1))
            .unwrap()
            .raw
            .physical_dag
            .evidence
            .clear();
        let unavailable = global_selection_with_summary_maintenance_lifecycles(
            &space,
            &workload,
            &[0],
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &missing_baseline,
        )
        .unwrap();
        assert!(unavailable
            .for_target(&space.roots[0].1)
            .unwrap()
            .chosen
            .is_none());

        let mut raw_cheaper = model;
        for evidence in raw_cheaper.node_evidence.aggregations.values_mut() {
            evidence.insert_cpu_ops = 10_000.0;
        }
        let cheap_selection = global_selection_with_summary_maintenance_lifecycles(
            &space,
            &workload,
            &[0],
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &raw_cheaper,
        )
        .unwrap();
        assert!(cheap_selection
            .for_target(&space.roots[0].1)
            .unwrap()
            .chosen
            .is_none());
        let cheap_plan = materialize_with_summary_maintenance_lifecycles(
            &cheap_selection,
            &space.roots[0].1,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &raw_cheaper,
        )
        .unwrap()
        .unwrap();
        assert!(cheap_plan.selected_raw_recompute);
        assert_eq!(cheap_plan.raw_recompute_total_cost, Some(Cost(5_264.0)));
    }

    #[test]
    fn raw_evolution_is_bound_to_the_requested_target() {
        let target_a = streaming_sum_query();
        let target_b = streaming_sum_query();
        let root_a = summary_with_operations(false, false, false);
        let root_b = summary_with_operations(false, false, false);
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target_a,
            &root_a,
            streaming_inputs(),
            streaming_cpu(),
        );
        let mut faster = streaming_inputs();
        // Candidate-local intermediate cardinality is not the raw target's
        // planning-time cardinality and must not constrain its baseline.
        faster.initial_input_rows = 7;
        faster.initial_input_bytes = 448;
        faster.initial_source_scan_bytes = 448;
        faster.ingestion_rate_per_second = 4.0;
        bind_aggregations(&mut model, &target_b, &root_b, faster, streaming_cpu());
        model
            .target_comparisons
            .get_mut(&Rc::as_ptr(&target_b))
            .unwrap()
            .raw = {
            let mut raw = streaming_raw();
            raw.ingestion_rate_per_second = 4.0;
            let statistics = &mut raw
                .physical_dag
                .evidence
                .get_mut("raw-scan")
                .unwrap()
                .statistics;
            let OperatorStatistics::Scan {
                edges,
                source_read_bytes,
            } = statistics
            else {
                unreachable!()
            };
            *source_read_bytes = 7_040;
            edges.input = EdgeStatistics {
                rows: 110,
                bytes: 7_040,
            };
            edges.output = edges.input;
            raw
        };

        let a = model.raw_query_recompute_total_cost(&target_a, 5.0);
        let b = model.raw_query_recompute_total_cost(&target_b, 5.0);
        assert_eq!(a, Some(Cost(5_264.0)));
        assert!(b.unwrap().0 > a.unwrap().0);
        assert_eq!(model.raw_query_recompute_total_cost(&target_a, 5.0), a);
    }

    #[test]
    fn raw_validation_uses_reachable_nodes_and_allows_repeated_source_scans() {
        let target = streaming_sum_query();
        let root = summary_with_operations(false, false, false);
        let scope = streaming_scope();
        let mut raw = streaming_raw();
        let first_scan = raw.physical_dag.nodes[0].clone();
        let mut second_scan = first_scan.clone();
        second_scan.id = "raw-scan-2".into();
        let mut unreachable = first_scan.clone();
        unreachable.id = "unreachable-per-evaluation".into();
        unreachable.execution = ExecutionMultiplicity::PerEvaluation;
        raw.physical_dag.nodes = vec![
            first_scan,
            second_scan,
            unreachable,
            PhysicalDagNode {
                id: "raw-concat".into(),
                operator: PhysicalOperator::Concat,
                children: vec!["raw-scan".into(), "raw-scan-2".into()],
                source_coverage: None,
                output_buffer_bytes: 0,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::Once,
            },
        ];
        raw.physical_dag.root = "raw-concat".into();
        let scan_evidence = raw.physical_dag.evidence["raw-scan"].clone();
        let mut second_scan_evidence = scan_evidence.clone();
        second_scan_evidence.physical_id = "raw-scan-2".into();
        raw.physical_dag
            .evidence
            .insert("raw-scan-2".into(), second_scan_evidence);
        let mut unreachable_evidence = scan_evidence;
        unreachable_evidence.physical_id = "unreachable-per-evaluation".into();
        raw.physical_dag
            .evidence
            .insert("unreachable-per-evaluation".into(), unreachable_evidence);
        raw.physical_dag.evidence.insert(
            "raw-concat".into(),
            PhysicalNodeEvidence {
                physical_id: "raw-concat".into(),
                statistics: OperatorStatistics::Concat {
                    inputs: vec![
                        EdgeStatistics {
                            rows: 80,
                            bytes: 5_120,
                        },
                        EdgeStatistics {
                            rows: 80,
                            bytes: 5_120,
                        },
                    ],
                    output: EdgeStatistics {
                        rows: 160,
                        bytes: 10_240,
                    },
                    promql: None,
                },
                output_buffer_bytes: 0,
            },
        );
        let mut model = streaming_model();
        assert!(model
            .bind_candidate_comparison(&target, &root, scope, raw)
            .is_ok());
    }

    #[test]
    fn comparison_binding_is_transactional_and_shared_nodes_allow_two_targets() {
        let target_a = streaming_sum_query();
        let target_b = streaming_sum_query();
        let root = summary_with_operations(false, false, false);
        let mut model = streaming_model();
        let mut wrong_scope = streaming_scope();
        wrong_scope.sources[0].source = Source::TimeSeries {
            metric: "wrong".into(),
        };
        assert_eq!(
            model.bind_candidate_comparison(&target_a, &root, wrong_scope, streaming_raw(),),
            Err(AnalyticalCostError::ComparisonScopeMismatch(
                "raw target source lineage"
            ))
        );
        assert!(model.target_comparisons.is_empty());
        assert!(model.candidate_comparisons.is_empty());

        model
            .bind_candidate_comparison(&target_a, &root, streaming_scope(), streaming_raw())
            .unwrap();
        model
            .bind_candidate_comparison(&target_b, &root, streaming_scope(), streaming_raw())
            .unwrap();
        assert_eq!(model.candidate_comparisons.len(), 2);
    }

    #[test]
    fn target_scope_rejects_extra_sources_and_tracks_info_matchers() {
        let target = streaming_sum_query();
        let mut extra = streaming_scope();
        extra
            .sources
            .push(crate::physical_operator_statistics::SourceCoverage {
                source: Source::TimeSeries {
                    metric: "unused".into(),
                },
                source_snapshot_id: "stream-start".into(),
                predicates: vec![],
                info_matchers: vec![],
            });
        assert_eq!(
            validate_query_scope(&target, &extra),
            Err(AnalyticalCostError::ComparisonScopeMismatch(
                "raw target source lineage"
            ))
        );

        let selector = vec![InfoMatcher {
            label: "job".into(),
            op: CompareOpKind::Eq,
            value: "api".into(),
        }];
        let info_target = QueryExpr::PromqlInfoEnrich {
            selector: selector.clone(),
            child: target,
        };
        let mut info_scope = streaming_scope();
        info_scope
            .sources
            .push(crate::physical_operator_statistics::SourceCoverage {
                source: Source::TimeSeries {
                    metric: "target_info".into(),
                },
                source_snapshot_id: "info-start".into(),
                predicates: vec![],
                info_matchers: selector,
            });
        validate_query_scope(&info_target, &info_scope).unwrap();
        info_scope.sources[1].info_matchers[0].value = "worker".into();
        assert_eq!(
            validate_query_scope(&info_target, &info_scope),
            Err(AnalyticalCostError::ComparisonScopeMismatch(
                "raw target source lineage"
            ))
        );
    }

    #[test]
    fn delete_owner_must_be_the_unique_state_reachable_from_its_input() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = summary_with_operations(false, false, true);
        let mut cpu = streaming_cpu();
        cpu.delete_cpu_ops = Some(1.0);
        cpu.delete_events_per_second = Some(1.0);
        cpu.delete_routing_fanout = Some(1);
        let mut model = streaming_model();
        model.capabilities.delete = true;
        bind_aggregations(&mut model, &target, &root, streaming_inputs(), cpu);
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &root.expr else {
            unreachable!();
        };
        let delete_ptr = Rc::as_ptr(summary_input);
        let unrelated = summary_with_operations(false, false, false);
        let unrelated_agg = evidence_nodes(&unrelated).0[0] as *const _;
        model
            .node_evidence
            .operation_state_owners
            .insert(delete_ptr, unrelated_agg);

        let plan = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(plan.summary_total_cost, None);
    }

    #[test]
    fn summary_edge_and_io_evidence_fail_closed() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = summary_join();
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        let join = evidence_nodes(&root).1[0];
        model.node_evidence.joins.insert(
            join as *const _,
            SummaryJoinEvidence {
                physical_id: "join-edge".into(),
                inputs: vec![test_edge(), EdgeStatistics { rows: 2, bytes: 16 }],
                output: test_edge(),
                cpu_ops_per_execution: 1.0,
                working_memory_bytes: 1,
                output_buffer_bytes: 0,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
            },
        );
        let bad_edge = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(bad_edge.summary_total_cost, None);

        model
            .node_evidence
            .joins
            .get_mut(&(join as *const _))
            .unwrap()
            .inputs = vec![test_edge(), test_edge()];
        model
            .node_evidence
            .operations
            .get_mut(&Rc::as_ptr(&root))
            .unwrap()
            .resource_mut()
            .io_bytes_per_execution = None;
        let missing_io = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(missing_io.summary_total_cost, None);
    }

    #[test]
    fn summary_edges_io_and_physical_identity_fail_closed() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = summary_join();
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        let (_, joins) = evidence_nodes(&root);
        model.node_evidence.insert_join(
            &Rc::new(joins[0].clone()),
            SummaryJoinEvidence {
                physical_id: "unused".into(),
                inputs: vec![test_edge(), test_edge()],
                output: test_edge(),
                cpu_ops_per_execution: 1.0,
                working_memory_bytes: 1,
                output_buffer_bytes: 0,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
            },
        );
        // Bind the actual join, then make one parent input disagree with its
        // child's output.
        model.node_evidence.joins.insert(
            joins[0] as *const _,
            SummaryJoinEvidence {
                physical_id: "join-edge".into(),
                inputs: vec![test_edge(), EdgeStatistics { rows: 2, bytes: 16 }],
                output: test_edge(),
                cpu_ops_per_execution: 1.0,
                working_memory_bytes: 1,
                output_buffer_bytes: 0,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
            },
        );
        let bad_edge = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(bad_edge.summary_total_cost, None);

        model
            .node_evidence
            .joins
            .get_mut(&(joins[0] as *const _))
            .unwrap()
            .inputs = vec![test_edge(), test_edge()];
        model
            .node_evidence
            .operations
            .get_mut(&Rc::as_ptr(&root))
            .unwrap()
            .resource_mut()
            .io_bytes_per_execution = None;
        let missing_io = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(missing_io.summary_total_cost, None);
    }

    #[test]
    fn liveness_does_not_add_disjoint_execution_workspaces() {
        let target = streaming_sum_query();
        let root = summary_join();
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        let join = evidence_nodes(&root).1[0];
        model.node_evidence.joins.insert(
            join as *const _,
            SummaryJoinEvidence {
                physical_id: "huge-join".into(),
                inputs: vec![test_edge(), test_edge()],
                output: test_edge(),
                cpu_ops_per_execution: 1.0,
                working_memory_bytes: u64::MAX,
                output_buffer_bytes: 0,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
            },
        );
        model
            .node_evidence
            .operations
            .get_mut(&Rc::as_ptr(&root))
            .unwrap()
            .resource_mut()
            .working_memory_bytes = u64::MAX;
        assert_eq!(
            estimate_transient_liveness(&root, &model.node_evidence),
            Ok(u64::MAX)
        );
    }

    #[test]
    fn conflicting_evidence_cannot_alias_one_provider_physical_identity() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = summary_join();
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        let aggregations = evidence_nodes(&root).0;
        let first = aggregations[0] as *const _;
        let second = aggregations[1] as *const _;
        model
            .node_evidence
            .aggregations
            .get_mut(&first)
            .unwrap()
            .physical_id = "aliased-state".into();
        let second_evidence = model.node_evidence.aggregations.get_mut(&second).unwrap();
        second_evidence.physical_id = "aliased-state".into();
        second_evidence.insert_cpu_ops = 99.0;

        let plan = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(plan.summary_total_cost, None);
    }

    #[test]
    fn lifecycle_plan_does_not_fall_back_to_partial_agg_cost_for_a_join_root() {
        let workload = streaming_workload();
        let root = summary_join();
        let target = streaming_sum_query();
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        let plan = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(plan.deployments.len(), 2);
        assert_eq!(plan.summary_total_cost, None);

        let mut costed = model;
        let join_node = evidence_nodes(&root).1[0];
        costed.node_evidence.joins.insert(
            join_node as *const _,
            SummaryJoinEvidence {
                physical_id: "costed-join".into(),
                inputs: vec![test_edge(), test_edge()],
                output: test_edge(),
                cpu_ops_per_execution: 6.0,
                working_memory_bytes: 64,
                output_buffer_bytes: 64,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
            },
        );
        let costed_plan = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &costed,
        )
        .unwrap();
        assert!(costed_plan.summary_total_cost.is_some());
    }

    #[test]
    fn whole_dag_cost_requires_and_uses_each_rc_bound_state_evidence() {
        let workload = streaming_workload();
        let root = summary_join();
        let target = streaming_sum_query();
        let (aggregations, joins) = evidence_nodes(&root);
        let mut model = streaming_model();
        bind_comparison(&mut model, &target, &root);
        model.node_evidence.aggregations.insert(
            aggregations[0] as *const _,
            StreamingAggregateEvidence {
                physical_id: "left-state".into(),
                input: test_edge(),
                output: test_edge(),
                source_coverage_index: Some(0),
                bootstrap_read_identity: "left-bootstrap".into(),
                inputs: streaming_inputs(),
                insert_cpu_ops: streaming_cpu().insert_cpu_ops.unwrap(),
            },
        );
        let incomplete = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(incomplete.summary_total_cost, None);

        let mut second_inputs = streaming_inputs();
        second_inputs.state_bytes_per_summary = 250;
        let mut second_cpu = streaming_cpu();
        second_cpu.insert_cpu_ops = Some(5.0);
        model.node_evidence.aggregations.insert(
            aggregations[1] as *const _,
            StreamingAggregateEvidence {
                physical_id: "right-state".into(),
                input: test_edge(),
                output: test_edge(),
                source_coverage_index: Some(0),
                bootstrap_read_identity: "right-bootstrap".into(),
                inputs: second_inputs,
                insert_cpu_ops: second_cpu.insert_cpu_ops.unwrap(),
            },
        );
        model.node_evidence.joins.insert(
            joins[0] as *const _,
            SummaryJoinEvidence {
                physical_id: "join".into(),
                inputs: vec![test_edge(), test_edge()],
                output: test_edge(),
                cpu_ops_per_execution: 6.0,
                working_memory_bytes: 64,
                output_buffer_bytes: 64,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
            },
        );
        model.node_evidence.operations.insert(
            Rc::as_ptr(&root),
            StreamingSummaryOperatorEvidence::Readout(SummaryOperatorResourceEvidence {
                physical_id: "root-readout".into(),
                inputs: vec![test_edge()],
                output: test_edge(),
                cpu_ops: 3.0,
                working_memory_bytes: 0,
                output_buffer_bytes: 0,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
            }),
        );
        let complete = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert!(complete.summary_total_cost.is_some());

        model
            .node_evidence
            .operations
            .get_mut(&Rc::as_ptr(&root))
            .unwrap()
            .resource_mut()
            .working_memory_bytes = 128;
        let larger_workspace = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        // The join's 64-byte output remains live while the readout's workspace
        // is active. The join's execution workspace is released first.
        assert_eq!(
            larger_workspace.summary_total_cost.unwrap().0 - complete.summary_total_cost.unwrap().0,
            64.0
        );

        // Equal SourceCoverage does not imply that two independent state
        // builds share one physical read. Only a provider-owned read identity
        // permits scan de-duplication.
        let mut shared_read = model;
        for aggregate in shared_read.node_evidence.aggregations.values_mut() {
            aggregate.bootstrap_read_identity = "one-physical-read".into();
        }
        let shared = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &shared_read,
        )
        .unwrap();
        assert_eq!(
            larger_workspace.summary_total_cost.unwrap().0 - shared.summary_total_cost.unwrap().0,
            640.0
        );
    }

    #[test]
    fn planner_selects_an_abstract_window_framework_from_downstream_evidence() {
        let mut workload = streaming_workload();
        workload.repeating_queries.as_mut().unwrap()[0]
            .requirements
            .accuracy =
            asap_types::workload::AccuracyRequirement::Explicit(AccuracyTarget::EpsilonDelta {
                epsilon: 0.10,
                delta: 0.01,
            });
        let target = streaming_sum_query();
        let root = summary_with_operations(false, false, false);
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &root.expr else {
            unreachable!();
        };
        let windowed_summary = Rc::clone(summary_input);
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );

        let mut tumbling = model.node_evidence.clone();
        for aggregate in tumbling.aggregations.values_mut() {
            aggregate.inputs.active_window_count = 1;
            aggregate.inputs.retained_window_count = 20;
        }
        let mut sliding = model.node_evidence.clone();
        for aggregate in sliding.aggregations.values_mut() {
            aggregate.inputs.active_window_count = 10;
            aggregate.inputs.retained_window_count = 10;
        }
        let mut exponential_histogram = model.node_evidence.clone();
        for aggregate in exponential_histogram.aggregations.values_mut() {
            aggregate.inputs.active_window_count = 2;
            aggregate.inputs.retained_window_count = 2;
        }
        for candidate in [
            StreamingWindowFrameworkCandidate {
                physical_plan_id: "tumbling-v1".into(),
                assignments: vec![StreamingWindowFrameworkAssignment {
                    summary: Rc::clone(&windowed_summary),
                    framework: Some(SummaryWindowFramework::Tumbling),
                }],
                accuracy: StreamingWindowAccuracyEvidence::Exact,
                node_evidence: tumbling,
            },
            StreamingWindowFrameworkCandidate {
                physical_plan_id: "sliding-v1".into(),
                assignments: vec![StreamingWindowFrameworkAssignment {
                    summary: Rc::clone(&windowed_summary),
                    framework: Some(SummaryWindowFramework::Sliding),
                }],
                accuracy: StreamingWindowAccuracyEvidence::Exact,
                node_evidence: sliding,
            },
            StreamingWindowFrameworkCandidate {
                physical_plan_id: "eh-v1".into(),
                assignments: vec![StreamingWindowFrameworkAssignment {
                    summary: Rc::clone(&windowed_summary),
                    framework: Some(SummaryWindowFramework::ExponentialHistogram),
                }],
                accuracy: StreamingWindowAccuracyEvidence::ExponentialHistogram(
                    ExponentialHistogramAccuracyEvidence::UniversalGsum {
                        epsilon: 0.05,
                        failure_probability: 0.01,
                        range: ExponentialHistogramQueryRange::MostRecentWindow,
                    },
                ),
                node_evidence: exponential_histogram,
            },
        ] {
            model
                .bind_window_framework_candidate(&target, &root, candidate)
                .unwrap();
        }
        // Framework candidates are authoritative. Selection must not depend
        // on duplicating one arbitrary implementation into the legacy global
        // evidence map.
        model.node_evidence = StreamingNodeEvidence::default();

        let plan = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities {
                supports_ephemeral: false,
                supports_prepared: false,
                supports_shared: false,
                supports_continuously_maintained: true,
            },
            &model,
        )
        .unwrap();

        assert_eq!(
            plan.deployments[0].selected_window_framework,
            Some(SummaryWindowFramework::ExponentialHistogram)
        );
        let guarantee = plan.window_accuracy_guarantee.as_ref().unwrap();
        assert_eq!(guarantee.metric, ErrorMetric::RelativeValue);
        assert!((guarantee.bound.evaluate().unwrap() - 0.05).abs() < f64::EPSILON);
        let exported =
            crate::summary_maintenance_dag_export::export_summary_maintenance_plan(&plan);
        assert_eq!(
            exported.deployments[0].selected_window_framework,
            Some(SummaryWindowFramework::ExponentialHistogram)
        );
        assert_eq!(
            exported.window_accuracy_guarantee.unwrap().metric,
            ErrorMetric::RelativeValue
        );

        workload.repeating_queries.as_mut().unwrap()[0]
            .requirements
            .accuracy =
            asap_types::workload::AccuracyRequirement::Explicit(AccuracyTarget::Epsilon(0.01));
        let stricter = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities {
                supports_ephemeral: false,
                supports_prepared: false,
                supports_shared: false,
                supports_continuously_maintained: true,
            },
            &model,
        )
        .unwrap();
        assert_ne!(
            stricter.deployments[0].selected_window_framework,
            Some(SummaryWindowFramework::ExponentialHistogram)
        );
        assert!(stricter.window_accuracy_guarantee.unwrap().is_exact());
    }

    #[test]
    fn window_framework_candidates_require_unique_nonempty_planner_primitives() {
        let target = streaming_sum_query();
        let root = summary_with_operations(false, false, false);
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &root.expr else {
            unreachable!();
        };
        let windowed_summary = Rc::clone(summary_input);
        let mut model = streaming_model();
        bind_comparison(&mut model, &target, &root);

        let empty = model.bind_window_framework_candidate(
            &target,
            &root,
            StreamingWindowFrameworkCandidate {
                physical_plan_id: "invalid-extension".into(),
                assignments: vec![StreamingWindowFrameworkAssignment {
                    summary: Rc::clone(&windowed_summary),
                    framework: Some(SummaryWindowFramework::Extension("  ".into())),
                }],
                accuracy: StreamingWindowAccuracyEvidence::Exact,
                node_evidence: model.node_evidence.clone(),
            },
        );
        assert!(matches!(empty, Err(AnalyticalCostError::MissingOrZero(_))));

        let candidate = StreamingWindowFrameworkCandidate {
            physical_plan_id: "tumbling-v1".into(),
            assignments: vec![StreamingWindowFrameworkAssignment {
                summary: windowed_summary,
                framework: Some(SummaryWindowFramework::Tumbling),
            }],
            accuracy: StreamingWindowAccuracyEvidence::Exact,
            node_evidence: model.node_evidence.clone(),
        };
        model
            .bind_window_framework_candidate(&target, &root, candidate.clone())
            .unwrap();
        assert!(matches!(
            model.bind_window_framework_candidate(&target, &root, candidate),
            Err(AnalyticalCostError::ComparisonScopeMismatch(_))
        ));
    }

    #[test]
    fn one_physical_identity_cannot_alias_different_window_frameworks() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = summary_join();
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &root.expr else {
            unreachable!();
        };
        let SummaryExpr::SummaryJoin { outer, inner, .. } = &summary_input.expr else {
            unreachable!();
        };
        let aggregation_nodes = [Rc::clone(outer), Rc::clone(inner)];
        let (aggregations, joins) = evidence_nodes(&root);
        model.node_evidence.joins.insert(
            joins[0] as *const _,
            SummaryJoinEvidence {
                physical_id: "joined-readout".into(),
                inputs: vec![test_edge(), test_edge()],
                output: test_edge(),
                matched_state_pairs_per_evaluation: 1,
                cpu_ops_per_matched_pair: 1.0,
                working_memory_bytes: 8,
                output_buffer_bytes: 0,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
            },
        );

        let mut shared_aggregation =
            model.node_evidence.aggregations[&(aggregations[0] as *const _)].clone();
        shared_aggregation.physical_id = "shared-window-state".into();
        model
            .node_evidence
            .aggregations
            .insert(aggregations[0] as *const _, shared_aggregation.clone());
        model
            .node_evidence
            .aggregations
            .insert(aggregations[1] as *const _, shared_aggregation);

        let retained_children: Vec<_> = aggregation_nodes
            .iter()
            .map(|aggregate| match &aggregate.expr {
                SummaryExpr::SummaryAgg { child, .. } => Rc::clone(child),
                _ => unreachable!(),
            })
            .collect();
        let mut shared_retained =
            model.node_evidence.retained_queries[&Rc::as_ptr(&retained_children[0])].clone();
        shared_retained.physical_id = "shared-retained-input".into();
        for child in &retained_children {
            model
                .node_evidence
                .retained_queries
                .insert(Rc::as_ptr(child), shared_retained.clone());
        }

        let candidate = StreamingWindowFrameworkCandidate {
            assignments: vec![
                StreamingWindowFrameworkAssignment {
                    summary: Rc::clone(&aggregation_nodes[0]),
                    framework: Some(SummaryWindowFramework::Tumbling),
                },
                StreamingWindowFrameworkAssignment {
                    summary: Rc::clone(&aggregation_nodes[1]),
                    framework: Some(SummaryWindowFramework::Sliding),
                },
            ],
            accuracy: StreamingWindowAccuracyEvidence::Exact,
            node_evidence: model.node_evidence.clone(),
        };
        model
            .bind_window_framework_candidate(&target, &root, candidate)
            .unwrap();

        let plan = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(plan.summary_total_cost, None);
    }

    #[test]
    fn promsketch_eh_accuracy_composes_registered_full_and_subwindow_bounds() {
        let full = StreamingWindowAccuracyEvidence::ExponentialHistogram(
            ExponentialHistogramAccuracyEvidence::KllRank {
                eh_epsilon: 0.01,
                kll_epsilon: 0.02,
                failure_probability: 0.01,
                range: ExponentialHistogramQueryRange::MostRecentWindow,
            },
        )
        .guarantee(true)
        .unwrap();
        assert_eq!(full.metric, ErrorMetric::Rank);
        assert!((full.bound.evaluate().unwrap() - 0.04).abs() < f64::EPSILON);

        let subwindow = StreamingWindowAccuracyEvidence::ExponentialHistogram(
            ExponentialHistogramAccuracyEvidence::KllRank {
                eh_epsilon: 0.01,
                kll_epsilon: 0.02,
                failure_probability: 0.01,
                range: ExponentialHistogramQueryRange::SubWindow {
                    suffix_rows: 100,
                    query_rows: 25,
                },
            },
        )
        .guarantee(true)
        .unwrap();
        assert!((subwindow.bound.evaluate().unwrap() - 0.10).abs() < f64::EPSILON);

        let gsum = StreamingWindowAccuracyEvidence::ExponentialHistogram(
            ExponentialHistogramAccuracyEvidence::UniversalGsum {
                epsilon: 0.05,
                failure_probability: 0.30,
                range: ExponentialHistogramQueryRange::SubWindow {
                    suffix_rows: 100,
                    query_rows: 25,
                },
            },
        )
        .guarantee(true)
        .unwrap();
        assert_eq!(gsum.metric, ErrorMetric::RelativeValue);
        assert!((gsum.bound.evaluate().unwrap() - 0.20).abs() < f64::EPSILON);
    }

    #[test]
    fn exponential_histogram_without_registered_accuracy_composition_fails_closed() {
        let mut workload = streaming_workload();
        workload.repeating_queries.as_mut().unwrap()[0]
            .requirements
            .accuracy =
            asap_types::workload::AccuracyRequirement::Explicit(AccuracyTarget::Epsilon(1.0));
        let target = streaming_sum_query();
        let root = summary_with_operations(false, false, false);
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &root.expr else {
            unreachable!();
        };
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        model
            .bind_window_framework_candidate(
                &target,
                &root,
                StreamingWindowFrameworkCandidate {
                    assignments: vec![StreamingWindowFrameworkAssignment {
                        summary: Rc::clone(summary_input),
                        framework: Some(SummaryWindowFramework::ExponentialHistogram),
                    }],
                    accuracy: StreamingWindowAccuracyEvidence::Exact,
                    node_evidence: model.node_evidence.clone(),
                },
            )
            .unwrap();

        let plan = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(plan.summary_total_cost, None);
    }

    #[test]
    fn whole_dag_fails_closed_for_missing_retained_work_or_false_source_lineage() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = summary_with_operations(false, false, false);
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        model.node_evidence.retained_queries.clear();
        let missing_retained = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(missing_retained.summary_total_cost, None);

        bind_comparison(&mut model, &target, &root);
        model
            .target_comparisons
            .get_mut(&Rc::as_ptr(&target))
            .unwrap()
            .scope
            .sources[0]
            .source = Source::TimeSeries {
            metric: "other_metric".into(),
        };
        let false_lineage = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(false_lineage.summary_total_cost, None);
    }

    #[test]
    fn aggregate_recurses_into_child_operations_and_state_only_needs_no_readout() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let estimated = summary_with_operations(false, false, false);
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &estimated.expr else {
            unreachable!();
        };
        let state_only = Rc::clone(summary_input);
        let mut no_readout_cpu = streaming_cpu();
        no_readout_cpu.readout_cpu_ops = None;
        let mut state_model = streaming_model();
        bind_aggregations(
            &mut state_model,
            &target,
            &state_only,
            streaming_inputs(),
            no_readout_cpu,
        );
        let state_plan = plan_summary_maintenance_lifecycles(
            state_only,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &state_model,
        )
        .unwrap();
        assert!(state_plan.summary_total_cost.is_some());

        let child = summary_with_operations(true, false, false);
        let nested = Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryAgg {
                child,
                family: SummaryFamilyType::ExactAggregate(ExactKind::Count, ExactParams::Count),
                col: ColumnRef::Wildcard,
                reduction: Reduction::by(vec![]),
                grouping: GroupingStrategy::PerSubpopulationInstance,
            },
            schema: estimated.schema.clone(),
            guarantee: None,
        });
        let mut nested_cpu = streaming_cpu();
        nested_cpu.merge_cpu_ops = Some(1.0);
        let mut nested_model = streaming_model();
        bind_aggregations(
            &mut nested_model,
            &target,
            &nested,
            streaming_inputs(),
            nested_cpu,
        );
        nested_model
            .node_evidence
            .operations
            .retain(|_, operation| {
                operation.resource().cpu_ops != 1.0
                    || operation.resource().working_memory_bytes == 0
            });
        let nested_plan = plan_summary_maintenance_lifecycles(
            nested,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &nested_model,
        )
        .unwrap();
        assert_eq!(nested_plan.summary_total_cost, None);
    }

    #[test]
    fn mixed_arrival_fails_closed_until_backlog_and_stream_are_separate() {
        let data = DataWorkload {
            arrival: DataArrival::Mixed,
            ..DataWorkload::default()
        };
        let mut scope = streaming_scope();
        scope.data_arrival = DataArrival::Mixed;
        assert_eq!(
            StreamingSummaryInputs::from_workload(physical(), &data, &scope),
            Err(AnalyticalCostError::UnsupportedDataArrival(
                DataArrival::Mixed
            ))
        );
    }

    #[test]
    fn direct_read_costs_build_updates_windows_and_recurrence() {
        let estimate = estimate_test(
            &summary_with_operations(false, false, false),
            &continuous_guarantee(),
            StreamingSummaryInputs {
                initial_input_rows: 10,
                initial_input_bytes: 640,
                initial_source_scan_bytes: 640,
                ingestion_rate_per_second: 2.0,
                active_window_count: 2,
                bootstrap_window_count: 1,
                retained_window_count: 3,
                physical_summary_count: 2,
                state_bytes_per_summary: 100,
            },
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(2.0),
                readout_cpu_ops: Some(3.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .unwrap();
        // 10 bootstrap + 10 arrivals into two active windows; two states read 5 times.
        assert_eq!(estimate.cpu_ops, 90.0);
        assert_eq!(estimate.peak_memory_bytes, 1_000);
        assert_eq!(estimate.scan_bytes, 640);
    }

    #[test]
    fn operations_use_update_or_read_multiplicity_and_shared_state_once() {
        let estimate = estimate_test(
            &summary_with_operations(true, true, true),
            &continuous_guarantee(),
            StreamingSummaryInputs {
                initial_input_rows: 1,
                initial_input_bytes: 8,
                initial_source_scan_bytes: 8,
                ingestion_rate_per_second: 4.0,
                active_window_count: 1,
                bootstrap_window_count: 1,
                retained_window_count: 2,
                physical_summary_count: 2,
                state_bytes_per_summary: 10,
            },
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(1.0),
                merge_cpu_ops: Some(2.0),
                subtract_cpu_ops: Some(3.0),
                delete_cpu_ops: Some(5.0),
                delete_events_per_second: Some(4.0),
                delete_routing_fanout: Some(2),
                readout_cpu_ops: Some(7.0),
            },
        )
        .unwrap();
        assert_eq!(estimate.cpu_ops, 21.0 + 20.0 + 30.0 + 200.0 + 70.0);
        // Three persistent windows plus one transient result, for two instances.
        assert_eq!(estimate.peak_memory_bytes, 80);
    }

    #[test]
    fn lifecycle_mode_and_schedule_must_match_existing_planner_semantics() {
        let mut guarantee = continuous_guarantee();
        guarantee.evaluation_schedule = EvaluationSchedule::OnRead;
        assert_eq!(
            estimate_test(
                &summary_with_operations(false, false, false),
                &guarantee,
                StreamingSummaryInputs {
                    initial_input_rows: 1,
                    initial_input_bytes: 8,
                    initial_source_scan_bytes: 8,
                    ingestion_rate_per_second: 1.0,
                    active_window_count: 1,
                    bootstrap_window_count: 1,
                    retained_window_count: 1,
                    physical_summary_count: 1,
                    state_bytes_per_summary: 8,
                },
                SummaryOperationCpuEvidence {
                    insert_cpu_ops: Some(1.0),
                    readout_cpu_ops: Some(1.0),
                    ..SummaryOperationCpuEvidence::default()
                },
            ),
            Err(AnalyticalCostError::IncompatibleLifecycleGuarantee)
        );
    }

    #[test]
    fn missing_cost_for_an_operation_in_the_dag_fails_closed() {
        assert_eq!(
            estimate_test(
                &summary_with_operations(true, false, false),
                &continuous_guarantee(),
                StreamingSummaryInputs {
                    initial_input_rows: 1,
                    initial_input_bytes: 8,
                    initial_source_scan_bytes: 8,
                    ingestion_rate_per_second: 1.0,
                    active_window_count: 1,
                    bootstrap_window_count: 1,
                    retained_window_count: 1,
                    physical_summary_count: 1,
                    state_bytes_per_summary: 8,
                },
                SummaryOperationCpuEvidence {
                    insert_cpu_ops: Some(1.0),
                    readout_cpu_ops: Some(1.0),
                    ..SummaryOperationCpuEvidence::default()
                },
            ),
            Err(AnalyticalCostError::MissingOrStale("merge_cpu_ops"))
        );
    }

    #[test]
    fn direct_build_mode_is_not_mispriced_as_incremental_maintenance() {
        let mut guarantee = continuous_guarantee();
        guarantee.summary_maintenance_lifecycle = SummaryMaintenanceLifecycle::Ephemeral;
        guarantee.summary_maintenance_mode = SummaryMaintenanceMode::DirectBuild;
        guarantee.evaluation_schedule = EvaluationSchedule::OneShot;
        assert_eq!(
            estimate_test(
                &summary_with_operations(false, false, false),
                &guarantee,
                StreamingSummaryInputs {
                    initial_input_rows: 1,
                    initial_input_bytes: 8,
                    initial_source_scan_bytes: 8,
                    ingestion_rate_per_second: 1.0,
                    active_window_count: 1,
                    bootstrap_window_count: 1,
                    retained_window_count: 1,
                    physical_summary_count: 1,
                    state_bytes_per_summary: 8,
                },
                SummaryOperationCpuEvidence {
                    insert_cpu_ops: Some(1.0),
                    readout_cpu_ops: Some(1.0),
                    ..SummaryOperationCpuEvidence::default()
                },
            ),
            Err(AnalyticalCostError::IncompatibleLifecycleGuarantee)
        );
    }

    #[test]
    fn prepared_maintenance_charges_only_its_active_interval() {
        let guarantee = SummaryMaintenanceLifecycleGuarantee {
            summary_maintenance_lifecycle: SummaryMaintenanceLifecycle::Prepared {
                activate_at: asap_types::workload::TimestampMs(1_000),
                retire_at: asap_types::workload::TimestampMs(6_000),
            },
            summary_maintenance_mode: SummaryMaintenanceMode::Incremental,
            evaluation_schedule: EvaluationSchedule::PerUpdate,
            output_representation: OutputRepresentation::SummaryState,
        };
        let estimate = estimate_test(
            &summary_with_operations(false, false, false),
            &guarantee,
            StreamingSummaryInputs {
                initial_input_rows: 10,
                initial_input_bytes: 80,
                initial_source_scan_bytes: 80,
                ingestion_rate_per_second: 2.0,
                active_window_count: 1,
                bootstrap_window_count: 1,
                retained_window_count: 1,
                physical_summary_count: 1,
                state_bytes_per_summary: 8,
            },
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(1.0),
                readout_cpu_ops: Some(1.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .unwrap();
        // Two pre-activation arrivals join the bootstrap; eight more are
        // maintained through the horizon; five reads are served.
        assert_eq!(estimate.cpu_ops, 25.0);
    }

    #[test]
    fn shared_retention_is_not_the_comparison_horizon() {
        let guarantee = SummaryMaintenanceLifecycleGuarantee {
            summary_maintenance_lifecycle: SummaryMaintenanceLifecycle::Shared {
                retention: asap_types::workload::DurationMs(999),
            },
            summary_maintenance_mode: SummaryMaintenanceMode::Incremental,
            evaluation_schedule: EvaluationSchedule::PerUpdate,
            output_representation: OutputRepresentation::SummaryState,
        };
        assert!(estimate_test(
            &summary_with_operations(false, false, false),
            &guarantee,
            StreamingSummaryInputs {
                initial_input_rows: 1,
                initial_input_bytes: 8,
                initial_source_scan_bytes: 8,
                ingestion_rate_per_second: 1.0,
                active_window_count: 1,
                bootstrap_window_count: 1,
                retained_window_count: 1,
                physical_summary_count: 1,
                state_bytes_per_summary: 8,
            },
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(1.0),
                readout_cpu_ops: Some(1.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .is_ok());
    }

    #[test]
    fn lifecycle_retention_rate_integrates_to_one_peak_capacity_charge() {
        let target = streaming_sum_query();
        let root = summary_with_operations(false, false, false);
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        let aggregation = evidence_nodes(&root).0[0];
        let inputs = model
            .lifecycle_inputs(aggregation, Some(Horizon(5.0)))
            .unwrap();
        let integrated = inputs.retention_cost_rate.unwrap().0 * 5.0;
        // (2 active + 3 retained) * 2 states * 100 bytes, calibrated once.
        assert_eq!(integrated, 1_000.0);
    }

    #[test]
    fn summary_join_requires_cardinality_and_working_memory_evidence() {
        let joined = summary_join();
        let inputs = StreamingSummaryInputs {
            initial_input_rows: 1,
            initial_input_bytes: 8,
            initial_source_scan_bytes: 8,
            ingestion_rate_per_second: 1.0,
            active_window_count: 1,
            bootstrap_window_count: 1,
            retained_window_count: 1,
            physical_summary_count: 1,
            state_bytes_per_summary: 8,
        };
        let cpu = SummaryOperationCpuEvidence {
            insert_cpu_ops: Some(1.0),
            readout_cpu_ops: Some(1.0),
            ..SummaryOperationCpuEvidence::default()
        };
        assert_eq!(
            estimate_join_test(&joined, &continuous_guarantee(), inputs, cpu, None,),
            Err(AnalyticalCostError::MissingOrStale("summary_join"))
        );
        let estimate = estimate_join_test(
            &joined,
            &continuous_guarantee(),
            inputs,
            cpu,
            Some(SummaryJoinEvidence {
                physical_id: "diagnostic-join".into(),
                inputs: vec![test_edge(), test_edge()],
                output: test_edge(),
                cpu_ops_per_execution: 12.0,
                working_memory_bytes: 32,
                output_buffer_bytes: 0,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
            }),
        )
        .unwrap();
        assert_eq!(estimate.cpu_ops, 77.0);
        assert_eq!(estimate.peak_memory_bytes, 64); // 4 persistent states + join memory.
    }

    fn summary_with_operations(merge: bool, subtract: bool, delete: bool) -> Rc<SummaryNode> {
        let state_type = SummaryFamilyType::ExactAggregate(ExactKind::Count, ExactParams::Count);
        let schema = SummarySchema {
            fields: vec![SummaryField {
                name: "count".into(),
                dtype: state_type.clone(),
                nullable: false,
            }],
            time_index: None,
        };
        let leaf = Rc::new(SummaryNode {
            expr: SummaryExpr::KeepPreAsap(Rc::new(QueryExpr::Scan {
                source: Source::TimeSeries {
                    metric: "metrics".into(),
                },
                predicates: vec![],
                schema: Schema::with_time_index(
                    vec![
                        Column::new("ts", DataType::Timestamp, false),
                        Column::new("value", DataType::Float64, false),
                    ],
                    0,
                    vec![],
                ),
            })),
            schema: schema.clone(),
            guarantee: None,
        });
        let agg = Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryAgg {
                child: leaf,
                family: state_type,
                col: ColumnRef::Wildcard,
                reduction: Reduction::by(vec![]),
                grouping: GroupingStrategy::PerSubpopulationInstance,
            },
            schema: schema.clone(),
            guarantee: None,
        });
        let mut root = Rc::clone(&agg);
        if merge {
            root = Rc::new(SummaryNode {
                expr: SummaryExpr::SummaryMerge {
                    children: vec![Rc::clone(&agg), Rc::clone(&agg)],
                },
                schema: schema.clone(),
                guarantee: None,
            });
        }
        if subtract {
            root = Rc::new(SummaryNode {
                expr: SummaryExpr::SummarySubtract {
                    left: Rc::clone(&root),
                    right: Rc::clone(&agg),
                },
                schema: schema.clone(),
                guarantee: None,
            });
        }
        if delete {
            root = Rc::new(SummaryNode {
                expr: SummaryExpr::SummaryDelete {
                    summary_input: root,
                    key: ColumnRef::Wildcard,
                },
                schema: schema.clone(),
                guarantee: None,
            });
        }
        Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryEstimate {
                summary_input: root,
                query: asap_types::post_asap::SketchQuery::PointCount {
                    key: ColumnRef::Wildcard,
                    value: None,
                },
            },
            schema,
            guarantee: None,
        })
    }

    fn summary_join() -> Rc<SummaryNode> {
        let left = summary_with_operations(false, false, false);
        let right = summary_with_operations(false, false, false);
        let SummaryExpr::SummaryEstimate {
            summary_input: left,
            ..
        } = &left.expr
        else {
            unreachable!()
        };
        let SummaryExpr::SummaryEstimate {
            summary_input: right,
            ..
        } = &right.expr
        else {
            unreachable!()
        };
        let schema = left.schema.clone();
        let join = Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryJoin {
                outer: Rc::clone(left),
                inner: Rc::clone(right),
                key: ColumnRef::Wildcard,
                family: SummaryFamilyType::ExactAggregate(ExactKind::Count, ExactParams::Count),
            },
            schema: schema.clone(),
            guarantee: None,
        });
        Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryEstimate {
                summary_input: join,
                query: asap_types::post_asap::SketchQuery::PointCount {
                    key: ColumnRef::Wildcard,
                    value: None,
                },
            },
            schema,
            guarantee: None,
        })
    }

    fn streaming_sum_query() -> Rc<QueryExpr> {
        let scan = Rc::new(QueryExpr::Scan {
            source: Source::TimeSeries {
                metric: "metrics".into(),
            },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        });
        Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::by(vec![]),
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![],
            having: None,
            child: scan,
        })
    }

    fn streaming_workload() -> QueryWorkload {
        QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![RepeatingEntry {
                query: Query("sum(metrics)".into()),
                demand: RepeatedDemand::FixedInterval(RepetitionInterval(1_000)),
                requirements: QueryRequirements::default(),
                predictability: Predictability::Predictable { known_at: None },
                time_selection: TimeSelection::default(),
            }]),
            data_workload: Some(DataWorkload {
                arrival: DataArrival::ContinuouslyIngesting,
                ingestion_rate: Evidence {
                    value: Some(Rate(2.0)),
                    source: EvidenceSource::Declared,
                    observed_at_ms: None,
                    valid_for_ms: None,
                },
                ..DataWorkload::default()
            }),
        }
    }

    fn streaming_model() -> SummaryMaintenanceCostModel {
        SummaryMaintenanceCostModel::new(
            ResourceCalibration {
                cost_per_cpu_op: 1.0,
                cost_per_scan_byte: 1.0,
                cost_per_retained_byte: 1.0,
                version: "test".into(),
            },
            SummaryMaintenanceCapabilities {
                incremental_update: true,
                merge: false,
                delete: false,
            },
        )
    }

    fn streaming_raw() -> StreamingRawInputEvidence {
        let scope = streaming_scope();
        let node = PhysicalDagNode {
            id: "raw-scan".into(),
            operator: PhysicalOperator::Scan,
            children: vec![],
            source_coverage: Some(scope.sources[0].clone()),
            output_buffer_bytes: 0,
            retained_bytes: 0,
            execution: ExecutionMultiplicity::Once,
        };
        let edge = EdgeStatistics {
            rows: 80,
            bytes: 5_120,
        };
        let statistics = OperatorStatistics::Scan {
            source_read_bytes: 5_120,
            edges: UnaryEdgeStatistics {
                input: edge,
                output: edge,
                promql: None,
            },
        };
        StreamingRawInputEvidence {
            planning_time_input_rows: 10,
            planning_time_input_bytes: 640,
            planning_time_source_scan_bytes: 640,
            arriving_logical_row_bytes: 64,
            arriving_source_row_bytes: 64,
            ingestion_rate_per_second: 2.0,
            physical_dag: EvidenceBackedPhysicalDag {
                nodes: vec![node],
                root: "raw-scan".into(),
                evidence: HashMap::from([(
                    "raw-scan".into(),
                    PhysicalNodeEvidence {
                        physical_id: "raw-scan".into(),
                        statistics,
                        output_buffer_bytes: 0,
                    },
                )]),
            },
        }
    }

    fn bind_comparison(
        model: &mut SummaryMaintenanceCostModel,
        target: &Rc<QueryExpr>,
        root: &Rc<SummaryNode>,
    ) {
        model
            .bind_candidate_comparison(target, root, streaming_scope(), streaming_raw())
            .unwrap();
        fn retained(
            model: &mut SummaryMaintenanceCostModel,
            node: &Rc<SummaryNode>,
            seen: &mut HashSet<*const SummaryNode>,
        ) {
            if !seen.insert(Rc::as_ptr(node)) {
                return;
            }
            match &node.expr {
                SummaryExpr::KeepPreAsap(_) => {
                    model.node_evidence.insert_retained_query(
                        node,
                        StreamingRetainedQueryEvidence {
                            physical_id: format!("retained-{node:p}"),
                            output: test_edge(),
                            preprocessing_cpu_ops_over_horizon: 1.0,
                            working_memory_bytes: 8,
                            output_buffer_bytes: 0,
                        },
                    );
                }
                SummaryExpr::SummaryAgg { child, .. } => retained(model, child, seen),
                SummaryExpr::SummaryMerge { children } => {
                    for child in children {
                        retained(model, child, seen);
                    }
                }
                SummaryExpr::SummarySubtract { left, right }
                | SummaryExpr::SummaryJoin {
                    outer: left,
                    inner: right,
                    ..
                } => {
                    retained(model, left, seen);
                    retained(model, right, seen);
                }
                SummaryExpr::SummaryDelete { summary_input, .. }
                | SummaryExpr::SummaryEstimate { summary_input, .. } => {
                    retained(model, summary_input, seen)
                }
            }
        }
        retained(model, root, &mut HashSet::new());
    }

    fn streaming_inputs() -> StreamingSummaryInputs {
        StreamingSummaryInputs {
            initial_input_rows: 10,
            initial_input_bytes: 640,
            initial_source_scan_bytes: 640,
            ingestion_rate_per_second: 2.0,
            active_window_count: 2,
            bootstrap_window_count: 1,
            retained_window_count: 3,
            physical_summary_count: 2,
            state_bytes_per_summary: 100,
        }
    }

    fn test_edge() -> EdgeStatistics {
        EdgeStatistics { rows: 1, bytes: 8 }
    }

    fn streaming_cpu() -> SummaryOperationCpuEvidence {
        SummaryOperationCpuEvidence {
            insert_cpu_ops: Some(2.0),
            readout_cpu_ops: Some(3.0),
            ..SummaryOperationCpuEvidence::default()
        }
    }

    fn bind_aggregations(
        model: &mut SummaryMaintenanceCostModel,
        target: &Rc<QueryExpr>,
        root: &Rc<SummaryNode>,
        inputs: StreamingSummaryInputs,
        cpu: SummaryOperationCpuEvidence,
    ) {
        bind_comparison(model, target, root);
        for node in evidence_nodes(root).0 {
            let source_root = matches!(
                &node.expr,
                SummaryExpr::SummaryAgg { child, .. }
                    if matches!(child.expr, SummaryExpr::KeepPreAsap(_))
            );
            let mut node_inputs = inputs;
            if !source_root {
                node_inputs.initial_input_rows = test_edge().rows;
                node_inputs.initial_input_bytes = test_edge().bytes;
                node_inputs.initial_source_scan_bytes = 0;
            }
            model.node_evidence.aggregations.insert(
                node as *const _,
                StreamingAggregateEvidence {
                    physical_id: format!("agg-{node:p}"),
                    input: test_edge(),
                    output: test_edge(),
                    source_coverage_index: source_root.then_some(0),
                    bootstrap_read_identity: if source_root {
                        "shared-bootstrap".into()
                    } else {
                        String::new()
                    },
                    inputs: node_inputs,
                    insert_cpu_ops: cpu.insert_cpu_ops.unwrap(),
                },
            );
        }
        fn bind_ops(
            model: &mut SummaryMaintenanceCostModel,
            node: &SummaryNode,
            seen: &mut HashSet<*const SummaryNode>,
            inputs: StreamingSummaryInputs,
            cpu: SummaryOperationCpuEvidence,
        ) {
            if !seen.insert(node as *const _) {
                return;
            }
            let operation = match &node.expr {
                SummaryExpr::SummaryMerge { .. } => cpu.merge_cpu_ops.map(|cpu_ops| {
                    StreamingSummaryOperatorEvidence::Merge(SummaryOperatorResourceEvidence {
                        physical_id: format!("merge-{node:p}"),
                        inputs: match &node.expr {
                            SummaryExpr::SummaryMerge { children } => {
                                vec![test_edge(); children.len()]
                            }
                            _ => unreachable!(),
                        },
                        output: test_edge(),
                        cpu_ops,
                        working_memory_bytes: inputs.state_bytes_per_summary,
                        output_buffer_bytes: 0,
                        executions_per_evaluation: 1,
                        io_bytes_per_execution: Some(0),
                    })
                }),
                SummaryExpr::SummarySubtract { .. } => cpu.subtract_cpu_ops.map(|cpu_ops| {
                    StreamingSummaryOperatorEvidence::Subtract(SummaryOperatorResourceEvidence {
                        physical_id: format!("subtract-{node:p}"),
                        inputs: vec![test_edge(), test_edge()],
                        output: test_edge(),
                        cpu_ops,
                        working_memory_bytes: inputs.state_bytes_per_summary,
                        output_buffer_bytes: 0,
                        executions_per_evaluation: 1,
                        io_bytes_per_execution: Some(0),
                    })
                }),
                SummaryExpr::SummaryDelete { .. } => cpu.delete_cpu_ops.and_then(|cpu_ops| {
                    Some(StreamingSummaryOperatorEvidence::Delete {
                        resource: SummaryOperatorResourceEvidence {
                            physical_id: format!("delete-{node:p}"),
                            inputs: vec![test_edge()],
                            output: test_edge(),
                            cpu_ops,
                            working_memory_bytes: 0,
                            output_buffer_bytes: 0,
                            executions_per_evaluation: 1,
                            io_bytes_per_execution: Some(0),
                        },
                        events_per_second: cpu.delete_events_per_second?,
                        routing_fanout: cpu.delete_routing_fanout?,
                    })
                }),
                SummaryExpr::SummaryEstimate { .. } => cpu.readout_cpu_ops.map(|cpu_ops| {
                    StreamingSummaryOperatorEvidence::Readout(SummaryOperatorResourceEvidence {
                        physical_id: format!("readout-{node:p}"),
                        inputs: vec![test_edge()],
                        output: test_edge(),
                        cpu_ops,
                        working_memory_bytes: 0,
                        output_buffer_bytes: 0,
                        executions_per_evaluation: 1,
                        io_bytes_per_execution: Some(0),
                    })
                }),
                _ => None,
            };
            if let Some(operation) = operation {
                model
                    .node_evidence
                    .operations
                    .insert(node as *const _, operation);
                if let SummaryExpr::SummaryDelete { summary_input, .. } = &node.expr {
                    fn owning_aggs(
                        node: &SummaryNode,
                        seen: &mut HashSet<*const SummaryNode>,
                        owners: &mut Vec<*const SummaryNode>,
                    ) {
                        if !seen.insert(node as *const _) {
                            return;
                        }
                        match &node.expr {
                            SummaryExpr::SummaryAgg { child, .. } => {
                                owners.push(node as *const _);
                                owning_aggs(child, seen, owners);
                            }
                            SummaryExpr::SummaryMerge { children } => {
                                for child in children {
                                    owning_aggs(child, seen, owners);
                                }
                            }
                            SummaryExpr::SummarySubtract { left, right }
                            | SummaryExpr::SummaryJoin {
                                outer: left,
                                inner: right,
                                ..
                            } => {
                                owning_aggs(left, seen, owners);
                                owning_aggs(right, seen, owners);
                            }
                            SummaryExpr::SummaryDelete { summary_input, .. }
                            | SummaryExpr::SummaryEstimate { summary_input, .. } => {
                                owning_aggs(summary_input, seen, owners);
                            }
                            SummaryExpr::KeepPreAsap(_) => {}
                        }
                    }
                    let mut owners = Vec::new();
                    owning_aggs(summary_input, &mut HashSet::new(), &mut owners);
                    owners.sort_unstable();
                    owners.dedup();
                    if let [owner] = owners.as_slice() {
                        model
                            .node_evidence
                            .operation_state_owners
                            .insert(node as *const _, *owner);
                    }
                }
            }
            match &node.expr {
                SummaryExpr::SummaryAgg { child, .. } => bind_ops(model, child, seen, inputs, cpu),
                SummaryExpr::SummaryMerge { children } => {
                    for child in children {
                        bind_ops(model, child, seen, inputs, cpu);
                    }
                }
                SummaryExpr::SummarySubtract { left, right }
                | SummaryExpr::SummaryJoin {
                    outer: left,
                    inner: right,
                    ..
                } => {
                    bind_ops(model, left, seen, inputs, cpu);
                    bind_ops(model, right, seen, inputs, cpu);
                }
                SummaryExpr::SummaryDelete { summary_input, .. }
                | SummaryExpr::SummaryEstimate { summary_input, .. } => {
                    bind_ops(model, summary_input, seen, inputs, cpu)
                }
                SummaryExpr::KeepPreAsap(_) => {}
            }
        }
        bind_ops(model, root, &mut HashSet::new(), inputs, cpu);
    }

    fn streaming_scope() -> ComparisonScope {
        let workload = streaming_workload();
        let entry = workload.entries().next().unwrap();
        ComparisonScope::from_workload(
            workload.data_workload.as_ref().unwrap(),
            &entry,
            asap_types::workload::TimestampMs(0),
            asap_types::workload::DurationMs(5_000),
            vec![crate::physical_operator_statistics::SourceCoverage {
                source: Source::TimeSeries {
                    metric: "metrics".into(),
                },
                source_snapshot_id: "stream-start".into(),
                predicates: vec![],
                info_matchers: vec![],
            }],
        )
        .unwrap()
    }
}
