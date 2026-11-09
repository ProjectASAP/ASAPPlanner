//! `asap-stage0-memo` — #509 Stage 0: the memo groups of the workload's
//! query roots (a [`Memo`] without candidates). Each node of the queries'
//! DAGs becomes the group of its expression, found or added by
//! [`Memo::find_or_add_group`], so identical sub-expressions, within one
//! query or across queries, are one group. Stage 1 starts from these groups.
//!
//! It depends only on `asap-types`, where its output (`Memo`) lives and
//! where the one expression equality is; no other stage crate depends on
//! it, and the planner facade calls it first.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::{NonASAPOp, Operator, OperatorNode, QueryRoot, SchemaDerivationError};
use asap_types::stages::{Memo, MemoGroupId, MemoQuery, ValueSign};
use asap_types::workload::bounds::{series_read, source_name, values_per_result};
use asap_types::workload::{DataWorkload, PrometheusMetricType, SourceFacts};

/// Stage 0: the memo groups of the workload's query roots, query `i`
/// being `roots[i]` with accuracy target `accuracy_targets[i]` (`None`:
/// none), every group without candidates. Each query is added with its
/// $n_q$, the number of values one evaluation returns, which `data` bounds
/// ([`values_per_result`]); the memo holds the facts `data` declares for
/// the sources scanned ([`scanned_source_facts`]). Fails with the first node that
/// breaks the node rules over its children's groups
/// ([`Memo::find_or_add_group`]); the facade validates the roots first, so
/// that is a frontend bug.
pub fn memo_of_queries(
    roots: &[QueryRoot],
    accuracy_targets: &[Option<AccuracyTarget>],
    data: &DataWorkload,
) -> Result<Memo, SchemaDerivationError> {
    assert_eq!(
        roots.len(),
        accuracy_targets.len(),
        "one accuracy target per query"
    );
    let mut memo = Memo::new(scanned_source_facts(roots, data));
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
            values_per_evaluation: values_per_result(root, series_read(root, data), data),
        });
    }
    Ok(memo)
}

