//! Recursive lowering from the operator IR to evidenced physical DAGs.

use std::rc::Rc;

use asap_types::ir::{NonASAPOp, OperatorNode, ScalarExpr};

use crate::analytical_cost::{
    validate_operator_semantics, AnalyticalCostError, EvidenceBackedPhysicalDAG,
    ExecutionMultiplicity, HashJoinBuildSide, PhysicalDAGNode, PhysicalNodeEvidence,
    PhysicalOperator, PromqlBinaryOperandMode, PromqlBinaryOperation, PromqlPresenceKind,
    PromqlSeriesSampleKind, PromqlVectorCardinality,
};
use crate::physical_operator_statistics::{
    ComparisonScope, EdgeStatistics, OperatorStatistics, SourceCoverage,
};

pub struct PhysicalNodeRequest<'a> {
    pub logical_node: &'a OperatorNode,
    pub operator: PhysicalOperator,
    pub occurrence: usize,
    pub synthetic: bool,
    pub children: &'a [String],
    pub source_coverage: Option<&'a SourceCoverage>,
}

pub trait PhysicalNodeEvidenceProvider {
    fn evidence(
        &self,
        request: PhysicalNodeRequest<'_>,
    ) -> Result<PhysicalNodeEvidence, AnalyticalCostError>;
}

