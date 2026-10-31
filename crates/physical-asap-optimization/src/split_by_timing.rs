//! Splitting a shared sub-DAG by execution timing before Stage 2 applies a
//! materialization assignment ([`split_shared_by_timing`]).

use std::collections::HashMap;
use std::rc::Rc;

use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{timing_under_consumer, MaterializationAssignment, OperatorNode};

/// Each (node, timing) pair's rebuilt node, with the original it copies.
type Copies = HashMap<(*const OperatorNode, ExecutionTiming), (Rc<OperatorNode>, Rc<OperatorNode>)>;

/// Copy, for one consumer, every sub-DAG that `assignment` would reach with
/// two different timings from `roots` (one query each, or the roots of one
/// query), so that a workload whose CSE shared a `Scan` between an
/// ingestion-time summary and a query-time computation, in one query or
/// across queries, can still be timed. Only the conflicting sub-DAGs are
/// copied, one copy per timing; a sub-DAG reached with one timing stays one
/// `Rc`, shared by the roots that read it. Returns the (possibly rewritten)
/// roots, in order, and `assignment` remapped onto them: a rebuilt node
/// keeps its original's explicit materialization.
pub fn split_shared_by_timing(
    roots: &[Rc<OperatorNode>],
    assignment: &MaterializationAssignment,
) -> (Vec<Rc<OperatorNode>>, MaterializationAssignment) {
    // First pass: the set of timings each node is reached with.
    let mut reached: HashMap<*const OperatorNode, Vec<ExecutionTiming>> = HashMap::new();
    let mut forced = HashMap::new();
    fn collect(
        node: &Rc<OperatorNode>,
        consumer: ExecutionTiming,
        assignment: &MaterializationAssignment,
        reached: &mut HashMap<*const OperatorNode, Vec<ExecutionTiming>>,
        forced: &mut HashMap<*const OperatorNode, bool>,
    ) {
        let timing = timing_under_consumer(node, consumer, assignment, forced);
        let entry = reached.entry(Rc::as_ptr(node)).or_default();
        if entry.contains(&timing) {
            return;
        }
        entry.push(timing);
        for child in node.children() {
            collect(child, timing, assignment, reached, forced);
        }
    }
    for root in roots {
        collect(
            root,
            ExecutionTiming::QueryTime,
            assignment,
            &mut reached,
            &mut forced,
        );
    }
    if reached.values().all(|timings| timings.len() <= 1) {
        return (roots.to_vec(), assignment.clone());
    }
    // Second pass: rebuild, giving each (node, timing) pair its own copy,
    // kept with its original.
    let mut copies: Copies = HashMap::new();
    fn rebuild(
        node: &Rc<OperatorNode>,
        consumer: ExecutionTiming,
        assignment: &MaterializationAssignment,
        reached: &HashMap<*const OperatorNode, Vec<ExecutionTiming>>,
        copies: &mut Copies,
        forced: &mut HashMap<*const OperatorNode, bool>,
    ) -> Rc<OperatorNode> {
        let timing = timing_under_consumer(node, consumer, assignment, forced);
        let key = (Rc::as_ptr(node), timing);
        if let Some((_, done)) = copies.get(&key) {
            return Rc::clone(done);
        }
        let conflicted = reached
            .get(&Rc::as_ptr(node))
            .is_some_and(|timings| timings.len() > 1);
        let mut changed = conflicted;
        let operator = node.operator.map_children(|child| {
            let rebuilt = rebuild(child, timing, assignment, reached, copies, forced);
            changed |= !Rc::ptr_eq(&rebuilt, child);
            rebuilt
        });
        let out = if changed {
            Rc::new(
                OperatorNode::with_schema(operator, node.schema.clone())
                    .with_guarantee(node.guarantee.clone())
                    .with_timing(node.timing)
                    .with_materialization(node.materialization),
            )
        } else {
            Rc::clone(node)
        };
        copies.insert(key, (Rc::clone(node), Rc::clone(&out)));
        out
    }
    let split = roots
        .iter()
        .map(|root| {
            rebuild(
                root,
                ExecutionTiming::QueryTime,
                assignment,
                &reached,
                &mut copies,
                &mut forced,
            )
        })
        .collect();
    let mut remapped = assignment.clone();
    for (original, copy) in copies.values() {
        if let Some(materialization) = assignment.get(original) {
            remapped.set(copy, materialization);
        }
    }
    (split, remapped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::operator::agg_intent::AggIntent;
    use asap_types::ir::operator::operator_properties::{Reduction, Source};
    use asap_types::ir::properties::{ExecutionDataState, ExecutionDataStateError};
    use asap_types::ir::scalar::expr_ir::ColumnRef;
    use asap_types::ir::schema::sketch::{
        ExactKind, ExactParams, GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams,
        SketchStatistic, SummaryUpdate,
    };
    use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
    use asap_types::ir::{
        apply_materialization_timings, ASAPOp, NodeToTimedNodeCache, NonASAPOp, Operator,
    };
    use asap_types::physical::{Materialization, Medium};

    const AT_INGESTION: Materialization = Materialization::MatAtIngestion(Medium::Memory);

    fn scan() -> Rc<OperatorNode> {
        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("value", DataType::Float64, false),
                    Field::plain("zone", DataType::Utf8, true),
                ],
                0,
                vec![],
            ),
        }))
        .unwrap()
    }

    fn kll() -> FieldDataType {
        FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll {
                    k: 200,
                    failure_probability: None,
                },
            ),
            GroupingStrategy::default(),
        )
    }

    fn exact_sum() -> FieldDataType {
        FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
    }

    fn agg(child: Rc<OperatorNode>, family: FieldDataType) -> Rc<OperatorNode> {
        Rc::new(
            OperatorNode::with_schema(
                Operator::ASAP(ASAPOp::SummaryAgg {
                    child,
                    family: family.clone(),
                    input: SummaryUpdate::column(ColumnRef::SampleValue),
                    reduction: Reduction::by(vec![]),
                    grouping: GroupingStrategy::default(),
                    filter: None,
                }),
                Schema::lifted(vec![Field::new("state", family, false)], None),
            )
            .with_guarantee(None),
        )
    }

    fn estimate(child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        Rc::new(
            OperatorNode::with_schema(
                Operator::ASAP(ASAPOp::SummaryEstimate {
                    summary_input: child,
                    query: SketchStatistic::Quantile { q: 0.99 },
                }),
                Schema::lifted(
                    vec![Field::plain("quantile_0_99", DataType::Float64, false)],
                    None,
                ),
            )
            .with_guarantee(None),
        )
    }

    fn max(child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![]),
            measures: vec![AggIntent::Max { col: None }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child,
        }))
        .unwrap()
    }

    fn concat(children: Vec<Rc<OperatorNode>>) -> Rc<OperatorNode> {
        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Concat {
            children,
            discriminator_unique_key: None,
        }))
        .unwrap()
    }

    fn finalize(child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        OperatorNode::new_shared(Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }))
            .unwrap()
    }

    fn child(node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        Rc::clone(node.children()[0])
    }

    /// One shared sub-DAG reached as ingestion-time input and as query-time
    /// input cannot be executed once for both; splitting it by timing first
    /// makes the plan timeable.
    #[test]
    fn a_shared_subtree_reached_at_two_timings_conflicts() {
        let shared = scan();
        let root = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Concat {
            children: vec![
                max(estimate(agg(Rc::clone(&shared), kll()))),
                max(Rc::clone(&shared)),
            ],
            discriminator_unique_key: None,
        }))
        .unwrap();
        assert_eq!(
            apply_materialization_timings(
                &root,
                &MaterializationAssignment::with_default(AT_INGESTION),
                &mut NodeToTimedNodeCache::new()
            )
            .err(),
            Some(ExecutionDataStateError::ConflictingTiming {
                first: ExecutionDataState::INGESTION_ROWS,
                second: ExecutionDataState::QUERY_ROWS,
            })
        );
        let (split, assignment) = split_shared_by_timing(
            std::slice::from_ref(&root),
            &MaterializationAssignment::with_default(AT_INGESTION),
        );
        assert!(apply_materialization_timings(
            &split[0],
            &assignment,
            &mut NodeToTimedNodeCache::new()
        )
        .is_ok());
    }

    /// Splitting rebuilds the ancestors of a copied sub-DAG; an explicit
    /// assignment on one of them follows it onto the rebuilt node.
    #[test]
    fn split_keeps_explicit_assignments() {
        let shared = scan();
        let summary = agg(Rc::clone(&shared), kll());
        let root = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Concat {
            children: vec![max(estimate(Rc::clone(&summary))), max(Rc::clone(&shared))],
            discriminator_unique_key: None,
        }))
        .unwrap();
        let mut assignment = MaterializationAssignment::default();
        assignment.set(&summary, AT_INGESTION);
        let (split, assignment) = split_shared_by_timing(std::slice::from_ref(&root), &assignment);
        let timed =
            apply_materialization_timings(&split[0], &assignment, &mut NodeToTimedNodeCache::new())
                .unwrap();
        let summary = child(&child(&child(&timed)));
        assert!(matches!(
            summary.operator,
            Operator::ASAP(ASAPOp::SummaryAgg { .. })
        ));
        assert_eq!(summary.materialization, AT_INGESTION);
        assert_eq!(summary.timing, Some(ExecutionTiming::IngestionTime));
    }

    /// Two queries share a scan: one reads it through a summary built at
    /// ingestion time, the other at query time. Splitting the workload's
    /// roots together copies the scan for one phase, so one timing pass
    /// over both roots succeeds; a node both queries reach at one timing
    /// stays one node.
    #[test]
    fn split_across_roots_copies_only_what_two_queries_read_at_two_timings() {
        let shared = scan();
        let summary = agg(Rc::clone(&shared), kll());
        let by_summary = max(estimate(Rc::clone(&summary)));
        let raw = max(Rc::clone(&shared));
        let common = max(estimate(agg(scan(), kll())));
        let roots = [
            concat(vec![Rc::clone(&by_summary), Rc::clone(&common)]),
            concat(vec![raw, Rc::clone(&common)]),
        ];
        let mut assignment = MaterializationAssignment::default();
        assignment.set(&summary, AT_INGESTION);
        let timed_unsplit = {
            let mut cache = NodeToTimedNodeCache::new();
            roots
                .iter()
                .map(|root| apply_materialization_timings(root, &assignment, &mut cache))
                .collect::<Result<Vec<_>, _>>()
        };
        assert!(matches!(
            timed_unsplit,
            Err(ExecutionDataStateError::ConflictingTiming { .. })
        ));
        let (split, assignment) = split_shared_by_timing(&roots, &assignment);
        assert_eq!(split.len(), 2);
        let mut cache = NodeToTimedNodeCache::new();
        let timed: Vec<_> = split
            .iter()
            .map(|root| apply_materialization_timings(root, &assignment, &mut cache).unwrap())
            .collect();
        let summary = child(&child(&child(&timed[0])));
        assert_eq!(summary.materialization, AT_INGESTION);
        assert_eq!(child(&summary).timing, Some(ExecutionTiming::IngestionTime));
        assert_eq!(
            child(&child(&timed[1])).timing,
            Some(ExecutionTiming::QueryTime)
        );
        assert!(Rc::ptr_eq(timed[0].children()[1], timed[1].children()[1]));
    }

    /// Splitting a workload that also shares a scan at two timings keeps a
    /// conflicting assignment on a fixed-timing node: the split roots still
    /// fail with the conflict instead of dropping the assignment.
    #[test]
    fn a_fixed_timing_conflict_survives_the_split() {
        let shared = scan();
        let summary = agg(Rc::clone(&shared), kll());
        let pair = concat(vec![
            max(estimate(Rc::clone(&summary))),
            max(Rc::clone(&shared)),
        ]);
        let state = agg(Rc::clone(&shared), exact_sum());
        let read = Rc::new(
            (*finalize(Rc::clone(&state)))
                .clone()
                .with_timing(Some(ExecutionTiming::QueryTime)),
        );
        let mut assignment = MaterializationAssignment::default();
        assignment.set(&summary, AT_INGESTION);
        assignment.set(&state, AT_INGESTION);
        assignment.set(&read, AT_INGESTION);
        let (split, assignment) = split_shared_by_timing(&[pair, read], &assignment);
        let mut cache = NodeToTimedNodeCache::new();
        let timed: Result<Vec<_>, _> = split
            .iter()
            .map(|root| apply_materialization_timings(root, &assignment, &mut cache))
            .collect();
        assert_eq!(
            timed.err(),
            Some(
                ExecutionDataStateError::MaterializationConflictsWithFixedTiming {
                    operator: "FinalizeExactAccumulator",
                    fixed: ExecutionTiming::QueryTime,
                    materialization: AT_INGESTION,
                }
            )
        );
    }
}