/// The facts `data` declares for each source the queries scan, by the name
/// the `Scan` reads ([`DataWorkload::series_facts`]: a histogram's or
/// summary's series by its family's), a declared counter's `value`
/// non-negative without a declaration.
fn scanned_source_facts(roots: &[QueryRoot], data: &DataWorkload) -> BTreeMap<String, SourceFacts> {
    let mut scanned = BTreeMap::new();
    let mut visited = HashSet::new();
    let mut pending: Vec<&Rc<OperatorNode>> =
        roots.iter().flat_map(QueryRoot::operator_refs).collect();
    while let Some(node) = pending.pop() {
        if !visited.insert(Rc::as_ptr(node)) {
            continue;
        }
        pending.extend(node.children());
        let Operator::NonASAP(NonASAPOp::Scan { source, .. }) = &node.operator else {
            continue;
        };
        let source = source_name(source);
        let Some((facts, series_type)) = data.series_facts(source) else {
            continue;
        };
        let mut facts = facts.clone();
        if series_type == Some(PrometheusMetricType::Counter) {
            facts
                .column_signs
                .entry("value".to_owned())
                .or_insert(ValueSign::NonNegative);
        }
        scanned.insert(source.to_owned(), facts);
    }
    scanned
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
) -> Result<MemoGroupId, SchemaDerivationError> {
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
    use asap_types::ir::operator::agg_intent::AggIntent;
    use asap_types::ir::operator::asap::ASAPOp;
    use asap_types::ir::operator::operator_properties::{
        BinaryOpKind, GroupKeys, Reduction, Source,
    };
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::scalar::expr_ir::{ColumnRef, CompareOpKind};
    use asap_types::ir::schema::sketch::{SketchAlgorithm, SketchStatistic, SummaryUpdate};
    use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
    use asap_types::ir::{BinaryOperator, NonASAPOp, Operator, ScalarExpr};

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
            output_names: vec![],
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
    fn memo_of_roots(roots: &[QueryRoot]) -> Result<Memo, SchemaDerivationError> {
        memo_of_queries(roots, &vec![None; roots.len()], &DataWorkload::default())
    }

    /// [`memo_of_roots`] of operator roots.
    fn memo_of(roots: Vec<Rc<OperatorNode>>) -> Memo {
        let roots: Vec<QueryRoot> = roots.into_iter().map(QueryRoot::Operator).collect();
        memo_of_roots(&roots).unwrap()
    }

    /// Query `query`'s root group.
    fn root(memo: &Memo, query: usize) -> MemoGroupId {
        match &memo.queries[query].root {
            QueryRoot::Operator(group) => *group,
            QueryRoot::Scalar(_) => panic!("expected an operator root"),
        }
    }

    /// Whether queries 0 and 1 of `[a, b]` are one group.
    fn one_group(a: Rc<OperatorNode>, b: Rc<OperatorNode>) -> bool {
        let memo = memo_of(vec![a, b]);
        root(&memo, 0) == root(&memo, 1)
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

    /// Aggregates that differ in a column, a quantile or a filter are
    /// different groups.
    #[test]
    fn aggregates_that_differ_in_an_argument_are_different_groups() {
        use asap_types::ir::scalar::ScalarValue;
        use asap_types::ir::Predicate;
        assert!(!one_group(
            quantile_agg(vec![1], Some(2), 0.5),
            quantile_agg(vec![1], Some(3), 0.5)
        ));
        assert!(!one_group(
            quantile_agg(vec![], Some(2), 0.5),
            quantile_agg(vec![], Some(2), 0.9)
        ));
        let filtered = node(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![1]),
            measures: vec![AggIntent::Quantile {
                col: Some(2),
                q: 0.5,
            }],
            output_names: vec![],
            filters: vec![Some(Predicate(ScalarExpr::Compare {
                left: Box::new(ScalarExpr::Column(3)),
                op: CompareOpKind::Gt,
                right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(1.0))),
                semantics: asap_types::ir::ExprSemantics::Sql,
            }))],
            having: None,
            child: scan(),
        });
        assert!(!one_group(quantile_agg(vec![1], Some(2), 0.5), filtered));
    }

    /// Equal nodes without a unique key are one group when they return the
    /// same rows each time: a keyless scan, a global aggregate, a `without`
    /// aggregate.
    #[test]
    fn equal_keyless_nodes_that_return_the_same_rows_are_one_group() {
        let table_scan = || {
            node(NonASAPOp::Scan {
                source: Source::Table {
                    table_ref: "metrics".into(),
                },
                predicates: vec![],
                schema: Schema::with_time_index(
                    vec![
                        Field::plain("ts", DataType::Timestamp, false),
                        Field::plain("value", DataType::Float64, true),
                    ],
                    0,
                    vec![],
                ),
            })
        };
        assert!(one_group(table_scan(), table_scan()));
        assert!(one_group(
            quantile_agg(vec![], Some(2), 0.9),
            quantile_agg(vec![], Some(2), 0.9)
        ));
        let without = || {
            node(NonASAPOp::Aggregate {
                reduction: Reduction::Reduce(GroupKeys::without(vec![0])),
                measures: vec![AggIntent::Count],
                output_names: vec![],
                filters: vec![],
                having: None,
                child: scan(),
            })
        };
        assert!(one_group(without(), without()));
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
        assert_eq!(memo.queries[0].accuracy_target, targets[0]);
        assert_eq!(memo.queries[1].accuracy_target, targets[1]);
    }

    /// `vector(scalar(sum by (service) (m)))`.
    fn scalar_of_vector() -> Rc<OperatorNode> {
        let sum_up = node(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![1]),
            measures: vec![AggIntent::Sum { col: Some(2) }],
            output_names: vec![],
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
        assert_eq!(memo.queries[2].root.operator_refs(), vec![&sum]);
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

    /// `DISTINCT` on every column returns the same rows each time and is one
    /// group; a `Dedup` on some columns keeps an unspecified row per key.
    #[test]
    fn dedup_is_one_group_only_on_every_column() {
        let dedup = |cols: Vec<usize>| {
            node(NonASAPOp::Dedup {
                cols,
                child: scan(),
            })
        };
        assert!(one_group(dedup(vec![]), dedup(vec![])));
        assert!(!one_group(dedup(vec![1]), dedup(vec![1])));
    }

    fn unordered_limit(child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        node(NonASAPOp::Limit {
            n: Some(10),
            offset: 0,
            partition_by: GroupKeys::default(),
            child,
        })
    }

    /// Two unordered `LIMIT 10`s may return different rows: two groups, and
    /// so are equal parents over them.
    #[test]
    fn unordered_limits_and_their_parents_are_different_groups() {
        assert!(!one_group(unordered_limit(scan()), unordered_limit(scan())));
        let distinct_over_limit = || {
            node(NonASAPOp::Dedup {
                cols: vec![],
                child: unordered_limit(scan()),
            })
        };
        assert!(!one_group(distinct_over_limit(), distinct_over_limit()));
    }

    /// A `LIMIT` over a `Sort` with the same partitions keeps the first rows
    /// of a fixed order (one group); over a `Sort` partitioned differently,
    /// which rows come first depends on input order (two groups).
    #[test]
    fn limits_over_a_sort_with_their_partitions_are_one_group() {
        let limit = |sort_partitions: GroupKeys| {
            node(NonASAPOp::Limit {
                n: Some(10),
                offset: 0,
                partition_by: GroupKeys::default(),
                child: node(NonASAPOp::Sort {
                    keys: vec![asap_types::ir::scalar::SortKey {
                        expr: ScalarExpr::Column(2),
                        ascending: false,
                        nulls_first: false,
                    }],
                    partition_by: sort_partitions,
                    child: scan(),
                }),
            })
        };
        assert!(one_group(
            limit(GroupKeys::default()),
            limit(GroupKeys::default())
        ));
        assert!(!one_group(
            limit(GroupKeys::by(vec![1])),
            limit(GroupKeys::by(vec![1]))
        ));
    }

    /// A projection emitting the literal `value`.
    fn literal(value: f64) -> Rc<OperatorNode> {
        node(NonASAPOp::Project {
            cols: vec![asap_types::ir::ProjectItem {
                alias: Some("v".into()),
                expr: ScalarExpr::literal_f64(value),
            }],
            child: scan(),
        })
    }

    /// Signed zeros and different non-finite literals are different groups;
    /// JSON's `null` for non-finite floats never decides equality.
    #[test]
    fn signed_zero_and_non_finite_literals_are_different_groups() {
        for (a, b) in [
            (0.0, -0.0),
            (-0.0, 0.0),
            (f64::INFINITY, f64::NEG_INFINITY),
            (f64::NAN, f64::NAN),
        ] {
            assert!(!one_group(literal(a), literal(b)), "{a} and {b}");
        }
        assert!(one_group(literal(f64::INFINITY), literal(f64::INFINITY)));
    }

    /// A frontend node that breaks the node rules (a readout of raw rows
    /// instead of summary state) makes Stage 0 return the error.
    #[test]
    fn memo_of_queries_returns_the_error_of_a_node_that_breaks_the_node_rules() {
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
        assert_eq!(memo.queries[0].values_per_evaluation, Some(1000));
        assert_eq!(memo.queries[1].values_per_evaluation, Some(1));
        let undeclared = memo_of_roots(&roots).unwrap();
        assert_eq!(undeclared.queries[0].values_per_evaluation, None);
    }

    /// A `Scan`'s column signs are the data workload's declared ones: `m`
    /// declares `latency` non-negative, the counter `requests_total`'s
    /// `value` is non-negative without a declaration, and every other
    /// column is `Unknown`.
    #[test]
    fn a_scan_reads_the_declared_column_signs() {
        let counter = node(NonASAPOp::Scan {
            source: Source::TimeSeries {
                metric: "requests_total".into(),
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
        });
        let data = DataWorkload {
            sources: BTreeMap::from([
                (
                    "m".to_owned(),
                    asap_types::workload::SourceFacts {
                        column_signs: BTreeMap::from([(
                            "latency".to_owned(),
                            ValueSign::NonNegative,
                        )]),
                        ..Default::default()
                    },
                ),
                (
                    "requests_total".to_owned(),
                    asap_types::workload::SourceFacts {
                        metric_type: Some(PrometheusMetricType::Counter),
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };
        let roots = [QueryRoot::Operator(scan()), QueryRoot::Operator(counter)];
        let memo = memo_of_queries(&roots, &[None, None], &data).unwrap();
        use ValueSign::{NonNegative, Unknown};
        let signs = |query| {
            &memo.groups()[root(&memo, query).0 as usize]
                .group_properties
                .column_sign
        };
        // [ts, service, value, latency]
        assert_eq!(signs(0), &[Unknown, Unknown, Unknown, NonNegative]);
        // [ts, value]
        assert_eq!(signs(1), &[Unknown, NonNegative]);
    }
}
