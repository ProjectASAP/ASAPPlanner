//! `asap-stage0-memo` — #509 Stage 0: the memo groups of the workload's
//! query roots (a [`Memo`] without candidates). Each node of the queries'
//! DAGs becomes the group of its expression, found or added by
//! [`Memo::find_or_add_group`], so identical sub-expressions, within one
//! query or across queries, are one group. Stage 1 starts from these groups.
//!
//! It depends only on `asap-types`, where its output (`Memo`) lives and
//! where the one expression equality is; no other stage crate depends on
//! it, and the planner facade calls it first.

use std::collections::HashMap;
use std::rc::Rc;

use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::stages::{Memo, MemoGroupError, MemoGroupId, MemoQuery};
use asap_types::workload::bounds::{series_read, values_per_result};
use asap_types::workload::DataWorkload;

/// Stage 0: the memo groups of the workload's query roots, query `i`
/// being `roots[i]` with accuracy target `accuracy_targets[i]` (`None`:
/// none), every group without candidates. Each query is added with its
/// $n_q$, the number of values one evaluation returns, which `data` bounds
/// ([`values_per_result`]); a `Scan`'s column signs are the ones `data`
/// declares, which the memo holds (`Memo::new`). Fails with the first node
/// whose expression is not valid over its children's groups
/// ([`Memo::find_or_add_group`]); the facade validates the roots first, so
/// that is a frontend bug.
pub fn memo_of_queries(
    roots: &[QueryRoot],
    accuracy_targets: &[Option<AccuracyTarget>],
    data: &DataWorkload,
) -> Result<Memo, MemoGroupError> {
    assert_eq!(
        roots.len(),
        accuracy_targets.len(),
        "one accuracy target per query"
    );
    let mut memo = Memo::new(data);
    let mut group_of_node = GroupOfNode::new();
    for root in roots {
        for node in root.operator_refs() {
            group_of(&mut memo, &mut group_of_node, node)?;
        }
    }
    let group = |node: &Rc<OperatorNode>| group_of_node[&Rc::as_ptr(node)];
    for (root, accuracy_target) in roots.iter().zip(accuracy_targets) {
        memo.add_query(MemoQuery {
            root: match root {
                QueryRoot::Operator(node) => QueryRoot::Operator(group(node)),
                QueryRoot::Scalar(expr) => {
                    QueryRoot::Scalar(expr.map_operator_refs(&mut |node| group(node)))
                }
            },
            accuracy_target: accuracy_target.clone(),
            number_of_values_per_query_evaluation: values_per_result(
                root,
                series_read(root, data),
                data,
            ),
        });
    }
    Ok(memo)
}

/// Each node of the queries' DAGs already visited, by address, to its
/// group: a node reached through several paths is visited once. The roots
/// outlive the map, so no address is reused.
type GroupOfNode = HashMap<*const OperatorNode, MemoGroupId>;

