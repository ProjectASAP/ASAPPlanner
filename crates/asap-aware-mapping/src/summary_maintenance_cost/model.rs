use super::*;
/// Adapter that supplies the existing lifecycle planner with analytical
/// summary costs across at-rest and continuously ingesting workloads. The
/// planner's existing lifecycle enums and legality checks remain authoritative.
#[derive(Debug, Clone)]
pub struct SummaryMaintenanceCostModel {
    pub node_evidence: SummaryNodeEvidence,
    pub calibration: ResourceCalibration,
    pub capabilities: SummaryMaintenanceCapabilities,
    target_comparisons: HashMap<*const OperatorNode, SummaryTargetComparison>,
    candidate_comparisons: HashMap<CandidateComparisonKey, BoundCandidateIdentity>,
    physical_plan_alternatives:
        HashMap<CandidateComparisonKey, Vec<SummaryPhysicalPlanAlternative>>,
    window_framework_candidates:
        HashMap<CandidateComparisonKey, Vec<SummaryWindowFrameworkCandidate>>,
}

type CandidateComparisonKey = (*const OperatorNode, *const OperatorNode);

#[derive(Debug, Clone)]
struct BoundCandidateIdentity {
    _target: Rc<OperatorNode>,
    _root: Rc<OperatorNode>,
}

#[derive(Debug, Clone)]
struct SummaryTargetComparison {
    _target: Rc<OperatorNode>,
    scope: ComparisonScope,
    raw: RawInputEvidence,
}

pub(super) type LogicalSourceSelection = (Source, Vec<Predicate>, Vec<InfoMatcher>);

