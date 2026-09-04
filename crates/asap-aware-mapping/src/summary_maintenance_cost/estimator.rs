use super::*;

pub(super) fn estimate_heterogeneous_summary(
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
            | SummaryExpr::ExactBinary {
                lhs: left,
                rhs: right,
                ..
            }
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
            SummaryExpr::ExactBinary { lhs, rhs, .. } => {
                let operation = summary_operation_evidence(node, evidence)?.resource();
                *cpu_ops += evaluation_count as f64
                    * validated_operator_executions("exact_binary", operation)? as f64
                    * validated_operator_cpu("exact_binary", operation.cpu_ops)?;
                add_operator_io(io_bytes, operation, evaluation_count)?;
                visit_ops(
                    lhs,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluation_count,
                    cpu_ops,
                    io_bytes,
                )?;
                visit_ops(
                    rhs,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluation_count,
                    cpu_ops,
                    io_bytes,
                )?;
            }
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
                        | SummaryExpr::ExactBinary {
                            lhs: left,
                            rhs: right,
                            ..
                        }
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
            | SummaryExpr::ExactBinary {
                lhs: left,
                rhs: right,
                ..
            }
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
pub(super) fn estimate_transient_liveness(
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
            | SummaryExpr::ExactBinary {
                lhs: left,
                rhs: right,
                ..
            }
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
            | SummaryExpr::ExactBinary { .. }
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
pub(super) fn evidence_nodes(root: &SummaryNode) -> (Vec<&SummaryNode>, Vec<&SummaryNode>) {
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
            | SummaryExpr::ExactBinary {
                lhs: left,
                rhs: right,
                ..
            }
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
pub(super) fn estimate_incremental_summary_maintenance(
    root: &SummaryNode,
    guarantee: &SummaryMaintenanceLifecycleGuarantee,
    inputs: StreamingSummaryInputs,
    cpu: SummaryOperationCpuEvidence,
    scope: &ComparisonScope,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    estimate_incremental_summary_maintenance_with_join(root, guarantee, inputs, cpu, None, scope)
}
#[cfg(test)]
pub(super) fn estimate_incremental_summary_maintenance_with_join(
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

pub(super) fn lifecycle_row_counts(
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

pub(super) fn validated_operator_cpu(
    name: &'static str,
    value: f64,
) -> Result<f64, AnalyticalCostError> {
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
            SummaryExpr::ExactBinary { lhs, rhs, .. } => {
                visit(lhs, seen, counts)?;
                visit(rhs, seen, counts)?;
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