impl<F> PhysicalNodeEvidenceProvider for F
where
    F: Fn(PhysicalNodeRequest<'_>) -> Result<PhysicalNodeEvidence, AnalyticalCostError>,
{
    fn evidence(
        &self,
        request: PhysicalNodeRequest<'_>,
    ) -> Result<PhysicalNodeEvidence, AnalyticalCostError> {
        self(request)
    }
}

/// Lower a non-ASAP operator DAG to the physical operators understood by
/// this cost model. The authoritative provider supplies statistics by the
/// stable physical IDs owned by that provider; missing evidence makes the
/// complete query unavailable. Scalar expressions remain part of their
/// containing operator's local cost. An ASAP node is unsupported here.
pub fn lower_query_physical_dag(
    root: &Rc<OperatorNode>,
    scope: &ComparisonScope,
    evidence: &dyn PhysicalNodeEvidenceProvider,
) -> Result<EvidenceBackedPhysicalDAG, AnalyticalCostError> {
    use std::collections::HashMap;

    use asap_types::pre_asap::{GroupKeys, RelationalSetOpKind};

    scope.validate()?;

    struct Lowerer<'a> {
        scope: &'a ComparisonScope,
        provider: &'a dyn PhysicalNodeEvidenceProvider,
        evidence: HashMap<String, PhysicalNodeEvidence>,
        next_id: usize,
        nodes: Vec<PhysicalDAGNode>,
    }

    impl Lowerer<'_> {
        fn lower(&mut self, query: &OperatorNode) -> Result<String, AnalyticalCostError> {
            let occurrence = self.next_id;
            self.next_id += 1;
            self.lower_new(query, occurrence)
        }

        fn resolve(
            &self,
            query: &OperatorNode,
            operator: PhysicalOperator,
            occurrence: usize,
            synthetic: bool,
            children: &[String],
            source_coverage: Option<&SourceCoverage>,
        ) -> Result<PhysicalNodeEvidence, AnalyticalCostError> {
            let evidence = self.provider.evidence(PhysicalNodeRequest {
                logical_node: query,
                operator,
                occurrence,
                synthetic,
                children,
                source_coverage,
            })?;
            if evidence.physical_id.is_empty() {
                return Err(AnalyticalCostError::InvalidPhysicalDAG(
                    "provider returned an empty physical identity",
                ));
            }
            Ok(evidence)
        }

        fn push(
            &mut self,
            evidence: PhysicalNodeEvidence,
            operator: PhysicalOperator,
            children: Vec<String>,
            source_coverage: Option<SourceCoverage>,
        ) -> Result<String, AnalyticalCostError> {
            let id = evidence.physical_id.clone();
            let node = PhysicalDAGNode {
                id: id.clone(),
                operator,
                children,
                source_coverage,
                output_buffer_bytes: evidence.output_buffer_bytes,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            };
            if let Some(existing) = self.nodes.iter().find(|existing| existing.id == id) {
                if existing != &node || self.evidence.get(&id) != Some(&evidence) {
                    return Err(AnalyticalCostError::InvalidPhysicalDAG(
                        "provider reused a physical identity for conflicting evidence",
                    ));
                }
                return Ok(id);
            }
            self.nodes.push(node);
            self.evidence.insert(id.clone(), evidence);
            Ok(id)
        }

        fn lower_unary(
            &mut self,
            query: &OperatorNode,
            occurrence: usize,
            operator: PhysicalOperator,
            child: &OperatorNode,
        ) -> Result<String, AnalyticalCostError> {
            let child_id = self.lower(child)?;
            self.push_unary(query, occurrence, operator, child_id)
        }

        /// `operator` over an already-lowered child.
        fn push_unary(
            &mut self,
            query: &OperatorNode,
            occurrence: usize,
            operator: PhysicalOperator,
            child_id: String,
        ) -> Result<String, AnalyticalCostError> {
            let children = vec![child_id.clone()];
            let evidence = self.resolve(query, operator, occurrence, false, &children, None)?;
            let statistics = &evidence.statistics;
            let child_statistics = self.node_statistics(&child_id)?;
            require_unary_edge(
                &evidence.physical_id,
                statistics,
                &child_id,
                child_statistics,
            )?;
            require_promql_edge(statistics, 0, child_statistics)?;
            require_operator_statistics(operator, statistics)?;
            self.push(evidence, operator, children, None)
        }

        fn lower_promql_unary(
            &mut self,
            query: &OperatorNode,
            occurrence: usize,
            operator: PhysicalOperator,
            child: &OperatorNode,
        ) -> Result<String, AnalyticalCostError> {
            self.lower_unary(query, occurrence, operator, child)
        }

        fn lower_promql_scalar_leaf(
            &mut self,
            query: &OperatorNode,
            occurrence: usize,
        ) -> Result<String, AnalyticalCostError> {
            let operator = PhysicalOperator::PromqlScalarLeaf;
            let evidence = self.resolve(query, operator, occurrence, false, &[], None)?;
            require_statistics_shape(&evidence.physical_id, &evidence.statistics, 0)?;
            require_operator_statistics(operator, &evidence.statistics)?;
            self.push(evidence, operator, vec![], None)
        }

        /// Lower the owned scalar operand of `vector(s)`. A literal or
        /// `time()` is a physical scalar leaf; `scalar(v)` reads its vector
        /// through `PromqlVectorToScalar`.
        fn lower_scalar_operand(
            &mut self,
            query: &OperatorNode,
            occurrence: usize,
            expr: &ScalarExpr,
        ) -> Result<String, AnalyticalCostError> {
            match expr {
                ScalarExpr::Literal(asap_types::pre_asap::ScalarValue::Float64(_))
                | ScalarExpr::EvalTimestamp => self.lower_promql_scalar_leaf(query, occurrence),
                ScalarExpr::PromqlScalarFromVector(vector) => self.lower_promql_unary(
                    query,
                    occurrence,
                    PhysicalOperator::PromqlVectorToScalar,
                    vector,
                ),
                _ => Err(AnalyticalCostError::UnsupportedQueryOperator),
            }
        }

        fn node_statistics(&self, id: &str) -> Result<&OperatorStatistics, AnalyticalCostError> {
            self.evidence
                .get(id)
                .map(|evidence| &evidence.statistics)
                .ok_or(AnalyticalCostError::InvalidPhysicalDAG(
                    "lowered child statistics are missing",
                ))
        }

        fn lower_new(
            &mut self,
            query: &OperatorNode,
            occurrence: usize,
        ) -> Result<String, AnalyticalCostError> {
            let Some(op) = query.non_asap() else {
                return Err(AnalyticalCostError::UnsupportedQueryOperator);
            };
            match op {
                NonASAPOp::Scan {
                    source, predicates, ..
                } => {
                    let coverage = bind_scan_coverage(
                        &format!("occurrence-{occurrence}"),
                        source,
                        predicates,
                        self.scope,
                    )?;
                    if predicates.is_empty() {
                        let evidence = self.resolve(
                            query,
                            PhysicalOperator::Scan,
                            occurrence,
                            false,
                            &[],
                            Some(&coverage),
                        )?;
                        require_statistics_shape(&evidence.physical_id, &evidence.statistics, 1)?;
                        require_scan_edges_equal(&evidence.statistics)?;
                        return self.push(evidence, PhysicalOperator::Scan, vec![], Some(coverage));
                    }
                    let scan_evidence = self.resolve(
                        query,
                        PhysicalOperator::Scan,
                        occurrence,
                        true,
                        &[],
                        Some(&coverage),
                    )?;
                    require_statistics_shape(
                        &scan_evidence.physical_id,
                        &scan_evidence.statistics,
                        1,
                    )?;
                    require_scan_edges_equal(&scan_evidence.statistics)?;
                    let scan_statistics = scan_evidence.statistics.clone();
                    let scan_id = self.push(
                        scan_evidence,
                        PhysicalOperator::Scan,
                        vec![],
                        Some(coverage),
                    )?;
                    let children = vec![scan_id.clone()];
                    let predicate_operations_per_row = predicates
                        .iter()
                        .try_fold(0_u64, |total, predicate| {
                            total
                                .checked_add(scalar_operation_count(&predicate.0)?)
                                .ok_or(AnalyticalCostError::Overflow)
                        })?
                        .max(1);
                    let filter_operator = PhysicalOperator::Filter {
                        predicate_operations_per_row,
                    };
                    let filter_evidence =
                        self.resolve(query, filter_operator, occurrence, false, &children, None)?;
                    require_unary_edge(
                        &filter_evidence.physical_id,
                        &filter_evidence.statistics,
                        &scan_id,
                        &scan_statistics,
                    )?;
                    require_operator_statistics(filter_operator, &filter_evidence.statistics)?;
                    self.push(filter_evidence, filter_operator, children, None)
                }
                NonASAPOp::Filter { pred, child } => {
                    let operator = PhysicalOperator::Filter {
                        predicate_operations_per_row: scalar_operation_count(&pred.0)?.max(1),
                    };
                    self.lower_unary(query, occurrence, operator, child)
                }
                NonASAPOp::Project { cols, child, .. } => {
                    let expression_operations_per_row = cols
                        .iter()
                        .try_fold(0_u64, |total, item| {
                            total
                                .checked_add(
                                    scalar_operation_count(&item.expr)?
                                        .checked_add(1)
                                        .ok_or(AnalyticalCostError::Overflow)?,
                                )
                                .ok_or(AnalyticalCostError::Overflow)
                        })?
                        .max(1);
                    self.lower_unary(
                        query,
                        occurrence,
                        PhysicalOperator::Project {
                            expression_operations_per_row,
                        },
                        child,
                    )
                }
                NonASAPOp::Aggregate {
                    reduction,
                    measures,
                    filters,
                    having,
                    child,
                    ..
                } => {
                    if having.is_some()
                        || asap_types::ir::non_asap::any_measure_filtered(filters)
                        || measures.is_empty()
                    {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    if matches!(reduction, asap_types::pre_asap::Reduction::PerEntity) {
                        if measures.len() != 1 {
                            return Err(AnalyticalCostError::UnsupportedQueryOperator);
                        }
                        let accumulator_count = u64::try_from(measures.len())
                            .map_err(|_| AnalyticalCostError::Overflow)?;
                        let operator = if measures.iter().all(presence_intent) {
                            PhysicalOperator::PromqlPresence {
                                kind: PromqlPresenceKind::Absent,
                                operations_per_row: accumulator_count,
                            }
                        } else if measures.iter().all(present_over_time_intent) {
                            PhysicalOperator::PromqlPresence {
                                kind: PromqlPresenceKind::PresentPerSeries,
                                operations_per_row: accumulator_count,
                            }
                        } else if measures.iter().all(fixed_state_per_series_intent) {
                            PhysicalOperator::PromqlPerSeries {
                                operations_per_row: accumulator_count,
                                accumulator_count,
                            }
                        } else {
                            return Err(AnalyticalCostError::UnsupportedQueryOperator);
                        };
                        return self.lower_promql_unary(query, occurrence, operator, child);
                    }
                    let asap_types::pre_asap::Reduction::Reduce(grouping) = reduction else {
                        unreachable!("per-entity reduction returned above")
                    };
                    if grouping.is_without() || !supports_hash_aggregate(reduction, measures) {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    self.lower_unary(
                        query,
                        occurrence,
                        PhysicalOperator::HashAggregate {
                            grouping_key_count: u64::try_from(grouping.keys().len())
                                .map_err(|_| AnalyticalCostError::Overflow)?,
                            accumulator_count: u64::try_from(measures.len())
                                .map_err(|_| AnalyticalCostError::Overflow)?,
                        },
                        child,
                    )
                }
                NonASAPOp::Dedup { cols, child } => {
                    let key_count = if cols.is_empty() {
                        child.schema.fields.len()
                    } else {
                        cols.len()
                    };
                    if key_count == 0 {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    self.lower_unary(
                        query,
                        occurrence,
                        PhysicalOperator::HashDeduplicate {
                            key_count: u64::try_from(key_count)
                                .map_err(|_| AnalyticalCostError::Overflow)?,
                        },
                        child,
                    )
                }
                NonASAPOp::Sort {
                    keys,
                    partition_by,
                    child,
                    ..
                } => {
                    if keys.is_empty() {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    self.lower_unary(
                        query,
                        occurrence,
                        PhysicalOperator::InMemoryComparisonSort {
                            ordering_key_count: u64::try_from(keys.len())
                                .map_err(|_| AnalyticalCostError::Overflow)?,
                            partitioned: partition_by != &GroupKeys::none(),
                        },
                        child,
                    )
                }
                NonASAPOp::Limit {
                    n,
                    offset,
                    partition_by: limit_partition_by,
                    child,
                } => {
                    // Offset-only and per-group limits have no physical
                    // operator here.
                    let Some(n) = n else {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    };
                    if limit_partition_by != &GroupKeys::none() {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    if let Some(NonASAPOp::Sort {
                        keys,
                        partition_by,
                        child: sorted_child,
                        ..
                    }) = child.non_asap()
                    {
                        if !keys.is_empty() && partition_by == &GroupKeys::none() {
                            let child_id = self.lower(sorted_child)?;
                            let children = vec![child_id.clone()];
                            let limit =
                                u64::try_from(*n).map_err(|_| AnalyticalCostError::Overflow)?;
                            let offset = u64::try_from(*offset)
                                .map_err(|_| AnalyticalCostError::Overflow)?;
                            let operator = PhysicalOperator::TopK {
                                limit,
                                offset,
                                ordering_key_count: u64::try_from(keys.len())
                                    .map_err(|_| AnalyticalCostError::Overflow)?,
                            };
                            let evidence =
                                self.resolve(query, operator, occurrence, false, &children, None)?;
                            let statistics = &evidence.statistics;
                            let child_statistics = self.node_statistics(&child_id)?;
                            require_unary_edge(
                                &evidence.physical_id,
                                statistics,
                                &child_id,
                                child_statistics,
                            )?;
                            let bound = limit
                                .checked_add(offset)
                                .ok_or(AnalyticalCostError::Overflow)?;
                            if bound == 0 {
                                return Err(AnalyticalCostError::MissingOrZero("topk_k"));
                            }
                            require_operator_statistics(operator, statistics)?;
                            return self.push(evidence, operator, children, None);
                        }
                    }
                    let child_id = self.lower(child)?;
                    let children = vec![child_id.clone()];
                    let operator = PhysicalOperator::Limit {
                        limit: u64::try_from(*n).map_err(|_| AnalyticalCostError::Overflow)?,
                        offset: u64::try_from(*offset)
                            .map_err(|_| AnalyticalCostError::Overflow)?,
                    };
                    let evidence =
                        self.resolve(query, operator, occurrence, false, &children, None)?;
                    let statistics = &evidence.statistics;
                    let child_statistics = self.node_statistics(&child_id)?;
                    require_unary_edge(
                        &evidence.physical_id,
                        statistics,
                        &child_id,
                        child_statistics,
                    )?;
                    require_operator_statistics(operator, statistics)?;
                    self.push(evidence, operator, children, None)
                }
                NonASAPOp::SQLWindowFunc {
                    func,
                    partition_by,
                    order_by,
                    child,
                    ..
                } => {
                    if order_by.is_empty()
                        || !matches!(
                            func,
                            asap_types::pre_asap::WindowFuncKind::RowNumber
                                | asap_types::pre_asap::WindowFuncKind::Rank
                                | asap_types::pre_asap::WindowFuncKind::DenseRank
                        )
                    {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    self.lower_unary(
                        query,
                        occurrence,
                        PhysicalOperator::InMemoryAnalyticWindow {
                            partition_key_count: u64::try_from(partition_by.keys().len())
                                .map_err(|_| AnalyticalCostError::Overflow)?,
                            ordering_key_count: u64::try_from(order_by.len())
                                .map_err(|_| AnalyticalCostError::Overflow)?,
                            function_operations_per_row: 1,
                        },
                        child,
                    )
                }
                NonASAPOp::TimeRange { range, child, .. } => {
                    let range_millis = duration_millis(*range, "range")?;
                    self.lower_promql_unary(
                        query,
                        occurrence,
                        PhysicalOperator::PromqlRange { range_millis },
                        child,
                    )
                }
                NonASAPOp::PromqlSubquery {
                    range,
                    resolution,
                    child,
                } => {
                    let range_millis = duration_millis(*range, "subquery range")?;
                    let resolution_millis = resolution
                        .map(|value| duration_millis(value, "subquery resolution"))
                        .transpose()?;
                    let operator = PhysicalOperator::PromqlSubquery {
                        range_millis,
                        resolution_millis,
                    };
                    let id = self.lower_promql_unary(query, occurrence, operator, child)?;
                    let statistics = self.node_statistics(&id)?;
                    let OperatorStatistics::PromqlSubquery { subquery_steps, .. } = statistics
                    else {
                        unreachable!("operator/statistics matching was validated")
                    };
                    if let Some(resolution_millis) = resolution_millis {
                        let expected = range_millis
                            .checked_div(resolution_millis)
                            .and_then(|steps| steps.checked_add(1))
                            .ok_or(AnalyticalCostError::Overflow)?;
                        if *subquery_steps != expected {
                            return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                                "subquery steps disagree with range and resolution",
                            ));
                        }
                    }
                    Ok(id)
                }
                NonASAPOp::PromqlRelabel { value, child, .. } => self.lower_promql_unary(
                    query,
                    occurrence,
                    PhysicalOperator::PromqlRelabel {
                        expression_operations_per_row: scalar_operation_count(value)?.max(1),
                    },
                    child,
                ),
                NonASAPOp::PromqlSeriesSample {
                    by, kind, child, ..
                } => {
                    if by.is_without() {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    let kind = match kind {
                        asap_types::pre_asap::SampleKind::LimitK(k) => {
                            let k = u64::try_from(*k).map_err(|_| AnalyticalCostError::Overflow)?;
                            if k == 0 {
                                return Err(AnalyticalCostError::MissingOrZero("limitk"));
                            }
                            PromqlSeriesSampleKind::LimitK { k }
                        }
                        asap_types::pre_asap::SampleKind::LimitRatio(ratio)
                            if ratio.is_finite() && (-1.0..=1.0).contains(ratio) =>
                        {
                            PromqlSeriesSampleKind::LimitRatio {
                                ratio_bits: ratio.to_bits(),
                            }
                        }
                        asap_types::pre_asap::SampleKind::LimitRatio(_) => {
                            return Err(AnalyticalCostError::UnsupportedQueryOperator)
                        }
                    };
                    self.lower_promql_unary(
                        query,
                        occurrence,
                        PhysicalOperator::PromqlSeriesSample {
                            kind,
                            grouping_key_count: u64::try_from(by.keys().len())
                                .map_err(|_| AnalyticalCostError::Overflow)?,
                        },
                        child,
                    )
                }
                NonASAPOp::PromqlInfoEnrich { selector, child } => {
                    let left_id = self.lower(child)?;
                    let coverage = bind_info_coverage(
                        &format!("occurrence-{occurrence}-info"),
                        selector,
                        self.scope,
                    )?;
                    let info_evidence = self.resolve(
                        query,
                        PhysicalOperator::Scan,
                        occurrence,
                        true,
                        &[],
                        Some(&coverage),
                    )?;
                    require_statistics_shape(
                        &info_evidence.physical_id,
                        &info_evidence.statistics,
                        1,
                    )?;
                    require_scan_edges_equal(&info_evidence.statistics)?;
                    let info_statistics = info_evidence.statistics.clone();
                    let info_id = self.push(
                        info_evidence,
                        PhysicalOperator::Scan,
                        vec![],
                        Some(coverage),
                    )?;
                    let children = vec![left_id.clone(), info_id.clone()];
                    let operator = PhysicalOperator::PromqlInfoEnrich {
                        matcher_operations_per_info_row: u64::try_from(
                            selector
                                .iter()
                                .filter(|matcher| matcher.label != "__name__")
                                .count()
                                .max(1),
                        )
                        .map_err(|_| AnalyticalCostError::Overflow)?,
                    };
                    let evidence =
                        self.resolve(query, operator, occurrence, false, &children, None)?;
                    require_binary_edges(
                        &evidence.physical_id,
                        &evidence.statistics,
                        &left_id,
                        self.node_statistics(&left_id)?,
                        &info_id,
                        &info_statistics,
                    )?;
                    require_operator_statistics(operator, &evidence.statistics)?;
                    self.push(evidence, operator, children, None)
                }
                NonASAPOp::BinaryOp {
                    operator, lhs, rhs, ..
                } => {
                    let op = &operator.kind;
                    let vector_match = &operator.vector_match;
                    let operation = promql_binary_operation(op);
                    let operand_mode = PromqlBinaryOperandMode::VectorVector;
                    let cardinality = promql_vector_cardinality(vector_match.as_ref());
                    let left_id = self.lower(lhs)?;
                    let right_id = self.lower(rhs)?;
                    let children = vec![left_id.clone(), right_id.clone()];
                    let left_statistics = self.node_statistics(&left_id)?;
                    let right_statistics = self.node_statistics(&right_id)?;
                    let build_side = (operand_mode == PromqlBinaryOperandMode::VectorVector)
                        .then_some(
                            if left_statistics.output().bytes <= right_statistics.output().bytes {
                                HashJoinBuildSide::Left
                            } else {
                                HashJoinBuildSide::Right
                            },
                        );
                    let operator = PhysicalOperator::PromqlBinary {
                        operation,
                        operand_mode,
                        cardinality,
                        build_side,
                    };
                    let evidence =
                        self.resolve(query, operator, occurrence, false, &children, None)?;
                    require_binary_edges(
                        &evidence.physical_id,
                        &evidence.statistics,
                        &left_id,
                        left_statistics,
                        &right_id,
                        right_statistics,
                    )?;
                    require_operator_statistics(operator, &evidence.statistics)?;
                    self.push(evidence, operator, children, None)
                }
                NonASAPOp::PromqlVectorFromScalar(scalar) => {
                    let scalar_occurrence = self.next_id;
                    self.next_id += 1;
                    let child_id = self.lower_scalar_operand(query, scalar_occurrence, scalar)?;
                    self.push_unary(
                        query,
                        occurrence,
                        PhysicalOperator::PromqlScalarToVector,
                        child_id,
                    )
                }
                NonASAPOp::TimeShift { shift, child } => {
                    if !shift.is_identity() {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    self.lower_unary(query, occurrence, PhysicalOperator::PassThrough, child)
                }
                NonASAPOp::Concat { children, .. } => {
                    let child_ids = children
                        .iter()
                        .map(|child| self.lower(child))
                        .collect::<Result<Vec<_>, _>>()?;
                    self.lower_concat(query, occurrence, child_ids)
                }
                NonASAPOp::SetOp {
                    kind: RelationalSetOpKind::Union,
                    all: true,
                    left,
                    right,
                } => {
                    let left_id = self.lower(left)?;
                    let right_id = self.lower(right)?;
                    self.lower_concat(query, occurrence, vec![left_id, right_id])
                }
                NonASAPOp::Join {
                    kind,
                    pred,
                    left,
                    right,
                } => {
                    let equality_key_count =
                        if matches!(kind, asap_types::pre_asap::JoinKind::Cross) {
                            None
                        } else {
                            hash_join_key_count(&pred.0, left, right)
                        };
                    let Some(equality_key_count) = equality_key_count else {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    };
                    let left_id = self.lower(left)?;
                    let right_id = self.lower(right)?;
                    let children = vec![left_id.clone(), right_id.clone()];
                    let left_statistics = self.node_statistics(&left_id)?;
                    let right_statistics = self.node_statistics(&right_id)?;
                    let build_side =
                        if left_statistics.output().bytes <= right_statistics.output().bytes {
                            HashJoinBuildSide::Left
                        } else {
                            HashJoinBuildSide::Right
                        };
                    let operator = PhysicalOperator::HashJoin {
                        build_side,
                        equality_key_count,
                    };
                    let evidence =
                        self.resolve(query, operator, occurrence, false, &children, None)?;
                    let statistics = &evidence.statistics;
                    require_binary_edges(
                        &evidence.physical_id,
                        statistics,
                        &left_id,
                        left_statistics,
                        &right_id,
                        right_statistics,
                    )?;
                    require_operator_statistics(operator, statistics)?;
                    self.push(evidence, operator, children, None)
                }
                _ => Err(AnalyticalCostError::UnsupportedQueryOperator),
            }
        }

        fn lower_concat(
            &mut self,
            query: &OperatorNode,
            occurrence: usize,
            child_ids: Vec<String>,
        ) -> Result<String, AnalyticalCostError> {
            if child_ids.is_empty() {
                return Err(AnalyticalCostError::InvalidPhysicalDAG(
                    "concat has no children",
                ));
            }
            let evidence = self.resolve(
                query,
                PhysicalOperator::Concat,
                occurrence,
                false,
                &child_ids,
                None,
            )?;
            let statistics = &evidence.statistics;
            require_statistics_shape(&evidence.physical_id, statistics, child_ids.len())?;
            let (rows, bytes) = child_ids.iter().enumerate().try_fold(
                (0_u64, 0_u64),
                |(rows, bytes), (index, child)| {
                    let child_statistics = self.node_statistics(child)?;
                    if statistics.input(index) != Some(child_statistics.output()) {
                        return Err(AnalyticalCostError::ConflictingEdgeStatistics {
                            parent: evidence.physical_id.clone(),
                            child: child.clone(),
                            input_index: index,
                        });
                    }
                    require_promql_edge(statistics, index, child_statistics)?;
                    Ok::<_, AnalyticalCostError>((
                        rows.checked_add(child_statistics.output().rows)
                            .ok_or(AnalyticalCostError::Overflow)?,
                        bytes
                            .checked_add(child_statistics.output().bytes)
                            .ok_or(AnalyticalCostError::Overflow)?,
                    ))
                },
            )?;
            if statistics.output() != (EdgeStatistics { rows, bytes }) {
                return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                    "concat statistics do not equal the sum of child outputs",
                ));
            }
            require_operator_statistics(PhysicalOperator::Concat, statistics)?;
            self.push(evidence, PhysicalOperator::Concat, child_ids, None)
        }
    }

    let mut lowerer = Lowerer {
        scope,
        provider: evidence,
        evidence: HashMap::new(),
        next_id: 0,
        nodes: Vec::new(),
    };
    let root = lowerer.lower(root)?;
    validate_source_consumption(&lowerer.nodes, scope)?;
    Ok(EvidenceBackedPhysicalDAG {
        nodes: lowerer.nodes,
        root,
        evidence: lowerer.evidence,
    })
}

fn validate_source_consumption(
    nodes: &[PhysicalDAGNode],
    scope: &ComparisonScope,
) -> Result<(), AnalyticalCostError> {
    let consumed = nodes
        .iter()
        .filter(|node| matches!(node.operator, PhysicalOperator::Scan))
        .filter_map(|node| node.source_coverage.as_ref())
        .collect::<Vec<_>>();
    for coverage in &consumed {
        if !scope.sources.contains(coverage) {
            return Err(AnalyticalCostError::InvalidPhysicalDAG(
                "physical scan consumes a source outside the comparison scope",
            ));
        }
    }
    if scope
        .sources
        .iter()
        .any(|expected| !consumed.contains(&expected))
    {
        return Err(AnalyticalCostError::InvalidPhysicalDAG(
            "physical scans omit a comparison-scope source",
        ));
    }
    Ok(())
}

fn require_statistics_shape(
    node: &str,
    statistics: &OperatorStatistics,
    input_count: usize,
) -> Result<(), AnalyticalCostError> {
    if statistics.input_count() != input_count {
        return Err(AnalyticalCostError::InvalidOperatorStatistics {
            node: node.into(),
            reason: "wrong input-edge count",
        });
    }
    if (0..statistics.input_count())
        .filter_map(|index| statistics.input(index))
        .chain(std::iter::once(statistics.output()))
        .any(|edge| !edge.is_consistent())
    {
        return Err(AnalyticalCostError::InvalidOperatorStatistics {
            node: node.into(),
            reason: "edge rows and logical bytes are inconsistent",
        });
    }
    Ok(())
}

fn require_scan_edges_equal(statistics: &OperatorStatistics) -> Result<(), AnalyticalCostError> {
    if statistics.input(0) != Some(statistics.output()) {
        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "Scan external input edge does not match its output edge",
        ));
    }
    Ok(())
}

fn require_unary_edge(
    node: &str,
    statistics: &OperatorStatistics,
    child_id: &str,
    child: &OperatorStatistics,
) -> Result<(), AnalyticalCostError> {
    require_statistics_shape(node, statistics, 1)?;
    if statistics.input(0) != Some(child.output()) {
        return Err(AnalyticalCostError::ConflictingEdgeStatistics {
            parent: node.into(),
            child: child_id.into(),
            input_index: 0,
        });
    }
    Ok(())
}

fn require_binary_edges(
    node: &str,
    statistics: &OperatorStatistics,
    left_id: &str,
    left: &OperatorStatistics,
    right_id: &str,
    right: &OperatorStatistics,
) -> Result<(), AnalyticalCostError> {
    require_statistics_shape(node, statistics, 2)?;
    for (index, (child_id, child)) in [(left_id, left), (right_id, right)].into_iter().enumerate() {
        if statistics.input(index) != Some(child.output()) {
            return Err(AnalyticalCostError::ConflictingEdgeStatistics {
                parent: node.into(),
                child: child_id.into(),
                input_index: index,
            });
        }
        require_promql_edge(statistics, index, child)?;
    }
    Ok(())
}

fn require_promql_edge(
    parent: &OperatorStatistics,
    input_index: usize,
    child: &OperatorStatistics,
) -> Result<(), AnalyticalCostError> {
    match (parent.promql_input(input_index), child.promql_output()) {
        (None, None) => Ok(()),
        (Some(input), Some(output)) if input == output => Ok(()),
        (Some(_), Some(_)) => Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "PromQL parent input does not match child output",
        )),
        _ => Err(AnalyticalCostError::MissingOrStale(
            "promql_edge_statistics",
        )),
    }
}