pub(super) fn deduplicate_source_selections(
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

/// Collect the source selections (scan sources with their predicates, and
/// info-metric selectors) of every leaf reachable from `node`, visiting a
/// shared node once.
pub(super) fn query_source_selections(
    node: &OperatorNode,
    seen: &mut HashSet<*const OperatorNode>,
    out: &mut Vec<LogicalSourceSelection>,
) -> Result<(), AnalyticalCostError> {
    if !seen.insert(node as *const _) {
        return Ok(());
    }
    match &node.operator {
        Operator::NonASAP(NonASAPOp::Scan {
            source, predicates, ..
        }) => out.push((source.clone(), predicates.clone(), vec![])),
        Operator::NonASAP(NonASAPOp::PromqlInfoEnrich { selector, child }) => {
            query_source_selections(child, seen, out)?;
            out.push((info_source(selector)?, vec![], selector.clone()));
        }
        _ => {
            for child in node.children() {
                query_source_selections(child, seen, out)?;
            }
        }
    }
    Ok(())
}

fn validate_query_scope(
    target: &OperatorNode,
    scope: &ComparisonScope,
) -> Result<(), AnalyticalCostError> {
    let mut actual = Vec::new();
    query_source_selections(target, &mut HashSet::new(), &mut actual)?;
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
    physical: &EvidenceBackedPhysicalDAG,
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
    physical: &EvidenceBackedPhysicalDAG,
) -> Result<Vec<&PhysicalDAGNode>, AnalyticalCostError> {
    let by_id: HashMap<_, _> = physical
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    if by_id.len() != physical.nodes.len() {
        return Err(AnalyticalCostError::InvalidPhysicalDAG("duplicate node id"));
    }
    fn visit<'a>(
        id: &'a str,
        by_id: &HashMap<&'a str, &'a PhysicalDAGNode>,
        visiting: &mut HashSet<&'a str>,
        visited: &mut HashSet<&'a str>,
        nodes: &mut Vec<&'a PhysicalDAGNode>,
    ) -> Result<(), AnalyticalCostError> {
        if visited.contains(id) {
            return Ok(());
        }
        if !visiting.insert(id) {
            return Err(AnalyticalCostError::InvalidPhysicalDAG("cycle"));
        }
        let node = by_id
            .get(id)
            .copied()
            .ok_or(AnalyticalCostError::InvalidPhysicalDAG("missing node"))?;
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
    raw: &RawInputEvidence,
    scope: &ComparisonScope,
) -> Result<(), AnalyticalCostError> {
    validate_arrival_rate(scope.data_arrival, raw.ingestion_rate_per_second)?;
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
        return Err(AnalyticalCostError::InvalidPhysicalDAG(
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

pub(super) fn ephemeral_rows_over_horizon(
    inputs: SummaryMaintenanceInputs,
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

pub(super) fn ephemeral_scan_bytes_over_horizon(
    inputs: SummaryMaintenanceInputs,
    raw: &RawInputEvidence,
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

pub(super) fn evaluation_offsets_ms(
    scope: &ComparisonScope,
) -> Result<Vec<u64>, AnalyticalCostError> {
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
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(interval))
        | QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt { interval, .. }) => {
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
            node_evidence: SummaryNodeEvidence::default(),
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
        target: &Rc<OperatorNode>,
        root: &Rc<OperatorNode>,
        scope: ComparisonScope,
        raw: RawInputEvidence,
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
            .or_insert(SummaryTargetComparison {
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
        target: &Rc<OperatorNode>,
        root: &Rc<OperatorNode>,
        alternative: SummaryPhysicalPlanAlternative,
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
        if self.window_framework_candidates.contains_key(&key) {
            return Err(AnalyticalCostError::ComparisonScopeMismatch(
                "physical alternative binding mode",
            ));
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
        target: &Rc<OperatorNode>,
        root: &Rc<OperatorNode>,
        candidate: SummaryWindowFrameworkCandidate,
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
        if self.physical_plan_alternatives.contains_key(&key) {
            return Err(AnalyticalCostError::ComparisonScopeMismatch(
                "physical alternative binding mode",
            ));
        }
        if candidate.assignments.is_empty() {
            return Err(AnalyticalCostError::MissingOrZero(
                "window framework assignments",
            ));
        }
        let mut assigned = HashSet::new();
        if candidate
            .assignments
            .iter()
            .any(|assignment| !assigned.insert(Rc::as_ptr(&assignment.summary)))
        {
            return Err(AnalyticalCostError::MissingOrZero(
                "unique window framework assignments",
            ));
        }
        if assigned != summary_aggregation_identities(root) {
            return Err(AnalyticalCostError::ComparisonScopeMismatch(
                "window framework assignments",
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
        root: &OperatorNode,
        target: Option<&OperatorNode>,
        horizon: Option<crate::recurrence::Horizon>,
        expected_reads: Option<f64>,
    ) -> Option<(CandidateComparisonKey, &SummaryTargetComparison)> {
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
        root: &OperatorNode,
        deployments: &[CostedSummaryDeployment<'_>],
        comparison: &SummaryTargetComparison,
        evidence: &SummaryNodeEvidence,
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

    fn canonical_inputs(&self, summary: &OperatorNode) -> Option<SummaryAggregateEvidence> {
        let evidence = self.node_evidence.aggregation(summary)?;
        evidence.inputs.validate().ok()?;
        Some(evidence)
    }

    fn calibrated(&self, estimate: ResourceEstimate) -> Option<Cost> {
        estimate.calibrated_cost(&self.calibration).ok().map(Cost)
    }

    fn lifecycle_inputs(
        &self,
        summary: &OperatorNode,
        horizon: Option<crate::recurrence::Horizon>,
    ) -> Option<SummaryMaintenanceLifecycleCostInputs> {
        let evidence = self.canonical_inputs(summary)?;
        let inputs = evidence.inputs;
        let insert = validated_operator_cpu("insert_cpu_ops", evidence.insert_cpu_ops).ok()?;
        let build = self.calibrated(ResourceEstimate::new(
            inputs.initial_input_rows as f64 * inputs.bootstrap_window_count as f64 * insert,
            0,
            inputs.initial_source_scan_bytes,
        ))?;
        let maintenance = self.calibrated(ResourceEstimate::new(
            inputs.active_window_count as f64 * insert,
            0,
            0,
        ))?;
        let retained = inputs
            .active_window_count
            .checked_add(inputs.retained_window_count)?
            .checked_mul(inputs.physical_summary_count)?
            .checked_mul(inputs.state_bytes_per_summary)?;
        let retention_total = self.calibrated(ResourceEstimate::new(0.0, retained, 0))?;
        let horizon_seconds = horizon.filter(|value| value.0 > 0.0)?.0;
        Some(SummaryMaintenanceLifecycleCostInputs {
            build_cost: Some(build),
            maintenance_cost_per_update: Some(maintenance),
            // Evaluation is a separate physical operator in the complete DAG.
            // A state-only candidate therefore does not fabricate evaluation
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
            Replacement::ExactComposition(_) => None,
            // Lifecycle selection supplies a complete override. If it cannot,
            // the candidate remains unavailable rather than receiving this
            // trait's structural fallback.
            Replacement::SubDAG(_) => None,
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
        _summary: &OperatorNode,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        SummaryMaintenanceLifecycleCostInputs::default()
    }

    fn summary_maintenance_lifecycle_cost_inputs_for_horizon(
        &self,
        summary: &OperatorNode,
        horizon: Option<crate::recurrence::Horizon>,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        self.lifecycle_inputs(summary, horizon).unwrap_or_default()
    }

    fn summary_maintenance_capabilities(
        &self,
        _summary: &OperatorNode,
    ) -> SummaryMaintenanceCapabilities {
        self.capabilities
    }

    fn complete_summary_candidate_cost(
        &self,
        root: &OperatorNode,
        target: Option<&OperatorNode>,
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
        root: &OperatorNode,
        target: Option<&OperatorNode>,
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
                    let window_accuracy_guarantee = candidate.accuracy.end_to_end_guarantee(
                        uses_exponential_histogram,
                        root.guarantee.as_ref(),
                    )?;
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
                    let frameworks = vec![None; deployments.len()];
                    self.complete_cost_with_evidence(
                        root,
                        deployments,
                        comparison,
                        &alternative.node_evidence,
                        &frameworks,
                    )
                    .map(|cost| CompleteSummaryCandidateEstimate {
                        cost,
                        physical_plan_id: Some(alternative.physical_plan_id.clone()),
                        window_frameworks: frameworks,
                        window_accuracy_guarantee: None,
                    })
                })
                .min_by(|left, right| left.cost.0.total_cmp(&right.cost.0));
        }
        let frameworks = vec![None; deployments.len()];
        self.complete_cost_with_evidence(
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

    fn raw_query_recompute_cost(&self, target: &OperatorNode) -> Option<Cost> {
        let _ = target;
        None
    }

    fn raw_query_recompute_total_cost(
        &self,
        target: &OperatorNode,
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

use super::estimator::*;
#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use asap_types::ir::{ASAPOp, BinaryOperator, NonASAPOp, Operator, OperatorNode};
    use asap_types::post_asap::{
        EvaluationSchedule, ExactKind, ExactParams, Field, FieldDataType, GroupingStrategy,
        OutputRepresentation, Schema, SummaryMaintenanceLifecycle,
        SummaryMaintenanceLifecycleGuarantee, SummaryMaintenanceMode, SummaryUpdate,
    };
    use asap_types::pre_asap::{
        agg_intent::AggIntent, ArithmeticOpKind, BinaryOpKind, DataType, Reduction, Source,
    };
    use asap_types::workload::{
        DataWorkload, Evidence, EvidenceSource, Predictability, Query, QueryLanguage,
        QueryRecurrence, QueryRequirements, QueryTimeScope, QueryWorkload, QueryWorkloadEntry,
        Rate, RepeatedDemand, RepeatingEntry, RepetitionInterval, TimeSelection,
    };

    use super::*;
    use crate::recurrence::Horizon;
    use crate::summary_maintenance_lifecycle::{
        assemble_selected_dag_with_summary_maintenance_lifecycles,
        global_selection_with_summary_maintenance_lifecycles, plan_summary_maintenance_lifecycles,
        SummaryMaintenanceLifecycleCapabilities, WorkloadDemand,
    };

    fn estimate_test(
        root: &OperatorNode,
        guarantee: &SummaryMaintenanceLifecycleGuarantee,
        inputs: SummaryMaintenanceInputs,
        cpu: SummaryOperationCpuEvidence,
    ) -> Result<ResourceEstimate, AnalyticalCostError> {
        estimate_incremental_summary_maintenance(root, guarantee, inputs, cpu, &streaming_scope())
    }

    fn estimate_join_test(
        root: &OperatorNode,
        guarantee: &SummaryMaintenanceLifecycleGuarantee,
        inputs: SummaryMaintenanceInputs,
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

    fn physical() -> SummaryPhysicalInputEvidence {
        SummaryPhysicalInputEvidence {
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

    /// A fixed snapshot needs cardinality evidence, but no stream-rate evidence.
    #[test]
    fn at_rest_workload_adapter_builds_once_without_arrivals() {
        let mut data = streaming_data_workload();
        data.arrival = DataArrival::AtRest;
        data.ingestion_rate = Evidence::default();
        data.input_cardinality = Evidence {
            value: Some(10),
            source: EvidenceSource::Declared,
            ..Default::default()
        };
        let scope = scope_for(&data, &query(), 0, 5_000);
        let inputs = SummaryMaintenanceInputs::from_workload(physical(), &data, &scope).unwrap();
        assert_eq!(inputs.initial_input_rows, 10);
        assert_eq!(inputs.ingestion_rate_per_second, 0.0);
        let guarantee = SummaryMaintenanceLifecycleGuarantee {
            summary_maintenance_lifecycle: SummaryMaintenanceLifecycle::Shared {
                retention: asap_types::workload::DurationMs(5_000),
            },
            summary_maintenance_mode: SummaryMaintenanceMode::DirectBuild,
            evaluation_schedule: EvaluationSchedule::OnRead,
            output_representation: OutputRepresentation::SummaryState,
        };
        assert_eq!(
            lifecycle_row_counts(inputs, &guarantee, &scope).unwrap(),
            (10, 0, 5_000)
        );
    }

    /// Arrival semantics cannot be overridden by missing or contradictory rate evidence.
    #[test]
    fn workload_adapter_checks_arrival_scope_and_rate_evidence() {
        let mut data = streaming_data_workload();
        data.input_cardinality = Evidence {
            value: Some(10),
            source: EvidenceSource::Declared,
            ..Default::default()
        };
        let mut scope = scope_for(&data, &query(), 0, 5_000);
        data.ingestion_rate = Evidence::default();
        assert_eq!(
            SummaryMaintenanceInputs::from_workload(physical(), &data, &scope),
            Err(AnalyticalCostError::MissingOrStale("ingestion_rate"))
        );
        data.arrival = DataArrival::AtRest;
        assert_eq!(
            SummaryMaintenanceInputs::from_workload(physical(), &data, &scope),
            Err(AnalyticalCostError::ComparisonScopeMismatch("data arrival"))
        );
        scope.data_arrival = DataArrival::AtRest;
        for rate in [1.0, -1.0, f64::INFINITY, f64::NAN] {
            data.ingestion_rate = Evidence {
                value: Some(Rate(rate)),
                source: EvidenceSource::Declared,
                ..Default::default()
            };
            assert!(SummaryMaintenanceInputs::from_workload(physical(), &data, &scope).is_err());
        }
        data.ingestion_rate = Evidence::default();
        data.input_cardinality = Evidence::default();
        assert_eq!(
            SummaryMaintenanceInputs::from_workload(physical(), &data, &scope),
            Err(AnalyticalCostError::MissingOrStale("input_cardinality"))
        );
    }

    /// The real lifecycle planner costs a fixed snapshot with the same node evidence API.
    #[test]
    fn lifecycle_planner_selects_fully_costed_at_rest_summary() {
        let workload = streaming_workload();
        let mut data = streaming_data_workload();
        data.arrival = DataArrival::AtRest;
        data.ingestion_rate = Evidence::default();
        data.input_cardinality = Evidence {
            value: Some(10),
            source: EvidenceSource::Declared,
            ..Default::default()
        };
        let mut scope = streaming_scope();
        scope.data_arrival = DataArrival::AtRest;
        let inputs = SummaryMaintenanceInputs::from_workload(physical(), &data, &scope).unwrap();
        let target = streaming_sum_query();
        let root = summary_with_operations(false, false, false);
        let mut provider = streaming_model();
        bind_aggregations(&mut provider, &target, &root, inputs, streaming_cpu());
        let mut model = streaming_model();
        model.node_evidence = provider.node_evidence;
        let mut raw = streaming_raw();
        raw.ingestion_rate_per_second = 0.0;
        let edge = EdgeStatistics {
            rows: 50,
            bytes: 3_200,
        };
        raw.physical_dag
            .evidence
            .get_mut("raw-scan")
            .unwrap()
            .statistics = OperatorStatistics::Scan {
            source_read_bytes: 3_200,
            edges: UnaryEdgeStatistics {
                input: edge,
                output: edge,
                promql: None,
            },
        };
        model
            .bind_candidate_comparison(&target, &root, scope.clone(), raw.clone())
            .unwrap();
        let plan = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new_with_data(&workload, &data, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert!(plan.summary_total_cost.is_some());
        assert!(!plan.selected_raw_recompute);
        assert_eq!(
            plan.deployments[0]
                .summary_maintenance_lifecycle_guarantee
                .as_ref()
                .unwrap()
                .summary_maintenance_mode,
            SummaryMaintenanceMode::DirectBuild
        );

        // Directly supplied raw evidence must not bypass the workload invariant.
        raw.ingestion_rate_per_second = 1.0;
        assert_eq!(
            streaming_model().bind_candidate_comparison(&target, &root, scope, raw),
            Err(AnalyticalCostError::ComparisonScopeMismatch(
                "at-rest ingestion rate"
            ))
        );
        // Nor may a provider hide arrivals on a summary edge.
        for aggregation in model.node_evidence.aggregations.values_mut() {
            aggregation.inputs.ingestion_rate_per_second = 1.0;
        }
        let invalid = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new_with_data(&workload, &data, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(invalid.summary_total_cost, None);
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
        let inputs = SummaryMaintenanceInputs::from_workload(physical(), &data, &scope).unwrap();
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
        let inputs = SummaryMaintenanceInputs::from_workload(empty, &data, &scope).unwrap();
        let estimate = estimate_test(
            &summary_with_operations(false, false, false),
            &continuous_guarantee(),
            inputs,
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(2.0),
                evaluation_cpu_ops: Some(1.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .unwrap();
        // 10 arrivals * 2 active windows * 2 insert ops + 5 reads * 2 summaries.
        assert_eq!(estimate.cpu_ops(), 50.0);
        assert_eq!(estimate.scan_bytes(), 0);
    }

    #[test]
    fn bootstrap_rows_and_bytes_must_be_present_together() {
        let mut inputs = SummaryMaintenanceInputs {
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
                evaluation_cpu_ops: Some(1.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .unwrap();
        // 10 bootstrap rows * 3 windows * 2 insert ops + 5 reads * 2 summaries.
        assert_eq!(estimate.cpu_ops(), 70.0);
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
        let inputs = SummaryMaintenanceInputs {
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
        };
        let root = summary_with_operations(false, false, false);
        let target = streaming_sum_query();
        bind_aggregations(&mut model, &target, &root, inputs, streaming_cpu());
        let plan = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
            SummaryPhysicalPlanAlternative {
                physical_plan_id: "high-retention-layout".into(),
                node_evidence: high_retention,
            },
            SummaryPhysicalPlanAlternative {
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
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
            plan.selected_window_implementation_id.as_deref(),
            Some("low-retention-layout")
        );
        assert_eq!(
            crate::summary_maintenance_dag_export::export_summary_maintenance_plan(&plan)
                .selected_window_implementation_id
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
        for group in space.target_subdag_candidates() {
            for candidate in &group.candidates {
                if let Replacement::SubDAG(root) = &candidate.replacement {
                    if !root.contains_asap() {
                        continue;
                    }
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
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        let plan = assemble_selected_dag_with_summary_maintenance_lifecycles(
            &selection,
            &space.roots[0].1,
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
        let cheap_plan = assemble_selected_dag_with_summary_maintenance_lifecycles(
            &cheap_selection,
            &space.roots[0].1,
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
            PhysicalDAGNode {
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
        let info_target = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(
            NonASAPOp::PromqlInfoEnrich {
                selector: selector.clone(),
                child: target,
            },
        ))
        .unwrap();
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
    fn summary_delete_dag_fails_closed_before_costing() {
        // SummaryDelete is reserved: planning rejects the DAG even with full
        // delete evidence, instead of costing (or owner-checking) the delete.
        let target = streaming_sum_query();
        let root = summary_with_operations(false, false, true);
        let mut cpu = streaming_cpu();
        cpu.delete_cpu_ops = Some(1.0);
        cpu.delete_events_per_second = Some(1.0);
        cpu.delete_routing_fanout = Some(1);
        let mut model = streaming_model();
        model.capabilities.delete = true;
        bind_aggregations(&mut model, &target, &root, streaming_inputs(), cpu);

        // Under a evaluation, the reserved delete surfaces as an illegal child.
        assert!(matches!(
            streaming_planning_error(Rc::clone(&root), &model),
            asap_types::post_asap::ExecutionDataStateError::IllegalChildDataState {
                edge: "FinalizeExactAccumulator.child",
                child: asap_types::post_asap::ExecutionDataState::QUERY_ROWS,
            }
        ));
        // As the root, it is reported as the unimplemented operator itself.
        assert!(matches!(
            streaming_planning_error(evaluation_state(&root), &model),
            asap_types::post_asap::ExecutionDataStateError::UnimplementedOperator {
                operator: "SummaryDelete"
            }
        ));
    }

    #[test]
    fn summary_edge_and_io_evidence_fail_closed() {
        // Over two independent summaries combined by a BinaryOp: a parent
        // input edge that disagrees with its child's output, or missing I/O
        // evidence on the root, leaves the whole-DAG cost unset.
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = add_independent_summary_results();
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        model.node_evidence.insert_operation(
            &root,
            SummaryOperatorEvidence::Binary(test_resource(
                "binary-edge",
                vec![test_edge(), EdgeStatistics { rows: 2, bytes: 16 }],
                1.0,
                1,
                0,
            )),
        );
        let bad_edge = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(bad_edge.summary_total_cost, None);

        let root_evidence = model
            .node_evidence
            .operations
            .get_mut(&Rc::as_ptr(&root))
            .unwrap()
            .resource_mut();
        root_evidence.inputs = vec![test_edge(), test_edge()];
        root_evidence.io_bytes_per_execution = None;
        let missing_io = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(missing_io.summary_total_cost, None);

        // Control: the same evidence with I/O restored is costable.
        model
            .node_evidence
            .operations
            .get_mut(&Rc::as_ptr(&root))
            .unwrap()
            .resource_mut()
            .io_bytes_per_execution = Some(0);
        let complete = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert!(complete.summary_total_cost.is_some());
    }

    #[test]
    fn summary_edges_io_and_physical_identity_fail_closed() {
        // Evidence bound to a structurally equal clone of the BinaryOp does not
        // count for the real node; a bad input edge or missing I/O on the real
        // node still fails closed.
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = add_independent_summary_results();
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        model.node_evidence.insert_operation(
            &Rc::new((*root).clone()),
            SummaryOperatorEvidence::Binary(test_resource(
                "unused",
                vec![test_edge(), test_edge()],
                1.0,
                1,
                0,
            )),
        );
        // Bind the actual BinaryOp, then make one parent input disagree with
        // its child's output.
        model.node_evidence.insert_operation(
            &root,
            SummaryOperatorEvidence::Binary(test_resource(
                "binary-edge",
                vec![test_edge(), EdgeStatistics { rows: 2, bytes: 16 }],
                1.0,
                1,
                0,
            )),
        );
        let bad_edge = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(bad_edge.summary_total_cost, None);

        let root_evidence = model
            .node_evidence
            .operations
            .get_mut(&Rc::as_ptr(&root))
            .unwrap()
            .resource_mut();
        root_evidence.inputs = vec![test_edge(), test_edge()];
        root_evidence.io_bytes_per_execution = None;
        let missing_io = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
        // Two independent summary states (combined by a BinaryOp) that claim
        // one physical id but carry different evidence leave the cost unset.
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = add_independent_summary_results();
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        let aggregations = evidence_nodes(&root).0;
        assert_eq!(aggregations.len(), 2);
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
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(plan.summary_total_cost, None);
    }

    #[test]
    fn summary_join_root_fails_closed_even_with_join_evidence() {
        // SummaryJoin is reserved: planning rejects the DAG whether or not
        // join evidence is bound, so no partial or join cost is produced.
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
        assert!(matches!(
            streaming_planning_error(Rc::clone(&root), &model),
            asap_types::post_asap::ExecutionDataStateError::UnimplementedOperator {
                operator: "SummaryJoin"
            }
        ));

        let join_node = evidence_nodes(&root).1[0];
        model.node_evidence.joins.insert(
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
        assert!(matches!(
            streaming_planning_error(root, &model),
            asap_types::post_asap::ExecutionDataStateError::UnimplementedOperator {
                operator: "SummaryJoin"
            }
        ));
    }

    #[test]
    fn whole_dag_cost_requires_and_uses_each_rc_bound_state_evidence() {
        // Over two independent summaries combined by a BinaryOp: the cost needs
        // evidence for each Rc-bound state, charges peak transient memory, and
        // de-duplicates bootstrap scans only on a shared provider read id.
        let workload = streaming_workload();
        let root = add_independent_summary_results();
        let (left, right) = binary_operands(&root);
        let target = streaming_sum_query();
        let aggregations = [evaluation_state(&left), evaluation_state(&right)];
        let mut model = streaming_model();
        bind_comparison(&mut model, &target, &root);
        model.node_evidence.insert_aggregation(
            &aggregations[0],
            SummaryAggregateEvidence {
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
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
        model.node_evidence.insert_aggregation(
            &aggregations[1],
            SummaryAggregateEvidence {
                physical_id: "right-state".into(),
                input: test_edge(),
                output: test_edge(),
                source_coverage_index: Some(0),
                bootstrap_read_identity: "right-bootstrap".into(),
                inputs: second_inputs,
                insert_cpu_ops: second_cpu.insert_cpu_ops.unwrap(),
            },
        );
        // The left evaluation plays the old join's role: a 64-byte workspace and a
        // 64-byte output that stays live until the root BinaryOp consumes it.
        model.node_evidence.insert_operation(
            &left,
            SummaryOperatorEvidence::ValueOperation(test_resource(
                "left-evaluation",
                vec![test_edge()],
                6.0,
                64,
                64,
            )),
        );
        model.node_evidence.insert_operation(
            &right,
            SummaryOperatorEvidence::ValueOperation(test_resource(
                "right-evaluation",
                vec![test_edge()],
                3.0,
                0,
                0,
            )),
        );
        model.node_evidence.insert_operation(
            &root,
            SummaryOperatorEvidence::Binary(test_resource(
                "root-binary",
                vec![test_edge(), test_edge()],
                3.0,
                0,
                0,
            )),
        );
        let complete = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        // The left evaluation's 64-byte output remains live while the binary's
        // workspace is active (64 + 128 = 192); the evaluation's own workspace is
        // released first, so the old peak was 64 + 64 = 128.
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
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
        let Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
            child: summary_input,
        }) = &root.operator
        else {
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
            SummaryWindowFrameworkCandidate {
                physical_plan_id: "tumbling-v1".into(),
                assignments: vec![SummaryWindowFrameworkAssignment {
                    summary: Rc::clone(&windowed_summary),
                    framework: Some(SummaryWindowFramework::Tumbling),
                }],
                accuracy: SummaryWindowAccuracyEvidence::Exact,
                node_evidence: tumbling,
            },
            SummaryWindowFrameworkCandidate {
                physical_plan_id: "sliding-v1".into(),
                assignments: vec![SummaryWindowFrameworkAssignment {
                    summary: Rc::clone(&windowed_summary),
                    framework: Some(SummaryWindowFramework::Sliding),
                }],
                accuracy: SummaryWindowAccuracyEvidence::Exact,
                node_evidence: sliding,
            },
            SummaryWindowFrameworkCandidate {
                physical_plan_id: "eh-v1".into(),
                assignments: vec![SummaryWindowFrameworkAssignment {
                    summary: Rc::clone(&windowed_summary),
                    framework: Some(SummaryWindowFramework::ExponentialHistogram),
                }],
                accuracy: SummaryWindowAccuracyEvidence::ExponentialHistogram(
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
        model.node_evidence = SummaryNodeEvidence::default();

        let plan = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
        assert_eq!(
            plan.selected_window_implementation_id.as_deref(),
            Some("eh-v1")
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
            exported.selected_window_implementation_id.as_deref(),
            Some("eh-v1")
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
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
        let Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
            child: summary_input,
        }) = &root.operator
        else {
            unreachable!();
        };
        let windowed_summary = Rc::clone(summary_input);
        let mut model = streaming_model();
        bind_comparison(&mut model, &target, &root);

        let empty = model.bind_window_framework_candidate(
            &target,
            &root,
            SummaryWindowFrameworkCandidate {
                physical_plan_id: "empty-assignments".into(),
                assignments: vec![],
                accuracy: SummaryWindowAccuracyEvidence::Exact,
                node_evidence: model.node_evidence.clone(),
            },
        );
        assert!(matches!(empty, Err(AnalyticalCostError::MissingOrZero(_))));

        let candidate = SummaryWindowFrameworkCandidate {
            physical_plan_id: "tumbling-v1".into(),
            assignments: vec![SummaryWindowFrameworkAssignment {
                summary: windowed_summary,
                framework: Some(SummaryWindowFramework::Tumbling),
            }],
            accuracy: SummaryWindowAccuracyEvidence::Exact,
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
        // Two independent summaries (combined by a BinaryOp) sharing one
        // physical state id but assigned different window frameworks leave
        // the cost unset.
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = add_independent_summary_results();
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        let (left, right) = binary_operands(&root);
        let aggregation_nodes = [evaluation_state(&left), evaluation_state(&right)];

        let mut shared_aggregation =
            model.node_evidence.aggregations[&Rc::as_ptr(&aggregation_nodes[0])].clone();
        shared_aggregation.physical_id = "shared-window-state".into();
        for aggregate in &aggregation_nodes {
            model
                .node_evidence
                .insert_aggregation(aggregate, shared_aggregation.clone());
        }

        let retained_children: Vec<_> = aggregation_nodes
            .iter()
            .map(|aggregate| match &aggregate.operator {
                Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) => Rc::clone(child),
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

        let candidate = SummaryWindowFrameworkCandidate {
            physical_plan_id: "mixed-framework-binary".into(),
            assignments: vec![
                SummaryWindowFrameworkAssignment {
                    summary: Rc::clone(&aggregation_nodes[0]),
                    framework: Some(SummaryWindowFramework::Tumbling),
                },
                SummaryWindowFrameworkAssignment {
                    summary: Rc::clone(&aggregation_nodes[1]),
                    framework: Some(SummaryWindowFramework::Sliding),
                },
            ],
            accuracy: SummaryWindowAccuracyEvidence::Exact,
            node_evidence: model.node_evidence.clone(),
        };
        model
            .bind_window_framework_candidate(&target, &root, candidate)
            .unwrap();

        let plan = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
        let full = SummaryWindowAccuracyEvidence::ExponentialHistogram(
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

        let subwindow = SummaryWindowAccuracyEvidence::ExponentialHistogram(
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

        let gsum = SummaryWindowAccuracyEvidence::ExponentialHistogram(
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
    fn eh_accuracy_rejects_negative_components_and_mismatched_summary_guarantees() {
        let evidence = SummaryWindowAccuracyEvidence::ExponentialHistogram(
            ExponentialHistogramAccuracyEvidence::KllRank {
                eh_epsilon: -0.01,
                kll_epsilon: 0.03,
                failure_probability: 0.01,
                range: ExponentialHistogramQueryRange::MostRecentWindow,
            },
        );
        assert!(evidence.guarantee(true).is_none());

        let evidence = SummaryWindowAccuracyEvidence::ExponentialHistogram(
            ExponentialHistogramAccuracyEvidence::KllRank {
                eh_epsilon: 0.01,
                kll_epsilon: 0.02,
                failure_probability: 0.01,
                range: ExponentialHistogramQueryRange::MostRecentWindow,
            },
        );
        let actual_summary = ResultGuarantee {
            metric: ErrorMetric::Rank,
            bound: BoundExpr::Constant { value: 0.03 },
            failure_probability: ProbabilityExpr::Constant { value: 0.01 },
            provenance: vec![],
        };
        assert!(evidence
            .end_to_end_guarantee(true, Some(&actual_summary))
            .is_none());
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
        let Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
            child: summary_input,
        }) = &root.operator
        else {
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
                SummaryWindowFrameworkCandidate {
                    physical_plan_id: "invalid-eh".into(),
                    assignments: vec![SummaryWindowFrameworkAssignment {
                        summary: Rc::clone(summary_input),
                        framework: Some(SummaryWindowFramework::ExponentialHistogram),
                    }],
                    accuracy: SummaryWindowAccuracyEvidence::Exact,
                    node_evidence: model.node_evidence.clone(),
                },
            )
            .unwrap();

        let plan = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(plan.summary_total_cost, None);
    }

    /// Bulk relational evidence cannot hide summary operators below an ordinary root.
    #[test]
    fn retained_subdag_evidence_cannot_hide_summary_work() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = add_shared_summary_result();
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        model.node_evidence.insert_retained_query(
            &root,
            RetainedSubDAGEvidence {
                physical_id: "false-retained-root".into(),
                output: test_edge(),
                preprocessing_cpu_ops_over_horizon: 0.0,
                working_memory_bytes: 0,
                output_buffer_bytes: 0,
            },
        );
        let plan = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(false_lineage.summary_total_cost, None);
    }

    #[test]
    fn state_only_needs_no_evaluation_and_summary_merge_child_fails_closed() {
        // A state-only root is costable without evaluation evidence; a
        // SummaryAgg over a reserved SummaryMerge is rejected at planning.
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let estimated = summary_with_operations(false, false, false);
        let state_only = evaluation_state(&estimated);
        let mut no_evaluation_cpu = streaming_cpu();
        no_evaluation_cpu.evaluation_cpu_ops = None;
        let mut state_model = streaming_model();
        bind_aggregations(
            &mut state_model,
            &target,
            &state_only,
            streaming_inputs(),
            no_evaluation_cpu,
        );
        let state_plan = plan_summary_maintenance_lifecycles(
            state_only,
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &state_model,
        )
        .unwrap();
        assert!(state_plan.summary_total_cost.is_some());

        let nested = std::rc::Rc::new(
            OperatorNode::with_schema(
                asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg {
                    child: evaluation_state(&summary_with_operations(true, false, false)),
                    family: FieldDataType::ExactAggregate(ExactKind::Count, ExactParams::Count),
                    input: SummaryUpdate {
                        item: None,
                        weight: asap_types::post_asap::SummaryInputExpr::Constant(1.0),
                        weight_domain: Default::default(),
                    },
                    reduction: Reduction::by(vec![]),
                    grouping: GroupingStrategy::PerSubpopulationInstance,
                    filter: None,
                }),
                count_state_schema(),
            )
            .with_guarantee(None),
        );
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
        assert!(matches!(
            streaming_planning_error(nested, &nested_model),
            asap_types::post_asap::ExecutionDataStateError::UnimplementedOperator {
                operator: "SummaryMerge"
            }
        ));
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
            SummaryMaintenanceInputs::from_workload(physical(), &data, &scope),
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
            SummaryMaintenanceInputs {
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
                evaluation_cpu_ops: Some(3.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .unwrap();
        // 10 bootstrap + 10 arrivals into two active windows; two states read 5 times.
        assert_eq!(estimate.cpu_ops(), 90.0);
        assert_eq!(estimate.peak_memory_bytes(), 1_000);
        assert_eq!(estimate.scan_bytes(), 640);
    }

    #[test]
    fn operations_use_update_or_read_multiplicity_and_shared_state_once() {
        let estimate = estimate_test(
            &summary_with_operations(true, true, true),
            &continuous_guarantee(),
            SummaryMaintenanceInputs {
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
                evaluation_cpu_ops: Some(7.0),
            },
        )
        .unwrap();
        assert_eq!(estimate.cpu_ops(), 21.0 + 20.0 + 30.0 + 200.0 + 70.0);
        // Three persistent windows plus one transient result, for two instances.
        assert_eq!(estimate.peak_memory_bytes(), 80);
    }

    #[test]
    fn lifecycle_mode_and_schedule_must_match_existing_planner_semantics() {
        let mut guarantee = continuous_guarantee();
        guarantee.evaluation_schedule = EvaluationSchedule::OnRead;
        assert_eq!(
            estimate_test(
                &summary_with_operations(false, false, false),
                &guarantee,
                SummaryMaintenanceInputs {
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
                    evaluation_cpu_ops: Some(1.0),
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
                SummaryMaintenanceInputs {
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
                    evaluation_cpu_ops: Some(1.0),
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
                SummaryMaintenanceInputs {
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
                    evaluation_cpu_ops: Some(1.0),
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
            SummaryMaintenanceInputs {
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
                evaluation_cpu_ops: Some(1.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .unwrap();
        // Two pre-activation arrivals join the bootstrap; eight more are
        // maintained through the horizon; five reads are served.
        assert_eq!(estimate.cpu_ops(), 25.0);
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
            SummaryMaintenanceInputs {
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
                evaluation_cpu_ops: Some(1.0),
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
        let inputs = SummaryMaintenanceInputs {
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
            evaluation_cpu_ops: Some(1.0),
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
        assert_eq!(estimate.cpu_ops(), 77.0);
        assert_eq!(estimate.peak_memory_bytes(), 64); // 4 persistent states + join memory.
    }

    fn count_state_schema() -> Schema {
        Schema::lifted(
            vec![Field::new(
                "count",
                FieldDataType::ExactAggregate(ExactKind::Count, ExactParams::Count),
                false,
            )],
            None,
        )
    }

    fn count_evaluation_schema() -> Schema {
        Schema::lifted(vec![Field::plain("count", DataType::Int64, false)], None)
    }

    /// The retained relational input of every test summary: a bare scan of
    /// the `metrics` series.
    fn metrics_scan() -> Rc<OperatorNode> {
        OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries {
                metric: "metrics".into(),
            },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        }))
        .unwrap()
    }

    fn summary_with_operations(merge: bool, subtract: bool, delete: bool) -> Rc<OperatorNode> {
        let state_type = FieldDataType::ExactAggregate(ExactKind::Count, ExactParams::Count);
        let schema = count_state_schema();
        let agg = std::rc::Rc::new(
            OperatorNode::with_schema(
                asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg {
                    child: metrics_scan(),
                    family: state_type,
                    input: SummaryUpdate {
                        item: None,
                        weight: asap_types::post_asap::SummaryInputExpr::Constant(1.0),
                        weight_domain: Default::default(),
                    },
                    reduction: Reduction::by(vec![]),
                    grouping: GroupingStrategy::PerSubpopulationInstance,
                    filter: None,
                }),
                schema.clone(),
            )
            .with_guarantee(None),
        );
        let mut root = Rc::clone(&agg);
        if merge {
            root = std::rc::Rc::new(
                OperatorNode::with_schema(
                    asap_types::ir::Operator::ASAP(ASAPOp::SummaryMerge {
                        children: vec![Rc::clone(&agg), Rc::clone(&agg)],
                    }),
                    schema.clone(),
                )
                .with_guarantee(None),
            );
        }
        if subtract {
            root = std::rc::Rc::new(
                OperatorNode::with_schema(
                    asap_types::ir::Operator::ASAP(ASAPOp::SummarySubtract {
                        left: Rc::clone(&root),
                        right: Rc::clone(&agg),
                    }),
                    schema.clone(),
                )
                .with_guarantee(None),
            );
        }
        if delete {
            root = std::rc::Rc::new(
                OperatorNode::with_schema(
                    asap_types::ir::Operator::ASAP(ASAPOp::SummaryDelete {
                        summary_input: root,
                        key: 0,
                    }),
                    schema.clone(),
                )
                .with_guarantee(None),
            );
        }
        std::rc::Rc::new(
            OperatorNode::with_schema(
                asap_types::ir::Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: root }),
                count_evaluation_schema(),
            )
            .with_guarantee(Some(ResultGuarantee::exact("exact count evaluation"))),
        )
    }

    fn summary_join() -> Rc<OperatorNode> {
        let left = summary_with_operations(false, false, false);
        let right = summary_with_operations(false, false, false);
        let Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: left }) = &left.operator
        else {
            unreachable!()
        };
        let Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: right }) = &right.operator
        else {
            unreachable!()
        };
        let join = std::rc::Rc::new(
            OperatorNode::with_schema(
                asap_types::ir::Operator::ASAP(ASAPOp::SummaryJoin {
                    outer: Rc::clone(left),
                    inner: Rc::clone(right),
                    key: 0,
                    family: FieldDataType::ExactAggregate(ExactKind::Count, ExactParams::Count),
                }),
                left.schema.clone(),
            )
            .with_guarantee(None),
        );
        std::rc::Rc::new(
            OperatorNode::with_schema(
                asap_types::ir::Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: join }),
                count_evaluation_schema(),
            )
            .with_guarantee(None),
        )
    }

    fn add_shared_summary_result() -> Rc<OperatorNode> {
        let operand = summary_with_operations(false, false, false);
        Rc::new(
            OperatorNode::new(Operator::NonASAP(NonASAPOp::BinaryOp {
                operator: BinaryOperator {
                    checked_relative_division: false,
                    checked_finite_division: false,
                    kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Add),
                    vector_match: None,
                },
                return_bool: false,
                lhs: Rc::clone(&operand),
                rhs: operand,
            }))
            .unwrap()
            .with_guarantee(Some(ResultGuarantee::exact("test binary"))),
        )
    }

    /// Two independent summary states, each read out, combined by an ordinary
    /// `BinaryOp`: the non-reserved replacement for a `SummaryJoin` fixture.
    #[test]
    fn independent_summary_results_form_a_valid_dag() {
        add_independent_summary_results()
            .validate_structure()
            .unwrap();
    }

    fn add_independent_summary_results() -> Rc<OperatorNode> {
        Rc::new(
            OperatorNode::new(Operator::NonASAP(NonASAPOp::BinaryOp {
                operator: BinaryOperator {
                    checked_relative_division: false,
                    checked_finite_division: false,
                    kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Add),
                    vector_match: None,
                },
                return_bool: false,
                lhs: summary_with_operations(false, false, false),
                rhs: summary_with_operations(false, false, false),
            }))
            .unwrap()
            .with_guarantee(Some(ResultGuarantee::exact("test binary"))),
        )
    }

    /// The `(lhs, rhs)` evaluations of [`add_independent_summary_results`].
    fn binary_operands(root: &OperatorNode) -> (Rc<OperatorNode>, Rc<OperatorNode>) {
        let Operator::NonASAP(NonASAPOp::BinaryOp { lhs, rhs, .. }) = &root.operator else {
            unreachable!();
        };
        (Rc::clone(lhs), Rc::clone(rhs))
    }

    /// The `SummaryAgg` under one `SummaryEstimate` evaluation.
    fn evaluation_state(evaluation: &OperatorNode) -> Rc<OperatorNode> {
        let Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
            child: summary_input,
        }) = &evaluation.operator
        else {
            unreachable!();
        };
        Rc::clone(summary_input)
    }

    /// The DAG-validation error that streaming lifecycle planning of `root`
    /// fails closed with.
    fn streaming_planning_error(
        root: Rc<OperatorNode>,
        model: &SummaryMaintenanceCostModel,
    ) -> asap_types::post_asap::ExecutionDataStateError {
        let workload = streaming_workload();
        match plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            model,
        ) {
            Err(
                crate::summary_maintenance_lifecycle::SummaryMaintenanceLifecyclePlanError::InvalidPostAsapDAG(
                    error,
                ),
            ) => error,
            Err(other) => panic!("unexpected planning error: {other}"),
            Ok(_) => panic!("planning must fail closed"),
        }
    }

    fn test_resource(
        physical_id: &str,
        inputs: Vec<EdgeStatistics>,
        cpu_ops: f64,
        working_memory_bytes: u64,
        output_buffer_bytes: u64,
    ) -> SummaryOperatorResourceEvidence {
        SummaryOperatorResourceEvidence {
            physical_id: physical_id.into(),
            inputs,
            output: test_edge(),
            cpu_ops,
            working_memory_bytes,
            output_buffer_bytes,
            executions_per_evaluation: 1,
            io_bytes_per_execution: Some(0),
        }
    }

    #[test]
    fn exact_binary_is_costable_with_explicit_physical_evidence() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = add_shared_summary_result();
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
            WorkloadDemand::new_with_data(&workload, &streaming_data_workload(), &[0]),
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
        .expect("binary physical evidence should produce a complete cost");
        assert!(plan.summary_total_cost.is_some());
    }

    fn streaming_sum_query() -> Rc<OperatorNode> {
        OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![]),
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: metrics_scan(),
        }))
        .unwrap()
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
        }
    }

    fn streaming_data_workload() -> DataWorkload {
        DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            data_ingestion_interval: Evidence {
                value: Some(asap_types::workload::DurationMs(1_000)),
                ..Default::default()
            },
            ingestion_rate: Evidence {
                value: Some(Rate(2.0)),
                source: EvidenceSource::Declared,
                observed_at_ms: None,
                valid_for_ms: None,
            },
            ..Default::default()
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

    fn streaming_raw() -> RawInputEvidence {
        let scope = streaming_scope();
        let node = PhysicalDAGNode {
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
        RawInputEvidence {
            planning_time_input_rows: 10,
            planning_time_input_bytes: 640,
            planning_time_source_scan_bytes: 640,
            arriving_logical_row_bytes: 64,
            arriving_source_row_bytes: 64,
            ingestion_rate_per_second: 2.0,
            physical_dag: EvidenceBackedPhysicalDAG {
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

    /// A retained relational sub-DAG: a non-ASAP node with no summary below
    /// it, costed as one unit through retained-query evidence.
    fn is_retained(node: &OperatorNode) -> bool {
        !node.contains_asap()
    }

    fn bind_comparison(
        model: &mut SummaryMaintenanceCostModel,
        target: &Rc<OperatorNode>,
        root: &Rc<OperatorNode>,
    ) {
        model
            .bind_candidate_comparison(target, root, streaming_scope(), streaming_raw())
            .unwrap();
        fn retained(
            model: &mut SummaryMaintenanceCostModel,
            node: &Rc<OperatorNode>,
            seen: &mut HashSet<*const OperatorNode>,
        ) {
            if !seen.insert(Rc::as_ptr(node)) {
                return;
            }
            if is_retained(node) {
                model.node_evidence.insert_retained_query(
                    node,
                    RetainedSubDAGEvidence {
                        physical_id: format!("retained-{node:p}"),
                        output: test_edge(),
                        preprocessing_cpu_ops_over_horizon: 1.0,
                        working_memory_bytes: 8,
                        output_buffer_bytes: 0,
                    },
                );
                return;
            }
            for child in node.children() {
                retained(model, child, seen);
            }
        }
        retained(model, root, &mut HashSet::new());
    }

    fn streaming_inputs() -> SummaryMaintenanceInputs {
        SummaryMaintenanceInputs {
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
            evaluation_cpu_ops: Some(3.0),
            ..SummaryOperationCpuEvidence::default()
        }
    }

    fn bind_aggregations(
        model: &mut SummaryMaintenanceCostModel,
        target: &Rc<OperatorNode>,
        root: &Rc<OperatorNode>,
        inputs: SummaryMaintenanceInputs,
        cpu: SummaryOperationCpuEvidence,
    ) {
        bind_comparison(model, target, root);
        for node in evidence_nodes(root).0 {
            let source_root = matches!(
                &node.operator,
                Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) if is_retained(child)
            );
            let mut node_inputs = inputs;
            if !source_root {
                node_inputs.initial_input_rows = test_edge().rows;
                node_inputs.initial_input_bytes = test_edge().bytes;
                node_inputs.initial_source_scan_bytes = 0;
            }
            model.node_evidence.aggregations.insert(
                node as *const _,
                SummaryAggregateEvidence {
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
        fn resource(
            physical_id: String,
            inputs: Vec<EdgeStatistics>,
            cpu_ops: f64,
            working_memory_bytes: u64,
        ) -> SummaryOperatorResourceEvidence {
            SummaryOperatorResourceEvidence {
                physical_id,
                inputs,
                output: test_edge(),
                cpu_ops,
                working_memory_bytes,
                output_buffer_bytes: 0,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
            }
        }
        fn bind_ops(
            model: &mut SummaryMaintenanceCostModel,
            node: &OperatorNode,
            seen: &mut HashSet<*const OperatorNode>,
            inputs: SummaryMaintenanceInputs,
            cpu: SummaryOperationCpuEvidence,
        ) {
            if !seen.insert(node as *const _) {
                return;
            }
            if is_retained(node) {
                return;
            }
            let operation = match &node.operator {
                Operator::NonASAP(NonASAPOp::BinaryOp { .. }) => {
                    cpu.evaluation_cpu_ops.map(|cpu_ops| {
                        SummaryOperatorEvidence::Binary(resource(
                            format!("binary-{node:p}"),
                            vec![test_edge(), test_edge()],
                            cpu_ops,
                            0,
                        ))
                    })
                }
                Operator::NonASAP(NonASAPOp::Join { .. }) => None,
                Operator::NonASAP(_)
                | Operator::ASAP(
                    ASAPOp::FinalizeExactAccumulator { .. }
                    | ASAPOp::MaintainPopulation { .. }
                    | ASAPOp::EvaluatePopulation { .. },
                ) => cpu.evaluation_cpu_ops.map(|cpu_ops| {
                    SummaryOperatorEvidence::ValueOperation(resource(
                        format!("value-operation-{node:p}"),
                        vec![test_edge()],
                        cpu_ops,
                        0,
                    ))
                }),
                Operator::ASAP(ASAPOp::SummaryMerge { children }) => {
                    cpu.merge_cpu_ops.map(|cpu_ops| {
                        SummaryOperatorEvidence::Merge(resource(
                            format!("merge-{node:p}"),
                            vec![test_edge(); children.len()],
                            cpu_ops,
                            inputs.state_bytes_per_summary,
                        ))
                    })
                }
                Operator::ASAP(ASAPOp::SummarySubtract { .. }) => {
                    cpu.subtract_cpu_ops.map(|cpu_ops| {
                        SummaryOperatorEvidence::Subtract(resource(
                            format!("subtract-{node:p}"),
                            vec![test_edge(), test_edge()],
                            cpu_ops,
                            inputs.state_bytes_per_summary,
                        ))
                    })
                }
                Operator::ASAP(ASAPOp::SummaryDelete { .. }) => {
                    cpu.delete_cpu_ops.and_then(|cpu_ops| {
                        Some(SummaryOperatorEvidence::Delete {
                            resource: resource(
                                format!("delete-{node:p}"),
                                vec![test_edge()],
                                cpu_ops,
                                0,
                            ),
                            events_per_second: cpu.delete_events_per_second?,
                            routing_fanout: cpu.delete_routing_fanout?,
                        })
                    })
                }
                Operator::ASAP(ASAPOp::SummaryEstimate { .. }) => {
                    cpu.evaluation_cpu_ops.map(|cpu_ops| {
                        SummaryOperatorEvidence::Evaluation(resource(
                            format!("evaluation-{node:p}"),
                            vec![test_edge()],
                            cpu_ops,
                            0,
                        ))
                    })
                }
                Operator::ASAP(
                    ASAPOp::SummaryAgg { .. }
                    | ASAPOp::SummaryJoin { .. }
                    | ASAPOp::Extension { .. },
                ) => None,
            };
            if let Some(operation) = operation {
                model
                    .node_evidence
                    .operations
                    .insert(node as *const _, operation);
                if let Operator::ASAP(ASAPOp::SummaryDelete { summary_input, .. }) = &node.operator
                {
                    fn owning_aggs(
                        node: &OperatorNode,
                        seen: &mut HashSet<*const OperatorNode>,
                        owners: &mut Vec<*const OperatorNode>,
                    ) {
                        if !seen.insert(node as *const _) {
                            return;
                        }
                        if matches!(node.operator, Operator::ASAP(ASAPOp::SummaryAgg { .. })) {
                            owners.push(node as *const _);
                        }
                        for child in node.children() {
                            owning_aggs(child, seen, owners);
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
            for child in node.children() {
                bind_ops(model, child, seen, inputs, cpu);
            }
        }
        bind_ops(model, root, &mut HashSet::new(), inputs, cpu);
    }

    fn streaming_scope() -> ComparisonScope {
        let workload = streaming_workload();
        let entry = workload.entries().next().unwrap();
        ComparisonScope::from_workload(
            &DataWorkload {
                arrival: DataArrival::ContinuouslyIngesting,
                data_ingestion_interval: Evidence {
                    value: Some(asap_types::workload::DurationMs(1_000)),
                    ..Default::default()
                },
                ingestion_rate: Evidence {
                    value: Some(Rate(2.0)),
                    source: EvidenceSource::Declared,
                    observed_at_ms: None,
                    valid_for_ms: None,
                },
                ..Default::default()
            },
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
