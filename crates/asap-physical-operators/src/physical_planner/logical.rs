//! Reconstruct shared operator references from the transport DAG for native lowering.
use super::*;
use planner_types::ir::export::{EdgeRole, NonASAPOpKind as N, PostAsapNodeId, WireScalarExpr};
use planner_types::ir::{
    ASAPOp, NonASAPOp, Operator as LogicalOperator, OperatorNode, Predicate, ProjectItem,
    ScalarExpr, SortKey as LogicalSortKey,
};
use std::rc::Rc;
pub(super) fn scalar(
    expr: &WireScalarExpr,
    id_of: &mut impl FnMut(PostAsapNodeId) -> Rc<OperatorNode>,
) -> ScalarExpr {
    fn boxed(
        e: &WireScalarExpr,
        id_of: &mut impl FnMut(PostAsapNodeId) -> Rc<OperatorNode>,
    ) -> Box<ScalarExpr> {
        Box::new(scalar(e, id_of))
    }
    fn list(
        es: &[WireScalarExpr],
        id_of: &mut impl FnMut(PostAsapNodeId) -> Rc<OperatorNode>,
    ) -> Vec<ScalarExpr> {
        es.iter().map(|e| scalar(e, id_of)).collect()
    }
    match expr {
        WireScalarExpr::Column(id) => ScalarExpr::Column(*id),
        WireScalarExpr::Literal(v) => ScalarExpr::Literal(v.clone()),
        WireScalarExpr::Negative { expr, semantics } => ScalarExpr::Negative {
            expr: boxed(expr, id_of),
            semantics: *semantics,
        },
        WireScalarExpr::Compare {
            left,
            op,
            right,
            semantics,
        } => ScalarExpr::Compare {
            left: boxed(left, id_of),
            op: op.clone(),
            right: boxed(right, id_of),
            semantics: *semantics,
        },
        WireScalarExpr::BoolAnd(parts) => ScalarExpr::BoolAnd(list(parts, id_of)),
        WireScalarExpr::BoolOr(parts) => ScalarExpr::BoolOr(list(parts, id_of)),
        WireScalarExpr::Not(e) => ScalarExpr::Not(boxed(e, id_of)),
        WireScalarExpr::IsNull(e) => ScalarExpr::IsNull(boxed(e, id_of)),
        WireScalarExpr::IsNotNull(e) => ScalarExpr::IsNotNull(boxed(e, id_of)),
        WireScalarExpr::Cast { expr, to, try_cast } => ScalarExpr::Cast {
            expr: boxed(expr, id_of),
            to: to.clone(),
            try_cast: *try_cast,
        },
        WireScalarExpr::InList {
            expr,
            list: items,
            negated,
        } => ScalarExpr::InList {
            expr: boxed(expr, id_of),
            list: list(items, id_of),
            negated: *negated,
        },
        WireScalarExpr::FunctionCall { name, args } => ScalarExpr::FunctionCall {
            name: name.clone(),
            args: list(args, id_of),
        },
        WireScalarExpr::Arithmetic {
            op,
            left,
            right,
            semantics,
        } => ScalarExpr::Arithmetic {
            op: op.clone(),
            left: boxed(left, id_of),
            right: boxed(right, id_of),
            semantics: *semantics,
        },
        WireScalarExpr::Case {
            operand,
            branches,
            else_expr,
        } => ScalarExpr::Case {
            operand: operand.as_ref().map(|e| boxed(e, id_of)),
            branches: branches
                .iter()
                .map(|(w, t)| (scalar(w, id_of), scalar(t, id_of)))
                .collect(),
            else_expr: else_expr.as_ref().map(|e| boxed(e, id_of)),
        },
        WireScalarExpr::CurrentTimestamp => ScalarExpr::CurrentTimestamp,
        WireScalarExpr::EvalTimestamp => ScalarExpr::EvalTimestamp,
        WireScalarExpr::PromqlScalarFromVector(node) => {
            ScalarExpr::PromqlScalarFromVector(id_of(*node))
        }
        WireScalarExpr::ScalarSubquery(node) => ScalarExpr::ScalarSubquery(id_of(*node)),
        WireScalarExpr::Exists { subquery, negated } => ScalarExpr::Exists {
            subquery: id_of(*subquery),
            negated: *negated,
        },
        WireScalarExpr::InSubquery {
            expr,
            subquery,
            negated,
        } => ScalarExpr::InSubquery {
            expr: boxed(expr, id_of),
            subquery: id_of(*subquery),
            negated: *negated,
        },
    }
}