fn require_operator_statistics(
    operator: PhysicalOperator,
    statistics: &OperatorStatistics,
) -> Result<(), AnalyticalCostError> {
    validate_operator_semantics(operator, statistics)
}

fn bind_scan_coverage(
    node_id: &str,
    source: &asap_types::pre_asap::Source,
    predicates: &[asap_types::ir::Predicate],
    scope: &ComparisonScope,
) -> Result<SourceCoverage, AnalyticalCostError> {
    let mut matches = scope.sources.iter().filter(|coverage| {
        coverage.source == *source
            && coverage.predicates == predicates
            && coverage.info_matchers.is_empty()
    });
    let coverage = matches
        .next()
        .cloned()
        .ok_or_else(|| AnalyticalCostError::ScanOutsideComparisonScope(node_id.into()))?;
    if matches.any(|candidate| candidate != &coverage) {
        return Err(AnalyticalCostError::InvalidPhysicalDAG(
            "scan source coverage is ambiguous",
        ));
    }
    Ok(coverage)
}

fn bind_info_coverage(
    node_id: &str,
    selector: &[asap_types::pre_asap::InfoMatcher],
    scope: &ComparisonScope,
) -> Result<SourceCoverage, AnalyticalCostError> {
    use asap_types::pre_asap::{CompareOpKind, Source};

    let mut metric: Option<&str> = None;
    for matcher in selector
        .iter()
        .filter(|matcher| matcher.label == "__name__")
    {
        if matcher.op != CompareOpKind::Eq || metric.is_some_and(|current| current != matcher.value)
        {
            return Err(AnalyticalCostError::UnsupportedQueryOperator);
        }
        metric = Some(&matcher.value);
    }
    let source = Source::TimeSeries {
        metric: metric.unwrap_or("target_info").into(),
    };
    let mut matches = scope.sources.iter().filter(|coverage| {
        coverage.source == source
            && coverage.predicates.is_empty()
            && coverage.info_matchers == selector
    });
    let coverage = matches
        .next()
        .cloned()
        .ok_or_else(|| AnalyticalCostError::ScanOutsideComparisonScope(node_id.into()))?;
    if matches.next().is_some() {
        return Err(AnalyticalCostError::InvalidPhysicalDAG(
            "info source coverage is ambiguous",
        ));
    }
    Ok(coverage)
}

