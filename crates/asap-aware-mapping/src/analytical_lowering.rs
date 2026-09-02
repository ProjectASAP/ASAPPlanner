//! Recursive lowering from the canonical query IR to analytical physical DAGs.

use std::rc::Rc;

use serde::{Deserialize, Serialize};

use crate::analytical_cost::{
    AnalyticalCostError, ExecutionMultiplicity, OperatorInputs, PhysicalDagNode, PhysicalOperator,
};

/// A lowered physical DAG and the node whose output is the query result.
/// Keeping the root beside its nodes prevents callers from accidentally
/// estimating a valid node list from the wrong entry point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhysicalDag {
    pub nodes: Vec<PhysicalDagNode>,
    pub root: String,
}

/// Lower a resolved query operator DAG to the physical operators understood by
/// this cost model. The callback supplies physical statistics for each logical
/// operator identity; returning `None` makes the complete query unavailable.
/// Scalar expressions remain part of their containing operator's local cost.
pub fn lower_query_physical_dag<F>(
    root: &Rc<asap_types::pre_asap::QueryExpr>,
    mut statistics: F,
) -> Result<PhysicalDag, AnalyticalCostError>
where
    F: FnMut(&asap_types::pre_asap::QueryExpr) -> Option<OperatorInputs>,
{
    use std::collections::HashMap;

    use asap_types::pre_asap::{GroupKeys, QueryExpr, SetOpKind};

    struct Lowerer<'a, F> {
        statistics: &'a mut F,
        logical_roots: HashMap<usize, String>,
        next_id: usize,
        nodes: Vec<PhysicalDagNode>,
    }

    impl<F> Lowerer<'_, F>
    where
        F: FnMut(&QueryExpr) -> Option<OperatorInputs>,
    {
        fn lower(&mut self, query: &QueryExpr) -> Result<String, AnalyticalCostError> {
            let identity = std::ptr::from_ref(query) as usize;
            if let Some(id) = self.logical_roots.get(&identity) {
                return Ok(id.clone());
            }
            let id = format!("query-{}", self.next_id);
            self.next_id += 1;
            // Insert only after successful lowering: a malformed recursive
            // shape cannot leave a partially reusable node behind.
            let root = self.lower_new(query, id)?;
            self.logical_roots.insert(identity, root.clone());
            Ok(root)
        }

        fn stats(&mut self, query: &QueryExpr) -> Result<OperatorInputs, AnalyticalCostError> {
            (self.statistics)(query)
                .ok_or(AnalyticalCostError::MissingOrStale("operator_statistics"))
        }

        fn push(
            &mut self,
            id: String,
            operator: PhysicalOperator,
            inputs: OperatorInputs,
            children: Vec<String>,
        ) -> String {
            let output_buffer_bytes = inputs.output_bytes;
            self.nodes.push(PhysicalDagNode {
                id: id.clone(),
                operator,
                inputs,
                children,
                output_buffer_bytes,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            });
            id
        }

        fn lower_unary(
            &mut self,
            query: &QueryExpr,
            id: String,
            operator: PhysicalOperator,
            child: &QueryExpr,
        ) -> Result<String, AnalyticalCostError> {
            let child_id = self.lower(child)?;
            let inputs = self.stats(query)?;
            let child_inputs = &self.node(&child_id)?.inputs;
            require_unary_edge(inputs, *child_inputs)?;
            require_operator_statistics(operator, inputs)?;
            Ok(self.push(id, operator, inputs, vec![child_id]))
        }

        fn node(&self, id: &str) -> Result<&PhysicalDagNode, AnalyticalCostError> {
            self.nodes.iter().find(|node| node.id == id).ok_or(
                AnalyticalCostError::InvalidPhysicalDag("lowered child is missing"),
            )
        }

        fn lower_new(
            &mut self,
            query: &QueryExpr,
            id: String,
        ) -> Result<String, AnalyticalCostError> {
            match query {
                QueryExpr::Scan { predicates, .. } => {
                    let inputs = self.stats(query)?;
                    require_consistent_edge_statistics(inputs)?;
                    if predicates.is_empty() {
                        if inputs.input_rows != inputs.output_rows
                            || inputs.input_bytes != inputs.output_bytes
                        {
                            return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                                "unfiltered scan output does not match its input",
                            ));
                        }
                        return Ok(self.push(id, PhysicalOperator::Scan, inputs, vec![]));
                    }
                    if inputs.output_rows > inputs.input_rows
                        || inputs.output_bytes > inputs.input_bytes
                    {
                        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                            "predicate-bearing scan expands its input",
                        ));
                    }
                    let scan_id = format!("{id}-scan");
                    let mut scan_inputs = inputs;
                    scan_inputs.output_rows = inputs.input_rows;
                    scan_inputs.output_bytes = inputs.input_bytes;
                    clear_operator_specific_inputs(&mut scan_inputs);
                    self.push(scan_id.clone(), PhysicalOperator::Scan, scan_inputs, vec![]);
                    require_operator_statistics(PhysicalOperator::Filter, inputs)?;
                    Ok(self.push(id, PhysicalOperator::Filter, inputs, vec![scan_id]))
                }
                QueryExpr::Filter { child, .. } => {
                    self.lower_unary(query, id, PhysicalOperator::Filter, child)
                }
                QueryExpr::Project { child, .. } => {
                    self.lower_unary(query, id, PhysicalOperator::Project, child)
                }
                QueryExpr::Aggregate { child, .. } => {
                    self.lower_unary(query, id, PhysicalOperator::HashAggregate, child)
                }
                QueryExpr::Dedup { child, .. } => {
                    self.lower_unary(query, id, PhysicalOperator::Deduplicate, child)
                }
                QueryExpr::Sort { child, .. } => {
                    self.lower_unary(query, id, PhysicalOperator::Sort, child)
                }
                QueryExpr::Limit { n, offset, child } => {
                    if let QueryExpr::Sort {
                        partition_by,
                        child: sorted_child,
                        ..
                    } = child.as_ref()
                    {
                        if partition_by == &GroupKeys::none() {
                            let child_id = self.lower(sorted_child)?;
                            let mut inputs = self.stats(query)?;
                            require_unary_edge(inputs, self.node(&child_id)?.inputs)?;
                            let bound = n
                                .checked_add(*offset)
                                .and_then(|value| u64::try_from(value).ok())
                                .ok_or(AnalyticalCostError::Overflow)?;
                            if bound == 0 {
                                return Err(AnalyticalCostError::MissingOrZero("topk_k"));
                            }
                            inputs.k = Some(bound);
                            require_limit_cardinality(*n, *offset, inputs)?;
                            return Ok(self.push(
                                id,
                                PhysicalOperator::TopK,
                                inputs,
                                vec![child_id],
                            ));
                        }
                    }
                    let child_id = self.lower(child)?;
                    let inputs = self.stats(query)?;
                    require_unary_edge(inputs, self.node(&child_id)?.inputs)?;
                    require_operator_statistics(PhysicalOperator::Limit, inputs)?;
                    require_limit_cardinality(*n, *offset, inputs)?;
                    Ok(self.push(id, PhysicalOperator::Limit, inputs, vec![child_id]))
                }
                QueryExpr::SQLWindowFunc { child, .. } => {
                    self.lower_unary(query, id, PhysicalOperator::Window, child)
                }
                QueryExpr::TimeShift { child, .. } => {
                    self.lower_unary(query, id, PhysicalOperator::PassThrough, child)
                }
                QueryExpr::Concat { children } => {
                    let child_ids = children
                        .iter()
                        .map(|child| self.lower(child))
                        .collect::<Result<Vec<_>, _>>()?;
                    self.lower_concat(query, id, child_ids)
                }
                QueryExpr::SetOp {
                    kind: SetOpKind::Union,
                    all: true,
                    left,
                    right,
                } => {
                    let left_id = self.lower(left)?;
                    let right_id = self.lower(right)?;
                    self.lower_concat(query, id, vec![left_id, right_id])
                }
                QueryExpr::Join {
                    kind,
                    pred,
                    left,
                    right,
                } => {
                    if matches!(kind, asap_types::pre_asap::JoinKind::Cross)
                        || !is_hash_join_predicate(&pred.0)
                    {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    let left_id = self.lower(left)?;
                    let right_id = self.lower(right)?;
                    let inputs = self.stats(query)?;
                    require_consistent_edge_statistics(inputs)?;
                    let left_inputs = self.node(&left_id)?.inputs;
                    let right_inputs = self.node(&right_id)?.inputs;
                    if inputs.input_rows != left_inputs.output_rows
                        || inputs.input_bytes != left_inputs.output_bytes
                        || inputs.right_rows != Some(right_inputs.output_rows)
                        || inputs.right_bytes != Some(right_inputs.output_bytes)
                    {
                        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                            "join inputs do not match child outputs",
                        ));
                    }
                    Ok(self.push(
                        id,
                        PhysicalOperator::HashJoin,
                        inputs,
                        vec![left_id, right_id],
                    ))
                }
                _ => Err(AnalyticalCostError::UnsupportedQueryOperator),
            }
        }

        fn lower_concat(
            &mut self,
            query: &QueryExpr,
            id: String,
            child_ids: Vec<String>,
        ) -> Result<String, AnalyticalCostError> {
            if child_ids.is_empty() {
                return Err(AnalyticalCostError::InvalidPhysicalDag(
                    "concat has no children",
                ));
            }
            let inputs = self.stats(query)?;
            require_consistent_edge_statistics(inputs)?;
            let (rows, bytes) =
                child_ids
                    .iter()
                    .try_fold((0_u64, 0_u64), |(rows, bytes), child| {
                        let child = self.node(child)?;
                        Ok::<_, AnalyticalCostError>((
                            rows.checked_add(child.inputs.output_rows)
                                .ok_or(AnalyticalCostError::Overflow)?,
                            bytes
                                .checked_add(child.inputs.output_bytes)
                                .ok_or(AnalyticalCostError::Overflow)?,
                        ))
                    })?;
            if inputs.input_rows != rows
                || inputs.input_bytes != bytes
                || inputs.output_rows != rows
                || inputs.output_bytes != bytes
            {
                return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                    "concat statistics do not equal the sum of child outputs",
                ));
            }
            Ok(self.push(id, PhysicalOperator::Concat, inputs, child_ids))
        }
    }

    let mut lowerer = Lowerer {
        statistics: &mut statistics,
        logical_roots: HashMap::new(),
        next_id: 0,
        nodes: Vec::new(),
    };
    let root = lowerer.lower(root)?;
    Ok(PhysicalDag {
        nodes: lowerer.nodes,
        root,
    })
}