pub(super) fn restore(dag: &PostAsapDAG) -> Result<BTreeMap<NodeId, Rc<OperatorNode>>, Error> {
    dag.validate().map_err(|e| invalid(e.to_string()))?;
    let mut done = BTreeMap::new();
    let mut remaining: Vec<_> = dag.nodes.iter().collect();
    while !remaining.is_empty() {
        let before = remaining.len();
        let mut next = Vec::new();
        for node in remaining {
            let mut edges: Vec<_> = dag.edges.iter().filter(|e| e.consumer == node.id).collect();
            if edges
                .iter()
                .any(|e| !done.contains_key(&u64::from(e.producer.0)))
            {
                next.push(node);
                continue;
            }
            edges.sort_by_key(|e| match e.role {
                EdgeRole::Left => 0,
                EdgeRole::Input => 1,
                EdgeRole::Right => 2,
                EdgeRole::ScalarRef => 3,
            });
            let inputs: Vec<_> = edges
                .iter()
                .filter(|e| e.role != EdgeRole::ScalarRef)
                .map(|e| Rc::clone(&done[&u64::from(e.producer.0)]))
                .collect();
            let input = |index: usize| {
                inputs
                    .get(index)
                    .cloned()
                    .ok_or_else(|| invalid("operator is missing an input"))
            };
            let mut missing = false;
            let mut ref_node = |id: PostAsapNodeId| {
                if let Some(node) = done.get(&u64::from(id.0)) {
                    Rc::clone(node)
                } else {
                    missing = true;
                    Rc::new(OperatorNode::with_schema(
                        LogicalOperator::NonASAP(NonASAPOp::Values {
                            rows: vec![],
                            schema: Default::default(),
                        }),
                        Default::default(),
                    ))
                }
            };
            let mut value = |expr: &WireScalarExpr| scalar(expr, &mut ref_node);
            let operator = match &node.payload {
                Payload::Relational { operator } => LogicalOperator::NonASAP(match operator {
                    N::Scan {
                        source,
                        predicates,
                        schema,
                    } => NonASAPOp::Scan {
                        source: source.clone(),
                        predicates: predicates.iter().map(|p| Predicate(value(&p.0))).collect(),
                        schema: schema.clone(),
                    },
                    N::Values { rows, schema } => NonASAPOp::Values {
                        rows: rows
                            .iter()
                            .map(|r| r.iter().map(&mut value).collect())
                            .collect(),
                        schema: schema.clone(),
                    },
                    N::Filter { pred } => NonASAPOp::Filter {
                        pred: Predicate(value(&pred.0)),
                        child: input(0)?,
                    },
                    N::Project { cols, qualifier } => NonASAPOp::Project {
                        cols: cols
                            .iter()
                            .map(|c| ProjectItem {
                                alias: c.alias.clone(),
                                expr: value(&c.expr),
                            })
                            .collect(),
                        qualifier: qualifier.clone(),
                        child: input(0)?,
                    },
                    N::Aggregate {
                        reduction,
                        measures,
                        output_names,
                        filters,
                        having,
                    } => NonASAPOp::Aggregate {
                        reduction: reduction.clone(),
                        measures: measures.clone(),
                        output_names: output_names.clone(),
                        filters: filters
                            .iter()
                            .map(|p| p.as_ref().map(|p| Predicate(value(&p.0))))
                            .collect(),
                        having: having.as_ref().map(|p| Predicate(value(&p.0))),
                        child: input(0)?,
                    },
                    N::Join { join_kind, pred } => NonASAPOp::Join {
                        kind: join_kind.clone(),
                        pred: Predicate(value(&pred.0)),
                        left: input(0)?,
                        right: input(1)?,
                    },
                    N::SetOp { set_kind, all } => NonASAPOp::SetOp {
                        kind: set_kind.clone(),
                        all: *all,
                        left: input(0)?,
                        right: input(1)?,
                    },
                    N::Concat {
                        discriminator_unique_key,
                    } => NonASAPOp::Concat {
                        children: inputs.clone(),
                        discriminator_unique_key: discriminator_unique_key.clone(),
                    },
                    N::Dedup { cols } => NonASAPOp::Dedup {
                        cols: cols.clone(),
                        child: input(0)?,
                    },
                    N::Sort { keys, partition_by } => NonASAPOp::Sort {
                        keys: keys
                            .iter()
                            .map(|k| LogicalSortKey {
                                expr: value(&k.expr),
                                ascending: k.ascending,
                                nulls_first: k.nulls_first,
                            })
                            .collect(),
                        partition_by: partition_by.clone(),
                        child: input(0)?,
                    },
                    N::Limit {
                        n,
                        offset,
                        partition_by,
                    } => NonASAPOp::Limit {
                        n: *n,
                        offset: *offset,
                        partition_by: partition_by.clone(),
                        child: input(0)?,
                    },
                    N::BinaryOp {
                        operator,
                        return_bool,
                    } => NonASAPOp::BinaryOp {
                        operator: operator.clone(),
                        return_bool: *return_bool,
                        lhs: input(0)?,
                        rhs: input(1)?,
                    },
                    N::SQLWindowFunc {
                        func,
                        args,
                        partition_by,
                        order_by,
                        frame,
                        output_name,
                    } => NonASAPOp::SQLWindowFunc {
                        func: func.clone(),
                        args: args.iter().map(&mut value).collect(),
                        partition_by: partition_by.clone(),
                        order_by: order_by
                            .iter()
                            .map(|k| LogicalSortKey {
                                expr: value(&k.expr),
                                ascending: k.ascending,
                                nulls_first: k.nulls_first,
                            })
                            .collect(),
                        frame: frame.clone(),
                        output_name: output_name.clone(),
                        child: input(0)?,
                    },
                    N::TimeRange { range, range_kind } => NonASAPOp::TimeRange {
                        range: *range,
                        kind: *range_kind,
                        child: input(0)?,
                    },
                    N::TimeShift { shift } => NonASAPOp::TimeShift {
                        shift: *shift,
                        child: input(0)?,
                    },
                    N::PromqlVectorFromScalar { expr } => {
                        NonASAPOp::PromqlVectorFromScalar(value(expr))
                    }
                    N::PromqlRelabel { dst, value: expr } => NonASAPOp::PromqlRelabel {
                        dst: dst.clone(),
                        value: value(expr),
                        child: input(0)?,
                    },
                    N::PromqlInfoEnrich { selector } => NonASAPOp::PromqlInfoEnrich {
                        selector: selector.clone(),
                        child: input(0)?,
                    },
                    N::PromqlSeriesSample { by, sample_kind } => NonASAPOp::PromqlSeriesSample {
                        by: by.clone(),
                        kind: *sample_kind,
                        child: input(0)?,
                    },
                    N::PromqlSubquery { range, resolution } => NonASAPOp::PromqlSubquery {
                        range: *range,
                        resolution: *resolution,
                        child: input(0)?,
                    },
                }),
                Payload::SummaryAgg {
                    family,
                    input: update,
                    reduction,
                    grouping,
                    filter,
                } => LogicalOperator::ASAP(ASAPOp::SummaryAgg {
                    child: input(0)?,
                    family: family.clone(),
                    input: update.clone(),
                    reduction: reduction.clone(),
                    grouping: grouping.clone(),
                    filter: filter.as_ref().map(|p| Predicate(value(&p.0))),
                }),
                Payload::SummaryEstimate { query } => {
                    LogicalOperator::ASAP(ASAPOp::SummaryEstimate {
                        summary_input: input(0)?,
                        query: query.clone(),
                    })
                }
                Payload::FinalizeExactAccumulator => {
                    LogicalOperator::ASAP(ASAPOp::FinalizeExactAccumulator { child: input(0)? })
                }
                Payload::MaintainPopulation { population } => {
                    LogicalOperator::ASAP(ASAPOp::MaintainPopulation {
                        child: input(0)?,
                        population: population.clone(),
                    })
                }
                Payload::EvaluatePopulation { evaluation } => {
                    LogicalOperator::ASAP(ASAPOp::EvaluatePopulation {
                        child: input(0)?,
                        evaluation: evaluation.clone(),
                    })
                }
                Payload::SummaryMerge => LogicalOperator::ASAP(ASAPOp::SummaryMerge {
                    children: inputs.clone(),
                }),
                _ => return Err(invalid("reserved ASAP operation has no native lowering")),
            };
            if missing {
                return Err(invalid(
                    "scalar reference is not a preceding DAG dependency",
                ));
            }
            let mut rebuilt = OperatorNode::with_schema(operator, node.output_schema.clone());
            rebuilt.guarantee = node.guarantee.clone();
            rebuilt.timing = Some(node.output_state.timing);
            done.insert(u64::from(node.id.0), Rc::new(rebuilt));
        }
        if next.len() == before {
            return Err(invalid("operator DAG is cyclic"));
        }
        remaining = next;
    }
    Ok(done)
}