fn duration_millis(
    duration: std::time::Duration,
    field: &'static str,
) -> Result<u64, AnalyticalCostError> {
    if duration.is_zero() {
        return Err(AnalyticalCostError::MissingOrZero(field));
    }
    u64::try_from(duration.as_millis()).map_err(|_| AnalyticalCostError::Overflow)
}

fn promql_binary_operation(
    operation: &asap_types::pre_asap::BinaryOpKind,
) -> PromqlBinaryOperation {
    use asap_types::pre_asap::{BinaryOpKind, PromQLVectorSetOpKind};
    match operation {
        BinaryOpKind::Set(PromQLVectorSetOpKind::And) => PromqlBinaryOperation::And,
        BinaryOpKind::Set(PromQLVectorSetOpKind::Or) => PromqlBinaryOperation::Or,
        BinaryOpKind::Set(PromQLVectorSetOpKind::Unless) => PromqlBinaryOperation::Unless,
        BinaryOpKind::Arithmetic(_) | BinaryOpKind::Compare(_) => {
            PromqlBinaryOperation::ArithmeticOrComparison
        }
    }
}

fn promql_vector_cardinality(
    vector_match: Option<&asap_types::pre_asap::VectorMatch>,
) -> PromqlVectorCardinality {
    use asap_types::pre_asap::GroupSide;
    match vector_match.and_then(|matching| matching.grouping.as_ref()) {
        Some(grouping) if grouping.side == GroupSide::Left => PromqlVectorCardinality::ManyToOne,
        Some(_) => PromqlVectorCardinality::OneToMany,
        None => PromqlVectorCardinality::OneToOne,
    }
}

fn hash_join_key_count(
    expr: &ScalarExpr,
    left: &OperatorNode,
    right: &OperatorNode,
) -> Option<u64> {
    use asap_types::pre_asap::CompareOpKind;

    let left_width = left.schema.fields.len();
    let total_width = left_width.saturating_add(right.schema.fields.len());

    fn column_side(column: usize, left_width: usize, total_width: usize) -> Option<bool> {
        if column < left_width {
            Some(false)
        } else if column < total_width {
            Some(true)
        } else {
            None
        }
    }

    fn predicate(expr: &ScalarExpr, left_width: usize, total_width: usize) -> Option<u64> {
        match expr {
            ScalarExpr::Compare {
                left,
                op: CompareOpKind::Eq,
                right,
                ..
            } => match (left.as_ref(), right.as_ref()) {
                (ScalarExpr::Column(left), ScalarExpr::Column(right)) => match (
                    column_side(*left, left_width, total_width),
                    column_side(*right, left_width, total_width),
                ) {
                    (Some(false), Some(true)) | (Some(true), Some(false)) => Some(1),
                    _ => None,
                },
                _ => None,
            },
            ScalarExpr::BoolAnd(parts) if !parts.is_empty() => {
                parts.iter().try_fold(0_u64, |count, part| {
                    count.checked_add(predicate(part, left_width, total_width)?)
                })
            }
            _ => None,
        }
    }

    predicate(expr, left_width, total_width)
}

fn scalar_operation_count(expr: &ScalarExpr) -> Result<u64, AnalyticalCostError> {
    let add = |parts: &[&ScalarExpr]| {
        parts.iter().try_fold(0_u64, |total, part| {
            total
                .checked_add(scalar_operation_count(part)?)
                .ok_or(AnalyticalCostError::Overflow)
        })
    };
    let with_local = |children| {
        add(children)?
            .checked_add(1)
            .ok_or(AnalyticalCostError::Overflow)
    };
    match expr {
        ScalarExpr::Column(_)
        | ScalarExpr::Literal(_)
        | ScalarExpr::EvalTimestamp
        | ScalarExpr::CurrentTimestamp => Ok(0),
        ScalarExpr::Compare { left, right, .. } | ScalarExpr::Arithmetic { left, right, .. } => {
            with_local(&[left, right])
        }
        ScalarExpr::BoolAnd(parts) | ScalarExpr::BoolOr(parts) => {
            let children = parts.iter().collect::<Vec<_>>();
            add(&children)?
                .checked_add(
                    u64::try_from(parts.len().saturating_sub(1))
                        .map_err(|_| AnalyticalCostError::Overflow)?,
                )
                .ok_or(AnalyticalCostError::Overflow)
        }
        ScalarExpr::Not(child) | ScalarExpr::IsNull(child) | ScalarExpr::IsNotNull(child) => {
            with_local(&[child])
        }
        ScalarExpr::Cast { expr, .. } => with_local(&[expr]),
        ScalarExpr::InList { expr, list, .. } => {
            let mut children = Vec::with_capacity(list.len() + 1);
            children.push(expr.as_ref());
            children.extend(list.iter());
            add(&children)?
                .checked_add(u64::try_from(list.len()).map_err(|_| AnalyticalCostError::Overflow)?)
                .ok_or(AnalyticalCostError::Overflow)
        }
        ScalarExpr::FunctionCall { args, .. } => {
            let children = args.iter().collect::<Vec<_>>();
            with_local(&children)
        }
        ScalarExpr::Case {
            operand,
            branches,
            else_expr,
        } => {
            let mut children = Vec::new();
            if let Some(operand) = operand {
                children.push(operand.as_ref());
            }
            for (when, then) in branches {
                children.extend([when, then]);
            }
            if let Some(else_expr) = else_expr {
                children.push(else_expr.as_ref());
            }
            with_local(&children)
        }
        _ => Err(AnalyticalCostError::UnsupportedQueryOperator),
    }
}

fn supports_hash_aggregate(
    reduction: &asap_types::pre_asap::Reduction,
    measures: &[asap_types::pre_asap::AggIntent],
) -> bool {
    use asap_types::pre_asap::{AggIntent, Reduction};

    matches!(reduction, Reduction::Reduce(_))
        && !measures.is_empty()
        && measures.iter().all(|intent| {
            matches!(
                intent,
                AggIntent::Count { .. }
                    | AggIntent::Sum { .. }
                    | AggIntent::Min { .. }
                    | AggIntent::Max { .. }
                    | AggIntent::Avg { .. }
                    | AggIntent::StdDev { .. }
                    | AggIntent::Variance { .. }
                    | AggIntent::PearsonCorr { .. }
                    | AggIntent::Group
                    | AggIntent::CountValues { .. }
                    | AggIntent::FrequencyL2 { .. }
                    | AggIntent::FrequencyEntropy { .. }
            )
        })
}

fn presence_intent(intent: &asap_types::pre_asap::AggIntent) -> bool {
    matches!(
        intent,
        asap_types::pre_asap::AggIntent::Absent | asap_types::pre_asap::AggIntent::AbsentOverTime
    )
}

fn present_over_time_intent(intent: &asap_types::pre_asap::AggIntent) -> bool {
    matches!(intent, asap_types::pre_asap::AggIntent::PresentOverTime)
}