/// The group of `node`: its operator over its children's groups (each
/// found or added first), found or added by [`Memo::find_or_add_group`].
fn group_of(
    memo: &mut Memo,
    group_of_node: &mut GroupOfNode,
    node: &Rc<OperatorNode>,
) -> Result<MemoGroupId, MemoGroupError> {
    if let Some(&group) = group_of_node.get(&Rc::as_ptr(node)) {
        return Ok(group);
    }
    for child in node.children() {
        group_of(memo, group_of_node, child)?;
    }
    let expression = node
        .operator
        .map_children(|child| group_of_node[&Rc::as_ptr(child)]);
    let group = memo.find_or_add_group(expression)?;
    group_of_node.insert(Rc::as_ptr(node), group);
    Ok(group)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use asap_types::accuracy::AccuracyTarget;
    use asap_types::ir::operator::agg_intent::AggIntent;
    use asap_types::ir::operator::arguments::{BinaryOpKind, Reduction, Source};
    use asap_types::ir::operator::asap::ASAPOp;
    use asap_types::ir::scalar::expr_ir::{ColumnRef, CompareOpKind};
    use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
    use asap_types::ir::{BinaryOperator, NonASAPOp, Operator, ScalarExpr};
    use asap_types::stages::ValueSign;
    use asap_types::summary_type::{SketchAlgorithm, SketchStatistic, SummaryUpdate};

    fn node(op: NonASAPOp) -> Rc<OperatorNode> {
        OperatorNode::new_shared(Operator::NonASAP(op)).unwrap()
    }

    /// `[ts, service, value, latency]`, no unique key.
    fn scan() -> Rc<OperatorNode> {
        node(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("service", DataType::Utf8, false),
                    Field::plain("value", DataType::Float64, false),
                    Field::plain("latency", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        })
    }

    fn quantile_agg(by: Vec<usize>, col: Option<usize>, q: f64) -> Rc<OperatorNode> {
        node(NonASAPOp::Aggregate {
            reduction: Reduction::by(by),
            measures: vec![AggIntent::Quantile { col, q }],
            output_names: vec![String::new()],
            filters: vec![],
            having: None,
            child: scan(),
        })
    }

    fn compare(lhs: Rc<OperatorNode>, rhs: Rc<OperatorNode>) -> Rc<OperatorNode> {
        node(NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                checked_relative_division: false,
                checked_finite_division: false,
                kind: BinaryOpKind::Compare(CompareOpKind::Eq),
                vector_match: None,
            },
            return_bool: false,
            lhs,
            rhs,
        })
    }

    /// The memo of `roots`, one query per root, without accuracy targets
    /// or data facts.
    fn memo_of_roots(roots: &[QueryRoot]) -> Result<Memo, MemoGroupError> {
        memo_of_queries(roots, &vec![None; roots.len()], &DataWorkload::default())
    }

    /// [`memo_of_roots`] of operator roots.
    fn memo_of(roots: Vec<Rc<OperatorNode>>) -> Memo {
        let roots: Vec<QueryRoot> = roots.into_iter().map(QueryRoot::Operator).collect();
        memo_of_roots(&roots).unwrap()
    }

    /// Query `query`'s root group.
    fn root(memo: &Memo, query: usize) -> MemoGroupId {
        match &memo.queries()[query].root {
            QueryRoot::Operator(group) => *group,
            QueryRoot::Scalar(_) => panic!("expected an operator root"),
        }
    }

    /// Two queries sharing a sub-expression read one group for it, and
    /// every group lists the queries that reach it and the groups that
    /// read it.
    #[test]
    fn two_queries_sharing_a_sub_expression_get_one_group() {
        let p50 = quantile_agg(vec![1], Some(2), 0.5);
        let p50_vs_p90 = compare(
            quantile_agg(vec![1], Some(2), 0.5),
            quantile_agg(vec![1], Some(2), 0.9),
        );
        let memo = memo_of(vec![p50, p50_vs_p90]);
        // scan, p50, p90, compare.
        assert_eq!(memo.groups().len(), 4);
        let (scan, p50, compare) = (MemoGroupId(0), root(&memo, 0), root(&memo, 1));
        let p90 = MemoGroupId(2);
        assert_eq!(
            memo.groups()[compare.0 as usize].expression.children(),
            vec![&p50, &p90]
        );
        assert_eq!(memo.groups()[scan.0 as usize].reached_by_queries(), &[0, 1]);
        assert_eq!(memo.groups()[p50.0 as usize].reached_by_queries(), &[0, 1]);
        assert_eq!(memo.groups()[p90.0 as usize].reached_by_queries(), &[1]);
        assert_eq!(memo.groups()[compare.0 as usize].reached_by_queries(), &[1]);
        assert_eq!(memo.groups()[scan.0 as usize].parent_groups(), &[p50, p90]);
        assert_eq!(memo.groups()[p50.0 as usize].parent_groups(), &[compare]);
        assert!(memo
            .groups()
            .iter()
            .all(|group| group.candidates.is_empty()));
    }

    /// One query's repeated sub-expression is one group, read twice.
    #[test]
    fn a_query_s_repeated_sub_expression_is_one_group() {
        let memo = memo_of(vec![compare(
            quantile_agg(vec![1], Some(2), 0.5),
            quantile_agg(vec![1], Some(2), 0.5),
        )]);
        assert_eq!(memo.groups().len(), 3);
        let p50 = MemoGroupId(1);
        assert_eq!(
            memo.groups()[root(&memo, 0).0 as usize]
                .expression
                .children(),
            vec![&p50, &p50]
        );
        assert_eq!(
            memo.groups()[p50.0 as usize].parent_groups(),
            &[root(&memo, 0)]
        );
    }

    /// A node reached through two paths is one group, visited once.
    #[test]
    fn an_already_shared_node_is_one_group() {
        let p50 = quantile_agg(vec![1], Some(2), 0.5);
        let memo = memo_of(vec![compare(Rc::clone(&p50), p50)]);
        assert_eq!(memo.groups().len(), 3);
    }

    /// Queries that differ only in their accuracy target are one group: the
    /// target is the query's ([`MemoQuery::accuracy_target`]), not an
    /// argument of its operators.
    #[test]
    fn queries_that_differ_only_in_their_accuracy_target_are_one_group() {
        let roots = [
            QueryRoot::Operator(quantile_agg(vec![1], Some(2), 0.99)),
            QueryRoot::Operator(quantile_agg(vec![1], Some(2), 0.99)),
        ];
        let targets = [
            Some(AccuracyTarget::max_error(0.01)),
            Some(AccuracyTarget::max_error(0.05)),
        ];
        let memo = memo_of_queries(&roots, &targets, &DataWorkload::default()).unwrap();
        assert_eq!(root(&memo, 0), root(&memo, 1));
        assert_eq!(memo.groups().len(), 2);
        assert_eq!(memo.queries()[0].accuracy_target, targets[0]);
        assert_eq!(memo.queries()[1].accuracy_target, targets[1]);
    }

    /// `vector(scalar(sum by (service) (m)))`.
    fn scalar_of_vector() -> Rc<OperatorNode> {
        let sum_up = node(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![1]),
            measures: vec![AggIntent::Sum { col: Some(2) }],
            output_names: vec![String::new()],
            filters: vec![],
            having: None,
            child: scan(),
        });
        node(NonASAPOp::PromqlVectorFromScalar(
            ScalarExpr::PromqlScalarFromVector(sum_up),
        ))
    }

    /// A vector read through `scalar(v)` is a child group like any input,
    /// one group across queries, and a scalar query's root reads it.
    #[test]
    fn a_scalar_referenced_vector_is_one_group_across_queries() {
        let roots = vec![
            QueryRoot::Operator(scalar_of_vector()),
            QueryRoot::Operator(scalar_of_vector()),
            QueryRoot::Scalar(ScalarExpr::PromqlScalarFromVector(Rc::clone(
                scalar_of_vector().children()[0],
            ))),
        ];
        let memo = memo_of_roots(&roots).unwrap();
        // scan, sum, bridge.
        assert_eq!(memo.groups().len(), 3);
        assert_eq!(root(&memo, 0), root(&memo, 1));
        let sum = MemoGroupId(1);
        assert_eq!(memo.queries()[2].root.operator_refs(), vec![&sum]);
        assert_eq!(
            memo.groups()[sum.0 as usize].reached_by_queries(),
            &[0, 1, 2]
        );
    }

    /// `SummaryAgg[DDSketch](scan)`.
    fn summary_agg() -> Rc<OperatorNode> {
        OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
            child: scan(),
            summary_type: FieldDataType::Sketch(SketchAlgorithm::DDSketch),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
        }))
        .unwrap()
    }

    /// Two quantile readouts of one summary are two groups over one state
    /// group.
    #[test]
    fn readouts_share_their_state_group_but_not_each_other() {
        let readout = |q| {
            OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: summary_agg(),
                query: SketchStatistic::Quantile { q },
            }))
            .unwrap()
        };
        let memo = memo_of(vec![readout(0.95), readout(0.99)]);
        let (p95, p99) = (root(&memo, 0), root(&memo, 1));
        assert_ne!(p95, p99);
        assert_eq!(
            memo.groups()[p95.0 as usize].expression.children(),
            memo.groups()[p99.0 as usize].expression.children()
        );
        assert_eq!(memo.groups().len(), 4);
    }

    /// A frontend node whose expression is not valid over its children's
    /// groups (a readout of raw rows instead of summary state) makes Stage 0
    /// return the error.
    #[test]
    fn memo_of_queries_returns_the_error_of_a_node_invalid_over_its_input_groups() {
        let readout_of_rows = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: scan(),
            query: SketchStatistic::Quantile { q: 0.5 },
        }))
        .unwrap();
        assert!(memo_of_roots(&[QueryRoot::Operator(readout_of_rows)]).is_err());
    }

    /// `m{…}` at the evaluation time: the latest sample of each series.
    fn instant_selector(source: Rc<OperatorNode>) -> Rc<OperatorNode> {
        node(NonASAPOp::TimeRange {
            range: std::time::Duration::from_secs(60),
            kind: asap_types::ir::TimeRangeKind::Instant,
            child: source,
        })
    }

    /// Each query enters the memo with $n_q$, the values one evaluation
    /// returns, as the data workload bounds it: `m`'s 1000 series for the
    /// instant vector `m`, one for the scalar `scalar(sum(m))`, unknown for
    /// `m` when no series count is declared.
    #[test]
    fn each_query_enters_with_the_values_one_evaluation_returns() {
        let roots = [
            QueryRoot::Operator(instant_selector(scan())),
            QueryRoot::Scalar(ScalarExpr::PromqlScalarFromVector(Rc::clone(
                scalar_of_vector().children()[0],
            ))),
        ];
        let declared = DataWorkload {
            sources: BTreeMap::from([(
                "m".to_owned(),
                asap_types::workload::SourceFacts {
                    series_count: Some(1000),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let memo = memo_of_queries(&roots, &[None, None], &declared).unwrap();
        assert_eq!(
            memo.queries()[0].number_of_values_per_query_evaluation,
            Some(1000)
        );
        assert_eq!(
            memo.queries()[1].number_of_values_per_query_evaluation,
            Some(1)
        );
        let undeclared = memo_of_roots(&roots).unwrap();
        assert_eq!(
            undeclared.queries()[0].number_of_values_per_query_evaluation,
            None
        );
    }

    /// `memo_of_queries` passes the data workload's facts to the memo: `m`
    /// declares `latency` non-negative, which its `Scan` group's signs read.
    #[test]
    fn a_scan_reads_the_declared_column_signs() {
        let data = DataWorkload {
            sources: BTreeMap::from([(
                "m".to_owned(),
                asap_types::workload::SourceFacts {
                    column_signs: BTreeMap::from([("latency".to_owned(), ValueSign::NonNegative)]),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let memo = memo_of_queries(&[QueryRoot::Operator(scan())], &[None], &data).unwrap();
        use ValueSign::{NonNegative, Unknown};
        // [ts, service, value, latency]
        assert_eq!(
            memo.groups()[root(&memo, 0).0 as usize]
                .group_properties
                .column_signs,
            [Unknown, Unknown, Unknown, NonNegative]
        );
    }
}