fn require_consistent_edge_statistics(inputs: OperatorInputs) -> Result<(), AnalyticalCostError> {
    require_cardinality_width("operator input", inputs.input_rows, inputs.input_bytes)?;
    require_cardinality_width("operator output", inputs.output_rows, inputs.output_bytes)?;
    Ok(())
}

fn require_cardinality_width(
    edge: &'static str,
    rows: u64,
    bytes: u64,
) -> Result<(), AnalyticalCostError> {
    match (rows, bytes) {
        (0, 0) => Ok(()),
        (0, _) => Err(AnalyticalCostError::InconsistentOperatorStatistics(edge)),
        (_, 0) => Err(AnalyticalCostError::MissingOrZero(edge)),
        _ => Ok(()),
    }
}

fn require_unary_edge(
    inputs: OperatorInputs,
    child: OperatorInputs,
) -> Result<(), AnalyticalCostError> {
    require_consistent_edge_statistics(inputs)?;
    if inputs.input_rows != child.output_rows || inputs.input_bytes != child.output_bytes {
        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "unary input does not match child output",
        ));
    }
    Ok(())
}

fn require_operator_statistics(
    operator: PhysicalOperator,
    inputs: OperatorInputs,
) -> Result<(), AnalyticalCostError> {
    let invalid = |reason| Err(AnalyticalCostError::InconsistentOperatorStatistics(reason));
    match operator {
        PhysicalOperator::Filter => {
            if inputs.output_rows > inputs.input_rows || inputs.output_bytes > inputs.input_bytes {
                return invalid("filter output expands its input");
            }
        }
        PhysicalOperator::Project => {
            if inputs.output_rows != inputs.input_rows {
                return invalid("projection changes row cardinality");
            }
        }
        PhysicalOperator::HashAggregate => {
            let groups = inputs
                .group_count
                .ok_or(AnalyticalCostError::MissingOrZero("group_count"))?;
            if groups == 0 && (inputs.input_rows != 0 || inputs.output_rows != 0) {
                return invalid("zero groups require an empty grouped input and output");
            }
            if inputs.output_rows > groups {
                return invalid("aggregate output exceeds group cardinality");
            }
        }
        PhysicalOperator::Deduplicate => {
            let groups = inputs
                .group_count
                .ok_or(AnalyticalCostError::MissingOrZero("group_count"))?;
            if inputs.output_rows != groups || inputs.output_rows > inputs.input_rows {
                return invalid("deduplicate output does not equal distinct cardinality");
            }
        }
        PhysicalOperator::Sort => {
            if inputs.output_rows != inputs.input_rows || inputs.output_bytes != inputs.input_bytes
            {
                return invalid("sort changes its input cardinality or width");
            }
        }
        PhysicalOperator::TopK | PhysicalOperator::Limit => {
            if inputs.output_rows > inputs.input_rows {
                return invalid("bounded output exceeds its input cardinality");
            }
        }
        PhysicalOperator::Window => {
            if inputs.output_rows != inputs.input_rows {
                return invalid("SQL window changes row cardinality");
            }
        }
        PhysicalOperator::PassThrough => {
            if inputs.output_rows != inputs.input_rows || inputs.output_bytes != inputs.input_bytes
            {
                return invalid("pass-through wrapper changes its edge statistics");
            }
        }
        PhysicalOperator::Scan | PhysicalOperator::HashJoin | PhysicalOperator::Concat => {}
    }
    Ok(())
}