fn fixed_state_per_series_intent(intent: &asap_types::pre_asap::AggIntent) -> bool {
    use asap_types::pre_asap::AggIntent;
    matches!(
        intent,
        AggIntent::Rate
            | AggIntent::Count { .. }
            | AggIntent::Sum { .. }
            | AggIntent::Min { .. }
            | AggIntent::Max { .. }
            | AggIntent::Avg { .. }
            | AggIntent::StdDev { .. }
            | AggIntent::Variance { .. }
            | AggIntent::Increase
            | AggIntent::Changes
            | AggIntent::Delta
            | AggIntent::IDelta
            | AggIntent::Deriv
            | AggIntent::Resets
            | AggIntent::PredictLinear { .. }
            | AggIntent::DoubleExpSmoothing { .. }
            | AggIntent::HistogramCount
            | AggIntent::HistogramSum
            | AggIntent::HistogramAvg
            | AggIntent::HistogramStdDev
            | AggIntent::HistogramStdVar
            | AggIntent::HistogramFraction { .. }
            | AggIntent::Math(_)
            | AggIntent::TimeFn(_)
            | AggIntent::LastOverTime
            | AggIntent::FirstOverTime
            | AggIntent::TsOfMinOverTime
            | AggIntent::TsOfMaxOverTime
            | AggIntent::TsOfFirstOverTime
            | AggIntent::TsOfLastOverTime
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytical_cost::{
        estimate_physical_dag, estimate_physical_dag_comparison, PhysicalDAGEstimateRequest,
    };
    use crate::physical_operator_statistics::{
        validate_comparison_scopes, BinaryEdgeStatistics, PartitionStatistics,
        PromqlEdgeStatistics, PromqlUnaryEdgeStatistics, PromqlValueKind, UnaryEdgeStatistics,
    };
    use asap_types::ir::{BinaryOperator, ExprSemantics, Predicate, SortKey, TimeRangeKind};
    use asap_types::workload::{
        DataArrival, DurationMs, QueryRecurrence, QueryTimeScope, TimeSelection, TimestampMs,
    };
    use std::collections::HashMap;

    fn edge(rows: u64, bytes: u64) -> EdgeStatistics {
        EdgeStatistics { rows, bytes }
    }

    fn unary_edges(input: EdgeStatistics, output: EdgeStatistics) -> UnaryEdgeStatistics {
        UnaryEdgeStatistics {
            input,
            output,
            promql: None,
        }
    }

    fn scan_stats(edge: EdgeStatistics, source_read_bytes: u64) -> OperatorStatistics {
        OperatorStatistics::Scan {
            edges: unary_edges(edge, edge),
            source_read_bytes,
        }
    }

    fn promql_edge(
        series: u64,
        evaluation_steps: u64,
        value_kind: PromqlValueKind,
    ) -> PromqlEdgeStatistics {
        PromqlEdgeStatistics {
            series,
            evaluation_steps,
            value_kind,
        }
    }

    fn promql_unary_edges(
        input: EdgeStatistics,
        output: EdgeStatistics,
        promql_input: PromqlEdgeStatistics,
        promql_output: PromqlEdgeStatistics,
    ) -> UnaryEdgeStatistics {
        UnaryEdgeStatistics {
            input,
            output,
            promql: Some(PromqlUnaryEdgeStatistics {
                input: promql_input,
                output: promql_output,
            }),
        }
    }

    fn unary_statistics(
        operator: PhysicalOperator,
        input: EdgeStatistics,
        output: EdgeStatistics,
    ) -> OperatorStatistics {
        let edges = unary_edges(input, output);
        match operator {
            PhysicalOperator::Filter { .. } => OperatorStatistics::Filter { edges },
            PhysicalOperator::Project { .. } => OperatorStatistics::Project { edges },
            PhysicalOperator::InMemoryComparisonSort { .. } => {
                OperatorStatistics::InMemoryComparisonSort {
                    edges,
                    input_partitioning: PartitionStatistics {
                        partitions: (!input.eq(&edge(0, 0)))
                            .then_some(input)
                            .into_iter()
                            .collect(),
                    },
                }
            }
            PhysicalOperator::TopK { .. } => OperatorStatistics::TopK { edges },
            PhysicalOperator::InMemoryAnalyticWindow { .. } => {
                OperatorStatistics::InMemoryAnalyticWindow {
                    edges,
                    input_partitioning: PartitionStatistics {
                        partitions: (!input.eq(&edge(0, 0)))
                            .then_some(input)
                            .into_iter()
                            .collect(),
                    },
                }
            }
            PhysicalOperator::Limit { .. } => OperatorStatistics::Limit { edges },
            PhysicalOperator::PassThrough => OperatorStatistics::PassThrough { edges },
            _ => panic!("test helper requires a stateless unary operator"),
        }
    }

    fn evidence(statistics: OperatorStatistics) -> PhysicalNodeEvidence {
        PhysicalNodeEvidence {
            physical_id: String::new(),
            output_buffer_bytes: statistics.output().bytes.min(1_024),
            statistics,
        }
    }

    fn scripted<'a>(
        provided: &'a HashMap<String, PhysicalNodeEvidence>,
    ) -> impl Fn(PhysicalNodeRequest<'_>) -> Result<PhysicalNodeEvidence, AnalyticalCostError> + 'a
    {
        move |request| {
            let key = if request.synthetic {
                format!("query-{}-scan", request.occurrence)
            } else {
                format!("query-{}", request.occurrence)
            };
            let mut evidence = provided
                .get(&key)
                .cloned()
                .ok_or_else(|| AnalyticalCostError::MissingOperatorStatistics(key.clone()))?;
            evidence.physical_id = key;
            Ok(evidence)
        }
    }

    fn scope(sources: Vec<SourceCoverage>) -> ComparisonScope {
        ComparisonScope {
            data_arrival: DataArrival::AtRest,
            planning_time: TimestampMs(1_000),
            horizon: DurationMs(1_000),
            recurrence: QueryRecurrence::OneTime {
                invocations: 1,
                execute_at: None,
            },
            time_selection: TimeSelection {
                scope: QueryTimeScope::Longitudinal,
                lookback: Some(DurationMs(1_000)),
                as_of: Some(TimestampMs(1_000)),
            },
            sources,
        }
    }

    fn coverage(
        source: asap_types::pre_asap::Source,
        predicates: Vec<Predicate>,
    ) -> SourceCoverage {
        SourceCoverage {
            source,
            source_snapshot_id: "snapshot-1".into(),
            predicates,
            info_matchers: vec![],
        }
    }

    #[test]
    fn info_source_coverage_includes_symbolic_selector_matchers() {
        use asap_types::pre_asap::{CompareOpKind, InfoMatcher, Source};

        let selector = vec![InfoMatcher {
            label: "cluster".into(),
            op: CompareOpKind::Eq,
            value: "prod".into(),
        }];
        let info_coverage = SourceCoverage {
            source: Source::TimeSeries {
                metric: "target_info".into(),
            },
            source_snapshot_id: "snapshot-1".into(),
            predicates: vec![],
            info_matchers: selector.clone(),
        };
        let matching_scope = scope(vec![info_coverage.clone()]);
        assert_eq!(
            bind_info_coverage("info", &selector, &matching_scope),
            Ok(info_coverage)
        );

        let wrong_scope = scope(vec![coverage(
            Source::TimeSeries {
                metric: "target_info".into(),
            },
            vec![],
        )]);
        assert!(matches!(
            bind_info_coverage("info", &selector, &wrong_scope),
            Err(AnalyticalCostError::ScanOutsideComparisonScope(_))
        ));
    }

    // Correlation can be costed as an exact hash aggregate using provider-supplied state size.
    #[test]
    fn correlation_lowers_to_physical_hash_aggregate() {
        use asap_types::pre_asap::{AggIntent, DataType, Field, Reduction, Schema, Source};
        let source = Source::Table {
            table_ref: "pairs".into(),
        };
        let root =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: Reduction::by(vec![]),
                measures: vec![AggIntent::PearsonCorr { left: 0, right: 1 }],
                output_names: vec!["r".into()],
                filters: vec![],
                having: None,
                child: OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(
                    NonASAPOp::Scan {
                        source: source.clone(),
                        predicates: vec![],
                        schema: Schema::new(vec![
                            Field::plain("x", DataType::Float64, true),
                            Field::plain("y", DataType::Float64, true),
                        ]),
                    },
                ))
                .unwrap(),
            }))
            .unwrap();
        let scope = scope(vec![coverage(source, vec![])]);
        let provided = HashMap::from([
            (
                "query-1".into(),
                evidence(scan_stats(edge(100, 1600), 1600)),
            ),
            (
                "query-0".into(),
                evidence(OperatorStatistics::HashAggregate {
                    edges: unary_edges(edge(100, 1600), edge(1, 8)),
                    group_count: 1,
                    key_bytes: 0,
                    accumulator_bytes_per_group: 48,
                }),
            ),
        ]);
        let dag = lower_query_physical_dag(&root, &scope, &scripted(&provided)).unwrap();
        assert!(matches!(
            dag.nodes.last().unwrap().operator,
            PhysicalOperator::HashAggregate {
                grouping_key_count: 0,
                accumulator_count: 1,
            }
        ));
        assert!(estimate_physical_dag(&dag.nodes, &dag.root, &scope, &dag.evidence).is_ok());
    }

    #[test]
    fn query_lowering_recurses_and_fuses_global_sort_limit() {
        use asap_types::pre_asap::{AggIntent, GroupKeys, Reduction, Source};
        use asap_types::pre_asap::{DataType, Field, Schema};
        use std::rc::Rc;

        let scan = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "events".into(),
            },
            predicates: vec![Predicate(ScalarExpr::Literal(
                asap_types::pre_asap::ScalarValue::Boolean(true),
            ))],
            schema: Schema::new(vec![
                Field::plain("service", DataType::Utf8, false),
                Field::plain("value", DataType::Float64, false),
            ]),
        }))
        .unwrap();
        let aggregate =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: Reduction::by(vec![0]),
                measures: vec![AggIntent::Sum { col: Some(1) }],
                output_names: vec![],
                filters: vec![],
                having: None,
                child: Rc::clone(&scan),
            }))
            .unwrap();
        let sort = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Sort {
            keys: vec![SortKey {
                expr: ScalarExpr::Column(0),
                ascending: false,
                nulls_first: false,
            }],
            partition_by: GroupKeys::none(),
            child: aggregate,
        }))
        .unwrap();
        let root = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Limit {
            n: Some(10),
            offset: 5,
            partition_by: GroupKeys::none(),
            child: sort,
        }))
        .unwrap();

        let scan_coverage = coverage(
            Source::Table {
                table_ref: "events".into(),
            },
            vec![Predicate(ScalarExpr::Literal(
                asap_types::pre_asap::ScalarValue::Boolean(true),
            ))],
        );
        let scope = scope(vec![scan_coverage]);
        let aggregate_statistics = OperatorStatistics::HashAggregate {
            edges: unary_edges(edge(400, 25_600), edge(100, 4_000)),
            group_count: 100,
            key_bytes: 16,
            accumulator_bytes_per_group: 8,
        };
        let topk_statistics = unary_statistics(
            PhysicalOperator::TopK {
                limit: 10,
                offset: 5,
                ordering_key_count: 1,
            },
            edge(100, 4_000),
            edge(10, 400),
        );
        let raw_scan = scan_stats(edge(1_000, 64_000), 64_000);
        let provided = HashMap::from([
            ("query-2-scan".into(), evidence(raw_scan)),
            (
                "query-2".into(),
                evidence(unary_statistics(
                    PhysicalOperator::Filter {
                        predicate_operations_per_row: 1,
                    },
                    edge(1_000, 64_000),
                    edge(400, 25_600),
                )),
            ),
            ("query-1".into(), evidence(aggregate_statistics)),
            ("query-0".into(), evidence(topk_statistics)),
        ]);
        let dag = lower_query_physical_dag(&root, &scope, &scripted(&provided)).unwrap();

        assert_eq!(
            dag.nodes
                .iter()
                .map(|node| node.operator)
                .collect::<Vec<_>>(),
            vec![
                PhysicalOperator::Scan,
                PhysicalOperator::Filter {
                    predicate_operations_per_row: 1,
                },
                PhysicalOperator::HashAggregate {
                    grouping_key_count: 1,
                    accumulator_count: 1,
                },
                PhysicalOperator::TopK {
                    limit: 10,
                    offset: 5,
                    ordering_key_count: 1,
                },
            ]
        );
        let topk = dag.nodes.last().unwrap();
        assert_eq!(topk.children, vec![dag.nodes[2].id.clone()]);
        assert!(matches!(
            provided[&topk.id].statistics,
            OperatorStatistics::TopK { .. }
        ));
        let physical_scan = &dag.nodes[0];
        assert_eq!(physical_scan.id, "query-2-scan");
        assert_eq!(
            physical_scan.source_coverage,
            Some(scope.sources[0].clone())
        );
        assert_eq!(physical_scan.output_buffer_bytes, 1_024);
        assert_ne!(
            physical_scan.output_buffer_bytes,
            provided[&physical_scan.id].statistics.output().bytes
        );
        assert!(estimate_physical_dag(&dag.nodes, &dag.root, &scope, &dag.evidence).is_ok());

        let mut inconsistent_scan = provided.clone();
        inconsistent_scan
            .get_mut("query-2-scan")
            .unwrap()
            .statistics = OperatorStatistics::Scan {
            edges: unary_edges(edge(1_000, 64_000), edge(999, 63_936)),
            source_read_bytes: 64_000,
        };
        assert_eq!(
            lower_query_physical_dag(&root, &scope, &scripted(&inconsistent_scan)),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "Scan external input edge does not match its output edge"
            ))
        );
    }

    #[test]
    fn query_lowering_shares_only_provider_identified_physical_nodes() {
        use asap_types::pre_asap::{CompareOpKind, DataType, Field, Schema};
        use asap_types::pre_asap::{JoinKind, Source};
        use std::rc::Rc;

        let shared = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "dimensions".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Field::plain("id", DataType::Int64, false)]),
        }))
        .unwrap();
        let root = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Join {
            kind: JoinKind::Inner,
            pred: Predicate(ScalarExpr::Compare {
                left: Box::new(ScalarExpr::Column(0)),
                op: CompareOpKind::Eq,
                right: Box::new(ScalarExpr::Column(1)),
                semantics: ExprSemantics::Sql,
            }),
            left: Rc::clone(&shared),
            right: Rc::clone(&shared),
        }))
        .unwrap();
        let source_coverage = coverage(
            Source::Table {
                table_ref: "dimensions".into(),
            },
            vec![],
        );
        let independent_scope = scope(vec![source_coverage.clone()]);
        let scan_statistics = scan_stats(edge(100, 800), 800);
        let join_statistics = OperatorStatistics::HashJoin {
            edges: BinaryEdgeStatistics {
                inputs: [edge(100, 800), edge(100, 800)],
                output: edge(25, 400),
                promql: None,
            },
        };
        let provided = HashMap::from([
            ("query-1".into(), evidence(scan_statistics.clone())),
            ("query-2".into(), evidence(scan_statistics.clone())),
            ("query-0".into(), evidence(join_statistics.clone())),
        ]);
        let dag =
            lower_query_physical_dag(&root, &independent_scope, &scripted(&provided)).unwrap();

        assert_eq!(dag.nodes.len(), 3);
        assert_ne!(dag.nodes[2].children[0], dag.nodes[2].children[1]);
        let estimate =
            estimate_physical_dag(&dag.nodes, &dag.root, &independent_scope, &dag.evidence)
                .unwrap();
        assert_eq!(estimate.scan_bytes(), 1_600);

        let shared_provider = |request: PhysicalNodeRequest<'_>| {
            let (physical_id, statistics) = match request.operator {
                PhysicalOperator::Scan => ("shared-scan", scan_statistics.clone()),
                PhysicalOperator::HashJoin { .. } => ("join", join_statistics.clone()),
                _ => return Err(AnalyticalCostError::UnsupportedQueryOperator),
            };
            Ok(PhysicalNodeEvidence {
                physical_id: physical_id.into(),
                output_buffer_bytes: statistics.output().bytes.min(1_024),
                statistics,
            })
        };
        let shared_scope = scope(vec![source_coverage]);
        let no_cache = crate::analytical_cost::CacheProfile::no_cache();
        let shared_dag = lower_query_physical_dag(&root, &shared_scope, &shared_provider).unwrap();
        assert_eq!(shared_dag.nodes.len(), 2);
        assert_eq!(
            shared_dag.nodes[1].children,
            vec!["shared-scan".to_owned(); 2]
        );
        let comparison = estimate_physical_dag_comparison(
            PhysicalDAGEstimateRequest {
                nodes: &dag.nodes,
                root: &dag.root,
                scope: &independent_scope,
                statistics: &dag,
                cache_profile: &no_cache,
            },
            PhysicalDAGEstimateRequest {
                nodes: &shared_dag.nodes,
                root: &shared_dag.root,
                scope: &shared_scope,
                statistics: &shared_dag,
                cache_profile: &no_cache,
            },
        )
        .unwrap();
        assert_eq!(comparison.raw.scan_bytes(), 1_600);
        assert_eq!(comparison.candidate.scan_bytes(), 800);

        let mut drifted_buffer = shared_dag.clone();
        drifted_buffer.nodes[0].output_buffer_bytes += 1;
        assert_eq!(
            estimate_physical_dag(
                &drifted_buffer.nodes,
                &drifted_buffer.root,
                &shared_scope,
                &drifted_buffer,
            ),
            Err(AnalyticalCostError::InvalidPhysicalDAG(
                "physical node buffer differs from evidence snapshot"
            ))
        );
        let mut drifted_identity = shared_dag.clone();
        drifted_identity
            .evidence
            .get_mut("shared-scan")
            .unwrap()
            .physical_id = "different".into();
        assert_eq!(
            estimate_physical_dag(
                &drifted_identity.nodes,
                &drifted_identity.root,
                &shared_scope,
                &drifted_identity,
            ),
            Err(AnalyticalCostError::InvalidPhysicalDAG(
                "evidence map key differs from embedded physical identity"
            ))
        );

        let conflicting_identity = |request: PhysicalNodeRequest<'_>| {
            let (physical_id, mut statistics) = match request.operator {
                PhysicalOperator::Scan => ("shared-scan", scan_statistics.clone()),
                PhysicalOperator::HashJoin { .. } => ("join", join_statistics.clone()),
                _ => return Err(AnalyticalCostError::UnsupportedQueryOperator),
            };
            if request.operator == PhysicalOperator::Scan && request.occurrence == 2 {
                statistics = scan_stats(edge(100, 800), 801);
            }
            Ok(PhysicalNodeEvidence {
                physical_id: physical_id.into(),
                output_buffer_bytes: statistics.output().bytes.min(1_024),
                statistics,
            })
        };
        assert_eq!(
            lower_query_physical_dag(&root, &shared_scope, &conflicting_identity),
            Err(AnalyticalCostError::InvalidPhysicalDAG(
                "provider reused a physical identity for conflicting evidence"
            ))
        );

        let invalid =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Join {
                kind: JoinKind::Inner,
                pred: Predicate(ScalarExpr::Compare {
                    left: Box::new(ScalarExpr::Column(0)),
                    op: CompareOpKind::Eq,
                    right: Box::new(ScalarExpr::Column(0)),
                    semantics: ExprSemantics::Sql,
                }),
                left: Rc::clone(&shared),
                right: Rc::clone(&shared),
            }))
            .unwrap();
        assert_eq!(
            lower_query_physical_dag(&invalid, &shared_scope, &shared_provider),
            Err(AnalyticalCostError::UnsupportedQueryOperator)
        );
    }

    #[test]
    fn query_lowering_covers_relational_unary_operators() {
        use asap_types::pre_asap::{DataType, Field, ScalarValue, Schema};
        use asap_types::pre_asap::{GroupKeys, Source, TimeShift, WindowFuncKind};

        let scan = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "events".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Field::plain("id", DataType::Int64, false)]),
        }))
        .unwrap();
        let filter =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Filter {
                pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
                child: scan,
            }))
            .unwrap();
        let project =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Project {
                cols: vec![],
                qualifier: None,
                child: filter,
            }))
            .unwrap();
        let dedup = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Dedup {
            cols: vec![0],
            child: project,
        }))
        .unwrap();
        let window = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(
            NonASAPOp::SQLWindowFunc {
                func: WindowFuncKind::RowNumber,
                args: vec![],
                partition_by: GroupKeys::none(),
                order_by: vec![SortKey {
                    expr: ScalarExpr::Column(0),
                    ascending: true,
                    nulls_first: false,
                }],
                frame: None,
                output_name: "rn".into(),
                child: dedup,
            },
        ))
        .unwrap();
        let sort = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Sort {
            keys: vec![SortKey {
                expr: ScalarExpr::Column(0),
                ascending: true,
                nulls_first: false,
            }],
            partition_by: GroupKeys::by(vec![0]),
            child: window,
        }))
        .unwrap();
        let limit = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Limit {
            n: Some(20),
            offset: 0,
            partition_by: GroupKeys::none(),
            child: sort,
        }))
        .unwrap();
        let root =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::TimeShift {
                shift: TimeShift::default(),
                child: limit,
            }))
            .unwrap();

        let source_coverage = coverage(
            Source::Table {
                table_ref: "events".into(),
            },
            vec![],
        );
        let scope = scope(vec![source_coverage]);
        let scan_statistics = scan_stats(edge(1_000, 8_000), 8_000);
        let dedup_statistics = OperatorStatistics::HashDeduplicate {
            edges: unary_edges(edge(800, 3_200), edge(500, 2_000)),
            distinct_key_count: 500,
            key_bytes: 8,
        };
        let provided = HashMap::from([
            ("query-7".into(), evidence(scan_statistics)),
            (
                "query-6".into(),
                evidence(unary_statistics(
                    PhysicalOperator::Filter {
                        predicate_operations_per_row: 1,
                    },
                    edge(1_000, 8_000),
                    edge(800, 6_400),
                )),
            ),
            (
                "query-5".into(),
                evidence(unary_statistics(
                    PhysicalOperator::Project {
                        expression_operations_per_row: 1,
                    },
                    edge(800, 6_400),
                    edge(800, 3_200),
                )),
            ),
            ("query-4".into(), evidence(dedup_statistics)),
            (
                "query-3".into(),
                evidence(unary_statistics(
                    PhysicalOperator::InMemoryAnalyticWindow {
                        partition_key_count: 0,
                        ordering_key_count: 1,
                        function_operations_per_row: 1,
                    },
                    edge(500, 2_000),
                    edge(500, 6_000),
                )),
            ),
            (
                "query-2".into(),
                evidence(unary_statistics(
                    PhysicalOperator::InMemoryComparisonSort {
                        ordering_key_count: 1,
                        partitioned: true,
                    },
                    edge(500, 6_000),
                    edge(500, 6_000),
                )),
            ),
            (
                "query-1".into(),
                evidence(unary_statistics(
                    PhysicalOperator::Limit {
                        limit: 20,
                        offset: 0,
                    },
                    edge(500, 6_000),
                    edge(20, 240),
                )),
            ),
            (
                "query-0".into(),
                evidence(unary_statistics(
                    PhysicalOperator::PassThrough,
                    edge(20, 240),
                    edge(20, 240),
                )),
            ),
        ]);
        let dag = lower_query_physical_dag(&root, &scope, &scripted(&provided)).unwrap();

        assert_eq!(
            dag.nodes
                .iter()
                .map(|node| node.operator)
                .collect::<Vec<_>>(),
            vec![
                PhysicalOperator::Scan,
                PhysicalOperator::Filter {
                    predicate_operations_per_row: 1,
                },
                PhysicalOperator::Project {
                    expression_operations_per_row: 1,
                },
                PhysicalOperator::HashDeduplicate { key_count: 1 },
                PhysicalOperator::InMemoryAnalyticWindow {
                    partition_key_count: 0,
                    ordering_key_count: 1,
                    function_operations_per_row: 1,
                },
                PhysicalOperator::InMemoryComparisonSort {
                    ordering_key_count: 1,
                    partitioned: true,
                },
                PhysicalOperator::Limit {
                    limit: 20,
                    offset: 0,
                },
                PhysicalOperator::PassThrough,
            ]
        );
        assert!(estimate_physical_dag(&dag.nodes, &dag.root, &scope, &dag.evidence).is_ok());
    }

    #[test]
    fn query_lowering_maps_concat_and_union_all_but_rejects_distinct_set_ops() {
        use asap_types::pre_asap::{DataType, Field, Schema};
        use asap_types::pre_asap::{RelationalSetOpKind, Source};

        let scan = |name: &str| {
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
                source: Source::Table {
                    table_ref: name.into(),
                },
                predicates: vec![],
                schema: Schema::new(vec![Field::plain("id", DataType::Int64, false)]),
            }))
            .unwrap()
        };
        let union = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::SetOp {
            kind: RelationalSetOpKind::Union,
            all: true,
            left: scan("a"),
            right: scan("b"),
        }))
        .unwrap();
        let scope = scope(vec![
            coverage(
                Source::Table {
                    table_ref: "a".into(),
                },
                vec![],
            ),
            coverage(
                Source::Table {
                    table_ref: "b".into(),
                },
                vec![],
            ),
        ]);
        let left_statistics = scan_stats(edge(10, 80), 80);
        let right_statistics = scan_stats(edge(20, 160), 160);
        let provided = HashMap::from([
            ("query-1".into(), evidence(left_statistics)),
            ("query-2".into(), evidence(right_statistics)),
            (
                "query-0".into(),
                evidence(OperatorStatistics::Concat {
                    inputs: vec![edge(10, 80), edge(20, 160)],
                    output: edge(30, 240),
                    promql: None,
                }),
            ),
        ]);
        let dag = lower_query_physical_dag(&union, &scope, &scripted(&provided)).unwrap();
        assert_eq!(dag.nodes.last().unwrap().operator, PhysicalOperator::Concat);

        let mut reversed_scope = scope.clone();
        reversed_scope.sources.reverse();
        assert_eq!(validate_comparison_scopes(&scope, &reversed_scope), Ok(1));
        let mut duplicate_scope = scope.clone();
        duplicate_scope.sources.push(scope.sources[0].clone());
        assert_eq!(
            duplicate_scope.validate(),
            Err(AnalyticalCostError::MissingComparisonScope(
                "duplicate source coverage"
            ))
        );

        let concat =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Concat {
                children: vec![scan("a"), scan("b")],
                discriminator_unique_key: None,
            }))
            .unwrap();
        let dag = lower_query_physical_dag(&concat, &scope, &scripted(&provided)).unwrap();
        assert_eq!(dag.nodes.last().unwrap().operator, PhysicalOperator::Concat);

        let distinct_union =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::SetOp {
                kind: RelationalSetOpKind::Union,
                all: false,
                left: scan("a"),
                right: scan("b"),
            }))
            .unwrap();
        assert_eq!(
            lower_query_physical_dag(&distinct_union, &scope, &scripted(&provided)),
            Err(AnalyticalCostError::UnsupportedQueryOperator)
        );
    }

    #[test]
    fn query_lowering_fails_closed_for_missing_or_inconsistent_statistics() {
        use asap_types::pre_asap::Source;
        use asap_types::pre_asap::{DataType, Field, Schema};

        let scan = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "events".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Field::plain("id", DataType::Int64, false)]),
        }))
        .unwrap();
        let root =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Project {
                cols: vec![],
                qualifier: None,
                child: scan,
            }))
            .unwrap();

        let comparison_scope = scope(vec![coverage(
            Source::Table {
                table_ref: "events".into(),
            },
            vec![],
        )]);
        let missing = HashMap::<String, PhysicalNodeEvidence>::new();
        assert_eq!(
            lower_query_physical_dag(&root, &comparison_scope, &scripted(&missing)),
            Err(AnalyticalCostError::MissingOperatorStatistics(
                "query-1".into()
            ))
        );
        struct MissingBuffer;
        impl PhysicalNodeEvidenceProvider for MissingBuffer {
            fn evidence(
                &self,
                _request: PhysicalNodeRequest<'_>,
            ) -> Result<PhysicalNodeEvidence, AnalyticalCostError> {
                Err(AnalyticalCostError::MissingOrStale("output_buffer_bytes"))
            }
        }
        assert_eq!(
            lower_query_physical_dag(&root, &comparison_scope, &MissingBuffer),
            Err(AnalyticalCostError::MissingOrStale("output_buffer_bytes"))
        );
        let scan_statistics = scan_stats(edge(100, 800), 800);
        let conflicting = HashMap::from([
            ("query-1".into(), evidence(scan_statistics)),
            (
                "query-0".into(),
                evidence(unary_statistics(
                    PhysicalOperator::Project {
                        expression_operations_per_row: 1,
                    },
                    edge(99, 792),
                    edge(99, 396),
                )),
            ),
        ]);
        assert_eq!(
            lower_query_physical_dag(&root, &comparison_scope, &scripted(&conflicting)),
            Err(AnalyticalCostError::ConflictingEdgeStatistics {
                parent: "query-0".into(),
                child: "query-1".into(),
                input_index: 0,
            })
        );

        let outside_scope = scope(vec![coverage(
            Source::Table {
                table_ref: "other".into(),
            },
            vec![],
        )]);
        assert_eq!(
            lower_query_physical_dag(&root, &outside_scope, &scripted(&conflicting)),
            Err(AnalyticalCostError::ScanOutsideComparisonScope(
                "occurrence-1".into()
            ))
        );

        let mut second_snapshot = comparison_scope.sources[0].clone();
        second_snapshot.source_snapshot_id = "snapshot-2".into();
        let ambiguous_scope = scope(vec![comparison_scope.sources[0].clone(), second_snapshot]);
        assert_eq!(
            lower_query_physical_dag(&root, &ambiguous_scope, &scripted(&conflicting)),
            Err(AnalyticalCostError::InvalidPhysicalDAG(
                "scan source coverage is ambiguous"
            ))
        );

        let mut complete = conflicting.clone();
        complete.get_mut("query-0").unwrap().statistics = unary_statistics(
            PhysicalOperator::Project {
                expression_operations_per_row: 1,
            },
            edge(100, 800),
            edge(100, 400),
        );
        let extra_scope = scope(vec![
            comparison_scope.sources[0].clone(),
            coverage(
                Source::Table {
                    table_ref: "unused".into(),
                },
                vec![],
            ),
        ]);
        assert_eq!(
            lower_query_physical_dag(&root, &extra_scope, &scripted(&complete)),
            Err(AnalyticalCostError::InvalidPhysicalDAG(
                "physical scans omit a comparison-scope source"
            ))
        );
    }

    #[test]
    fn query_lowering_accepts_a_consistently_empty_edge() {
        use asap_types::pre_asap::{DataType, Field, ScalarValue, Schema};
        use asap_types::pre_asap::{GroupKeys, Source};

        let scan = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "events".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Field::plain("id", DataType::Int64, false)]),
        }))
        .unwrap();
        let filter =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Filter {
                pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(false))),
                child: scan,
            }))
            .unwrap();
        let root = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Limit {
            n: Some(10),
            offset: 0,
            partition_by: GroupKeys::none(),
            child: filter,
        }))
        .unwrap();

        let scope = scope(vec![coverage(
            Source::Table {
                table_ref: "events".into(),
            },
            vec![],
        )]);
        let scan_statistics = scan_stats(edge(100, 800), 800);
        let provided = HashMap::from([
            ("query-2".into(), evidence(scan_statistics)),
            (
                "query-1".into(),
                evidence(unary_statistics(
                    PhysicalOperator::Filter {
                        predicate_operations_per_row: 1,
                    },
                    edge(100, 800),
                    edge(0, 0),
                )),
            ),
            (
                "query-0".into(),
                evidence(unary_statistics(
                    PhysicalOperator::Limit {
                        limit: 10,
                        offset: 0,
                    },
                    edge(0, 0),
                    edge(0, 0),
                )),
            ),
        ]);
        let dag = lower_query_physical_dag(&root, &scope, &scripted(&provided)).unwrap();
        let estimate = estimate_physical_dag(&dag.nodes, &dag.root, &scope, &dag.evidence).unwrap();
        assert_eq!(estimate.cpu_ops(), 200.0);
        assert_eq!(estimate.scan_bytes(), 800);
    }

    #[test]
    fn query_lowering_rejects_aggregates_without_a_hash_implementation() {
        use asap_types::pre_asap::{AggIntent, GroupKeys, Reduction, Source, WindowFuncKind};
        use asap_types::pre_asap::{DataType, Field, Schema};
        use asap_types::types::AccuracyTarget;

        let scan = || {
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
                source: Source::Table {
                    table_ref: "events".into(),
                },
                predicates: vec![],
                schema: Schema::new(vec![Field::plain("value", DataType::Float64, false)]),
            }))
            .unwrap()
        };
        let exact_quantile =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: Reduction::by(vec![]),
                measures: vec![AggIntent::Quantile {
                    col: Some(0),
                    q: 0.99,
                    accuracy: AccuracyTarget::Exact,
                }],
                output_names: vec![],
                filters: vec![],
                having: None,
                child: scan(),
            }))
            .unwrap();
        let empty_sort_limit =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Limit {
                n: Some(10),
                offset: 0,
                partition_by: GroupKeys::none(),
                child: OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(
                    NonASAPOp::Sort {
                        keys: vec![],
                        partition_by: GroupKeys::none(),
                        child: scan(),
                    },
                ))
                .unwrap(),
            }))
            .unwrap();
        let unsupported_window = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(
            NonASAPOp::SQLWindowFunc {
                func: WindowFuncKind::Lag,
                args: vec![ScalarExpr::Column(0)],
                partition_by: GroupKeys::none(),
                order_by: vec![],
                frame: None,
                output_name: "lag".into(),
                child: scan(),
            },
        ))
        .unwrap();
        let shifted =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::TimeShift {
                shift: asap_types::pre_asap::TimeShift {
                    offset_ms: 60_000,
                    at: None,
                },
                child: scan(),
            }))
            .unwrap();
        let scope = scope(vec![coverage(
            Source::Table {
                table_ref: "events".into(),
            },
            vec![],
        )]);
        let unavailable = HashMap::<String, PhysicalNodeEvidence>::new();
        for query in [
            &exact_quantile,
            &empty_sort_limit,
            &unsupported_window,
            &shifted,
        ] {
            assert_eq!(
                lower_query_physical_dag(query, &scope, &scripted(&unavailable)),
                Err(AnalyticalCostError::UnsupportedQueryOperator)
            );
        }
    }

    #[test]
    fn scalar_work_counts_every_local_predicate_operation() {
        use asap_types::pre_asap::{CompareOpKind, ScalarValue};

        let comparison = || ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(0)),
            op: CompareOpKind::Eq,
            right: Box::new(ScalarExpr::Literal(ScalarValue::Int64(1))),
            semantics: ExprSemantics::Sql,
        };
        let predicate = ScalarExpr::BoolAnd(vec![comparison(), comparison()]);

        assert_eq!(scalar_operation_count(&predicate), Ok(3));
    }

    #[test]
    fn promql_presence_is_lowered_with_a_per_step_output_bound() {
        use asap_types::pre_asap::{AggIntent, DataType, Field, Reduction, Schema, Source};

        let source = Source::TimeSeries {
            metric: "missing".into(),
        };
        let scan = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
            source: source.clone(),
            predicates: vec![],
            schema: Schema::new(vec![Field::plain("value", DataType::Float64, false)]),
        }))
        .unwrap();
        let root =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: Reduction::PerEntity,
                measures: vec![AggIntent::Absent],
                output_names: vec![],
                filters: vec![],
                having: None,
                child: scan,
            }))
            .unwrap();
        let vector = promql_edge(0, 2, PromqlValueKind::Vector);
        let scan_statistics = OperatorStatistics::Scan {
            edges: promql_unary_edges(edge(0, 0), edge(0, 0), vector, vector),
            source_read_bytes: 0,
        };
        let output = promql_edge(1, 2, PromqlValueKind::Vector);
        let presence_statistics = OperatorStatistics::PromqlPresence {
            edges: promql_unary_edges(edge(0, 0), edge(2, 16), vector, output),
        };
        let provided = HashMap::from([
            ("query-1".into(), evidence(scan_statistics)),
            ("query-0".into(), evidence(presence_statistics.clone())),
        ]);
        let query_scope = scope(vec![coverage(source, vec![])]);
        let dag = lower_query_physical_dag(&root, &query_scope, &scripted(&provided)).unwrap();
        assert!(matches!(
            dag.nodes.last().map(|node| node.operator),
            Some(PhysicalOperator::PromqlPresence {
                kind: PromqlPresenceKind::Absent,
                operations_per_row: 1
            })
        ));

        let OperatorStatistics::PromqlPresence { mut edges } = presence_statistics else {
            unreachable!()
        };
        edges.output = edge(3, 24);
        let invalid = HashMap::from([
            (
                "query-1".into(),
                evidence(OperatorStatistics::Scan {
                    edges: promql_unary_edges(edge(0, 0), edge(0, 0), vector, vector),
                    source_read_bytes: 0,
                }),
            ),
            (
                "query-0".into(),
                evidence(OperatorStatistics::PromqlPresence { edges }),
            ),
        ]);
        assert!(matches!(
            lower_query_physical_dag(&root, &query_scope, &scripted(&invalid)),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));
    }

    #[test]
    fn promql_range_and_subquery_preserve_internal_steps() {
        use asap_types::pre_asap::{DataType, Field, Schema, Source};
        use std::time::Duration;

        let source = Source::TimeSeries { metric: "m".into() };
        let scan = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
            source: source.clone(),
            predicates: vec![],
            schema: Schema::new(vec![Field::plain("value", DataType::Float64, false)]),
        }))
        .unwrap();
        let range =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::TimeRange {
                range: Duration::from_secs(300),
                kind: TimeRangeKind::Range,
                child: scan,
            }))
            .unwrap();
        let root = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(
            NonASAPOp::PromqlSubquery {
                range: Duration::from_secs(300),
                resolution: Some(Duration::from_secs(60)),
                child: range,
            },
        ))
        .unwrap();
        let vector = promql_edge(10, 6, PromqlValueKind::Vector);
        let range_vector = promql_edge(10, 6, PromqlValueKind::RangeVector);
        let outer_range = promql_edge(10, 1, PromqlValueKind::RangeVector);
        let provided = HashMap::from([
            (
                "query-2".into(),
                evidence(OperatorStatistics::Scan {
                    edges: promql_unary_edges(edge(600, 9_600), edge(600, 9_600), vector, vector),
                    source_read_bytes: 4_800,
                }),
            ),
            (
                "query-1".into(),
                evidence(OperatorStatistics::PromqlRange {
                    edges: promql_unary_edges(
                        edge(600, 9_600),
                        edge(60, 960),
                        vector,
                        range_vector,
                    ),
                    max_window_samples_per_series: 10,
                }),
            ),
            (
                "query-0".into(),
                evidence(OperatorStatistics::PromqlSubquery {
                    edges: promql_unary_edges(
                        edge(60, 960),
                        edge(10, 160),
                        range_vector,
                        outer_range,
                    ),
                    subquery_steps: 6,
                }),
            ),
        ]);
        let query_scope = scope(vec![coverage(source, vec![])]);
        let dag = lower_query_physical_dag(&root, &query_scope, &scripted(&provided)).unwrap();
        assert!(matches!(
            dag.nodes[1].operator,
            PhysicalOperator::PromqlRange {
                range_millis: 300_000
            }
        ));
        assert!(matches!(
            dag.nodes[2].operator,
            PhysicalOperator::PromqlSubquery {
                range_millis: 300_000,
                resolution_millis: Some(60_000)
            }
        ));
        assert!(estimate_physical_dag(&dag.nodes, &dag.root, &query_scope, &dag.evidence).is_ok());
    }

    #[test]
    fn promql_binary_lowering_keeps_operation_and_matching_cardinality() {
        use asap_types::pre_asap::{
            ArithmeticOpKind, BinaryOpKind, DataType, Field, GroupSide, Schema, Source,
            VectorGrouping, VectorMatch, VectorMatchKind,
        };

        let left_source = Source::TimeSeries { metric: "a".into() };
        let right_source = Source::TimeSeries { metric: "b".into() };
        let scan = |source| {
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
                source,
                predicates: vec![],
                schema: Schema::new(vec![Field::plain("value", DataType::Float64, false)]),
            }))
            .unwrap()
        };
        let root =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::BinaryOp {
                operator: BinaryOperator {
                    kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
                    vector_match: Some(VectorMatch {
                        kind: VectorMatchKind::On,
                        labels: vec!["service".into()],
                        grouping: Some(VectorGrouping {
                            side: GroupSide::Left,
                            labels: vec!["region".into()],
                        }),
                    }),
                    checked_relative_division: false,
                    checked_finite_division: false,
                },
                return_bool: false,
                lhs: scan(left_source.clone()),
                rhs: scan(right_source.clone()),
            }))
            .unwrap();
        let left_promql = promql_edge(10, 10, PromqlValueKind::Vector);
        let right_promql = promql_edge(5, 10, PromqlValueKind::Vector);
        let output_promql = promql_edge(8, 10, PromqlValueKind::Vector);
        let left_edge = edge(100, 1_600);
        let right_edge = edge(50, 800);
        let output_edge = edge(80, 1_280);
        let provided = HashMap::from([
            (
                "query-1".into(),
                evidence(OperatorStatistics::Scan {
                    edges: promql_unary_edges(left_edge, left_edge, left_promql, left_promql),
                    source_read_bytes: 800,
                }),
            ),
            (
                "query-2".into(),
                evidence(OperatorStatistics::Scan {
                    edges: promql_unary_edges(right_edge, right_edge, right_promql, right_promql),
                    source_read_bytes: 400,
                }),
            ),
            (
                "query-0".into(),
                evidence(OperatorStatistics::PromqlBinary {
                    edges: BinaryEdgeStatistics {
                        inputs: [left_edge, right_edge],
                        output: output_edge,
                        promql: Some(
                            crate::physical_operator_statistics::PromqlBinaryEdgeStatistics {
                                inputs: [left_promql, right_promql],
                                output: output_promql,
                            },
                        ),
                    },
                    matching_key_bytes: 16,
                }),
            ),
        ]);
        let query_scope = scope(vec![
            coverage(left_source, vec![]),
            coverage(right_source, vec![]),
        ]);
        let dag = lower_query_physical_dag(&root, &query_scope, &scripted(&provided)).unwrap();
        assert!(matches!(
            dag.nodes.last().map(|node| node.operator),
            Some(PhysicalOperator::PromqlBinary {
                operation: PromqlBinaryOperation::ArithmeticOrComparison,
                operand_mode: PromqlBinaryOperandMode::VectorVector,
                cardinality: PromqlVectorCardinality::ManyToOne,
                build_side: Some(HashJoinBuildSide::Right),
            })
        ));
    }

    #[test]
    fn promql_relabel_sample_and_per_series_lower_as_a_complete_chain() {
        use asap_types::pre_asap::{
            AggIntent, DataType, Field, GroupKeys, Reduction, SampleKind, ScalarValue, Schema,
            Source,
        };

        let source = Source::TimeSeries {
            metric: "requests".into(),
        };
        let scan = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
            source: source.clone(),
            predicates: vec![],
            schema: Schema::new(vec![Field::plain("value", DataType::Float64, false)]),
        }))
        .unwrap();
        let relabel = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(
            NonASAPOp::PromqlRelabel {
                dst: "service".into(),
                value: ScalarExpr::Literal(ScalarValue::Utf8("api".into())),
                child: scan,
            },
        ))
        .unwrap();
        let sample = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(
            NonASAPOp::PromqlSeriesSample {
                by: GroupKeys::none(),
                kind: SampleKind::LimitK(5),
                child: relabel,
            },
        ))
        .unwrap();
        let root =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: Reduction::PerEntity,
                measures: vec![AggIntent::Sum { col: None }],
                output_names: vec![],
                filters: vec![],
                having: None,
                child: sample,
            }))
            .unwrap();

        let input = edge(100, 1_600);
        let sampled = edge(50, 800);
        let input_promql = promql_edge(10, 10, PromqlValueKind::Vector);
        let sampled_promql = promql_edge(5, 10, PromqlValueKind::Vector);
        let provided = HashMap::from([
            (
                "query-3".into(),
                evidence(OperatorStatistics::Scan {
                    edges: promql_unary_edges(input, input, input_promql, input_promql),
                    source_read_bytes: 800,
                }),
            ),
            (
                "query-2".into(),
                evidence(OperatorStatistics::PromqlRelabel {
                    edges: promql_unary_edges(input, input, input_promql, input_promql),
                }),
            ),
            (
                "query-1".into(),
                evidence(OperatorStatistics::PromqlSeriesSample {
                    edges: promql_unary_edges(input, sampled, input_promql, sampled_promql),
                    group_count: 1,
                    key_bytes: 8,
                }),
            ),
            (
                "query-0".into(),
                evidence(OperatorStatistics::PromqlPerSeries {
                    edges: promql_unary_edges(sampled, sampled, sampled_promql, sampled_promql),
                    accumulator_bytes_per_series: 8,
                }),
            ),
        ]);
        let query_scope = scope(vec![coverage(source, vec![])]);

        let dag = lower_query_physical_dag(&root, &query_scope, &scripted(&provided)).unwrap();
        assert!(matches!(
            dag.nodes.as_slice(),
            [
                PhysicalDAGNode {
                    operator: PhysicalOperator::Scan,
                    ..
                },
                PhysicalDAGNode {
                    operator: PhysicalOperator::PromqlRelabel { .. },
                    ..
                },
                PhysicalDAGNode {
                    operator: PhysicalOperator::PromqlSeriesSample { .. },
                    ..
                },
                PhysicalDAGNode {
                    operator: PhysicalOperator::PromqlPerSeries { .. },
                    ..
                }
            ]
        ));
        assert!(estimate_physical_dag(&dag.nodes, &dag.root, &query_scope, &dag.evidence).is_ok());
    }
}