fn require_limit_cardinality(
    n: usize,
    offset: usize,
    inputs: OperatorInputs,
) -> Result<(), AnalyticalCostError> {
    let n = u64::try_from(n).map_err(|_| AnalyticalCostError::Overflow)?;
    let offset = u64::try_from(offset).map_err(|_| AnalyticalCostError::Overflow)?;
    let expected = inputs.input_rows.saturating_sub(offset).min(n);
    if inputs.output_rows != expected {
        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "limit output does not match n and offset",
        ));
    }
    Ok(())
}

fn clear_operator_specific_inputs(inputs: &mut OperatorInputs) {
    inputs.group_count = None;
    inputs.key_bytes = None;
    inputs.aggregate_value_bytes = None;
    inputs.k = None;
    inputs.right_rows = None;
    inputs.right_bytes = None;
    inputs.hash_join_build_side = None;
}

fn is_hash_join_predicate(expr: &asap_types::pre_asap::QueryExpr) -> bool {
    use asap_types::pre_asap::{CompareOpKind, QueryExpr};

    match expr {
        QueryExpr::Compare {
            left,
            op: CompareOpKind::Eq,
            right,
        } => {
            matches!(left.as_ref(), QueryExpr::Column(_))
                && matches!(right.as_ref(), QueryExpr::Column(_))
        }
        QueryExpr::BoolAnd(parts) => !parts.is_empty() && parts.iter().all(is_hash_join_predicate),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytical_cost::{estimate_physical_dag, HashJoinBuildSide};

    fn operator_inputs(
        input_rows: u64,
        input_bytes: u64,
        output_rows: u64,
        output_bytes: u64,
    ) -> OperatorInputs {
        OperatorInputs {
            input_rows,
            input_bytes,
            output_rows,
            output_bytes,
            group_count: None,
            key_bytes: None,
            aggregate_value_bytes: None,
            k: None,
            right_rows: None,
            right_bytes: None,
            hash_join_build_side: None,
        }
    }

    #[test]
    fn query_lowering_recurses_and_fuses_global_sort_limit() {
        use asap_types::pre_asap::{
            agg_intent::default_cardinality, GroupKeys, QueryExpr, Reduction, Source,
        };
        use asap_types::pre_asap::{Column, DataType, Schema};
        use std::rc::Rc;

        let scan = Rc::new(QueryExpr::Scan {
            source: Source::Table {
                table_ref: "events".into(),
            },
            predicates: vec![asap_types::pre_asap::Predicate(Rc::new(
                QueryExpr::Literal(asap_types::pre_asap::ScalarValue::Boolean(true)),
            ))],
            schema: Schema::new(vec![
                Column::new("service", DataType::Utf8, false),
                Column::new("value", DataType::Float64, false),
            ]),
        });
        let aggregate = Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::by(vec![0]),
            measures: vec![default_cardinality()],
            output_names: vec![],
            having: None,
            child: Rc::clone(&scan),
        });
        let sort = Rc::new(QueryExpr::Sort {
            keys: vec![],
            partition_by: GroupKeys::none(),
            child: aggregate,
        });
        let root = Rc::new(QueryExpr::Limit {
            n: 10,
            offset: 5,
            child: sort,
        });

        let dag = lower_query_physical_dag(&root, |node| {
            let mut stats = match node {
                QueryExpr::Scan { .. } => operator_inputs(1_000, 64_000, 400, 25_600),
                QueryExpr::Aggregate { .. } => operator_inputs(400, 25_600, 100, 4_000),
                QueryExpr::Limit { .. } => operator_inputs(100, 4_000, 10, 400),
                _ => return None,
            };
            if matches!(node, QueryExpr::Aggregate { .. }) {
                stats.group_count = Some(100);
                stats.key_bytes = Some(16);
                stats.aggregate_value_bytes = Some(8);
            }
            Some(stats)
        })
        .unwrap();

        assert_eq!(
            dag.nodes
                .iter()
                .map(|node| node.operator)
                .collect::<Vec<_>>(),
            vec![
                PhysicalOperator::Scan,
                PhysicalOperator::Filter,
                PhysicalOperator::HashAggregate,
                PhysicalOperator::TopK,
            ]
        );
        let topk = dag.nodes.last().unwrap();
        assert_eq!(topk.inputs.k, Some(15));
        assert_eq!(topk.children, vec![dag.nodes[2].id.clone()]);
        assert!(estimate_physical_dag(&dag.nodes, &dag.root, 3).is_ok());
    }

    #[test]
    fn query_lowering_deduplicates_shared_rc_children() {
        use asap_types::pre_asap::{Column, CompareOpKind, DataType, Schema};
        use asap_types::pre_asap::{JoinKind, Predicate, QueryExpr, Source};
        use std::rc::Rc;

        let shared = Rc::new(QueryExpr::Scan {
            source: Source::Table {
                table_ref: "dimensions".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Column::new("id", DataType::Int64, false)]),
        });
        let root = Rc::new(QueryExpr::Join {
            kind: JoinKind::Inner,
            pred: Predicate(Rc::new(QueryExpr::Compare {
                left: Rc::new(QueryExpr::Column(0)),
                op: CompareOpKind::Eq,
                right: Rc::new(QueryExpr::Column(1)),
            })),
            left: Rc::clone(&shared),
            right: Rc::clone(&shared),
        });
        let dag = lower_query_physical_dag(&root, |node| match node {
            QueryExpr::Scan { .. } => Some(operator_inputs(100, 800, 100, 800)),
            QueryExpr::Join { .. } => {
                let mut stats = operator_inputs(100, 800, 25, 400);
                stats.right_rows = Some(100);
                stats.right_bytes = Some(800);
                stats.hash_join_build_side = Some(HashJoinBuildSide::Right);
                Some(stats)
            }
            _ => None,
        })
        .unwrap();

        assert_eq!(dag.nodes.len(), 2);
        assert_eq!(dag.nodes[1].children, vec![dag.nodes[0].id.clone(); 2]);
        let estimate = estimate_physical_dag(&dag.nodes, &dag.root, 1).unwrap();
        assert_eq!(estimate.scan_bytes, 800);
    }

    #[test]
    fn query_lowering_covers_relational_unary_operators() {
        use asap_types::pre_asap::{Column, DataType, ScalarValue, Schema};
        use asap_types::pre_asap::{
            GroupKeys, Predicate, QueryExpr, Source, TimeShift, WindowFuncKind,
        };
        use std::rc::Rc;

        let scan = Rc::new(QueryExpr::Scan {
            source: Source::Table {
                table_ref: "events".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Column::new("id", DataType::Int64, false)]),
        });
        let filter = Rc::new(QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Boolean(true)))),
            child: scan,
        });
        let project = Rc::new(QueryExpr::Project {
            cols: vec![],
            qualifier: None,
            child: filter,
        });
        let dedup = Rc::new(QueryExpr::Dedup {
            cols: vec![0],
            child: project,
        });
        let window = Rc::new(QueryExpr::SQLWindowFunc {
            func: WindowFuncKind::RowNumber,
            args: vec![],
            partition_by: GroupKeys::none(),
            order_by: vec![],
            frame: None,
            output_name: "rn".into(),
            child: dedup,
        });
        let sort = Rc::new(QueryExpr::Sort {
            keys: vec![],
            partition_by: GroupKeys::by(vec![0]),
            child: window,
        });
        let limit = Rc::new(QueryExpr::Limit {
            n: 20,
            offset: 0,
            child: sort,
        });
        let root = Rc::new(QueryExpr::TimeShift {
            shift: TimeShift::default(),
            child: limit,
        });

        let dag = lower_query_physical_dag(&root, |node| {
            let mut inputs = match node {
                QueryExpr::Scan { .. } => operator_inputs(1_000, 8_000, 1_000, 8_000),
                QueryExpr::Filter { .. } => operator_inputs(1_000, 8_000, 800, 6_400),
                QueryExpr::Project { .. } => operator_inputs(800, 6_400, 800, 3_200),
                QueryExpr::Dedup { .. } => operator_inputs(800, 3_200, 500, 2_000),
                QueryExpr::SQLWindowFunc { .. } => operator_inputs(500, 2_000, 500, 6_000),
                QueryExpr::Sort { .. } => operator_inputs(500, 6_000, 500, 6_000),
                QueryExpr::Limit { .. } => operator_inputs(500, 6_000, 20, 240),
                QueryExpr::TimeShift { .. } => operator_inputs(20, 240, 20, 240),
                _ => return None,
            };
            if matches!(node, QueryExpr::Dedup { .. }) {
                inputs.group_count = Some(500);
                inputs.key_bytes = Some(8);
            }
            Some(inputs)
        })
        .unwrap();

        assert_eq!(
            dag.nodes
                .iter()
                .map(|node| node.operator)
                .collect::<Vec<_>>(),
            vec![
                PhysicalOperator::Scan,
                PhysicalOperator::Filter,
                PhysicalOperator::Project,
                PhysicalOperator::Deduplicate,
                PhysicalOperator::Window,
                PhysicalOperator::Sort,
                PhysicalOperator::Limit,
                PhysicalOperator::PassThrough,
            ]
        );
        assert!(estimate_physical_dag(&dag.nodes, &dag.root, 2).is_ok());
    }

    #[test]
    fn query_lowering_maps_concat_and_union_all_but_rejects_distinct_set_ops() {
        use asap_types::pre_asap::{Column, DataType, Schema};
        use asap_types::pre_asap::{QueryExpr, SetOpKind, Source};
        use std::rc::Rc;

        let scan = |name: &str| QueryExpr::Scan {
            source: Source::Table {
                table_ref: name.into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Column::new("id", DataType::Int64, false)]),
        };
        let union = Rc::new(QueryExpr::SetOp {
            kind: SetOpKind::Union,
            all: true,
            left: Rc::new(scan("a")),
            right: Rc::new(scan("b")),
        });
        let dag = lower_query_physical_dag(&union, |node| match node {
            QueryExpr::Scan {
                source: Source::Table { table_ref },
                ..
            } if table_ref == "a" => Some(operator_inputs(10, 80, 10, 80)),
            QueryExpr::Scan { .. } => Some(operator_inputs(20, 160, 20, 160)),
            QueryExpr::SetOp { .. } => Some(operator_inputs(30, 240, 30, 240)),
            _ => None,
        })
        .unwrap();
        assert_eq!(dag.nodes.last().unwrap().operator, PhysicalOperator::Concat);

        let concat = Rc::new(QueryExpr::Concat {
            children: vec![scan("a"), scan("b")],
        });
        let dag = lower_query_physical_dag(&concat, |node| match node {
            QueryExpr::Scan {
                source: Source::Table { table_ref },
                ..
            } if table_ref == "a" => Some(operator_inputs(10, 80, 10, 80)),
            QueryExpr::Scan { .. } => Some(operator_inputs(20, 160, 20, 160)),
            QueryExpr::Concat { .. } => Some(operator_inputs(30, 240, 30, 240)),
            _ => None,
        })
        .unwrap();
        assert_eq!(dag.nodes.last().unwrap().operator, PhysicalOperator::Concat);

        let distinct_union = Rc::new(QueryExpr::SetOp {
            kind: SetOpKind::Union,
            all: false,
            left: Rc::new(scan("a")),
            right: Rc::new(scan("b")),
        });
        assert_eq!(
            lower_query_physical_dag(&distinct_union, |_| None),
            Err(AnalyticalCostError::UnsupportedQueryOperator)
        );
    }

    #[test]
    fn query_lowering_fails_closed_for_missing_or_inconsistent_statistics() {
        use asap_types::pre_asap::{Column, DataType, Schema};
        use asap_types::pre_asap::{QueryExpr, Source};
        use std::rc::Rc;

        let scan = Rc::new(QueryExpr::Scan {
            source: Source::Table {
                table_ref: "events".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Column::new("id", DataType::Int64, false)]),
        });
        let root = Rc::new(QueryExpr::Project {
            cols: vec![],
            qualifier: None,
            child: scan,
        });

        assert_eq!(
            lower_query_physical_dag(&root, |_| None),
            Err(AnalyticalCostError::MissingOrStale("operator_statistics"))
        );
        assert_eq!(
            lower_query_physical_dag(&root, |node| match node {
                QueryExpr::Scan { .. } => Some(operator_inputs(100, 800, 100, 800)),
                QueryExpr::Project { .. } => Some(operator_inputs(99, 792, 99, 396)),
                _ => None,
            }),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "unary input does not match child output"
            ))
        );
    }

    #[test]
    fn query_lowering_accepts_a_consistently_empty_edge() {
        use asap_types::pre_asap::{Column, DataType, ScalarValue, Schema};
        use asap_types::pre_asap::{Predicate, QueryExpr, Source};
        use std::rc::Rc;

        let scan = Rc::new(QueryExpr::Scan {
            source: Source::Table {
                table_ref: "events".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Column::new("id", DataType::Int64, false)]),
        });
        let filter = Rc::new(QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Boolean(false)))),
            child: scan,
        });
        let root = Rc::new(QueryExpr::Limit {
            n: 10,
            offset: 0,
            child: filter,
        });

        let dag = lower_query_physical_dag(&root, |node| match node {
            QueryExpr::Scan { .. } => Some(operator_inputs(100, 800, 100, 800)),
            QueryExpr::Filter { .. } => Some(operator_inputs(100, 800, 0, 0)),
            QueryExpr::Limit { .. } => Some(operator_inputs(0, 0, 0, 0)),
            _ => None,
        })
        .unwrap();
        let estimate = estimate_physical_dag(&dag.nodes, &dag.root, 1).unwrap();
        assert_eq!(estimate.cpu_ops, 200.0);
        assert_eq!(estimate.scan_bytes, 800);
    }
}
