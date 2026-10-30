use std::rc::Rc;

use super::*;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::{ASAPOp, QueryRoot};
use asap_types::physical::Medium;
use asap_types::stages::{
    CandidateForMemoGroupId, MemoGroupId, OperatorNodeOfCandidateForMemoGroup,
    OperatorNodeOfCandidateForMemoGroupId, ProducerCandidates,
};

const QUERY_TIME: ExecutionTiming = ExecutionTiming::QueryTime;
const NOT_MATERIALIZED: Materialization = Materialization::NotMaterialized;
const AT_QUERY: Materialization = Materialization::MatAtQuery(Medium::Memory);

/// A one-row `VALUES` node: stateless at any timing.
fn values_node(value: f64) -> Rc<OperatorNode> {
    use asap_types::ir::scalar::expr_ir::ScalarValue;
    use asap_types::ir::schema::{DataType, Field, Schema};
    use asap_types::ir::ScalarExpr;
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Values {
        rows: vec![vec![ScalarExpr::Literal(ScalarValue::Float64(value))]],
        schema: Schema::lifted(vec![Field::plain("value", DataType::Float64, false)], None),
    }))
    .unwrap()
}

/// One hand-built candidate: operator node `node`, query time, not
/// materialized, `producers` per input, no output, no keep flags.
fn copy_of(node: usize, producers: &[&[usize]]) -> PhysicalCandidate {
    PhysicalCandidate {
        operator_node: OperatorNodeOfCandidateForMemoGroupId(node),
        timing: QUERY_TIME,
        materialization: NOT_MATERIALIZED,
        producer_candidates_per_input: producers
            .iter()
            .map(|input| {
                input
                    .iter()
                    .map(|&id| PhysicalCandidateId(id))
                    .collect::<ProducerCandidates>()
            })
            .collect(),
        may_produce_query_output: false,
        may_keep_across_evaluations_per_sole_reader_query: Vec::new(),
    }
}

/// `candidates` over `nodes`, operator node `i` the only candidate of
/// group `i`; the last group is the one query's root.
fn hand_built(
    nodes: Vec<Rc<OperatorNode>>,
    candidates: Vec<PhysicalCandidate>,
    reader_constraints: Vec<ReaderConstraint>,
) -> PhysicalASAPMemoCandidates {
    let root = MemoGroupId(nodes.len() as u32 - 1);
    PhysicalASAPMemoCandidates::new(
        nodes
            .into_iter()
            .enumerate()
            .map(
                |(group, physical_node)| OperatorNodeOfCandidateForMemoGroup {
                    candidates_for_memo_group: vec![CandidateForMemoGroupId {
                        group: MemoGroupId(group as u32),
                        index: 0,
                    }],
                    is_implementation_root: true,
                    physical_node,
                },
            )
            .collect(),
        candidates,
        vec![QueryRoot::Operator(root)],
        reader_constraints,
    )
}

/// Stage 2's copy chain: copy `k + 1` requires copy `k`.
fn activation_chain(copies: &[usize]) -> Vec<ReaderConstraint> {
    copies
        .windows(2)
        .map(|pair| ReaderConstraint::ActivationRequires {
            candidate: PhysicalCandidateId(pair[1]),
            required_candidate: PhysicalCandidateId(pair[0]),
        })
        .collect()
}

fn ids(values: &[usize]) -> Vec<PhysicalCandidateId> {
    values.iter().map(|&id| PhysicalCandidateId(id)).collect()
}

/// The copies of each class, in class order.
fn copies_per_class(classes: &CandidateClasses) -> Vec<Vec<PhysicalCandidateId>> {
    classes
        .classes
        .iter()
        .map(|class| class.copies.clone())
        .collect()
}

// Three copies of a leaf and two of its reader, wired copy to copy as
// Stage 2 does: one class each, with the copy count as capacity.
#[test]
fn equal_copies_fold_into_one_class_with_their_count_as_capacity() {
    let leaf: &[usize] = &[0, 1, 2];
    let mut constraints = activation_chain(&[0, 1, 2]);
    constraints.extend(activation_chain(&[3, 4]));
    let physical = hand_built(
        vec![values_node(1.0), values_node(2.0)],
        vec![
            copy_of(0, &[]),
            copy_of(0, &[]),
            copy_of(0, &[]),
            copy_of(1, &[leaf]),
            copy_of(1, &[leaf]),
        ],
        constraints,
    );
    let classes = fold_copies_into_classes(&physical);
    assert_eq!(
        copies_per_class(&classes),
        vec![ids(&[0, 1, 2]), ids(&[3, 4])]
    );
    assert_eq!(classes.class(CandidateClassId(0)).capacity(), 3);
    assert_eq!(classes.class(CandidateClassId(1)).capacity(), 2);
    assert_eq!(
        classes
            .class(CandidateClassId(1))
            .producer_classes_per_input,
        vec![vec![CandidateClassId(0)]]
    );
    for id in 0..5 {
        let expected = CandidateClassId(usize::from(id >= 3));
        assert_eq!(classes.class_of(PhysicalCandidateId(id)), expected);
    }
}

// Two copies of one leaf that differ in exactly one property Stage 2 sets
// per candidate stay two classes, whichever property it is.
#[test]
fn copies_that_differ_in_one_property_stay_apart() {
    type Change = fn(&mut PhysicalCandidate, &mut Vec<ReaderConstraint>);
    let changes: [(&str, Change); 7] = [
        ("timing", |copy, _| {
            copy.timing = ExecutionTiming::IngestionTime
        }),
        ("materialization", |copy, _| copy.materialization = AT_QUERY),
        ("query output", |copy, _| {
            copy.may_produce_query_output = true
        }),
        ("keep flags", |copy, _| {
            copy.may_keep_across_evaluations_per_sole_reader_query = vec![true]
        }),
        ("minimum readers", |_, constraints| {
            constraints.push(ReaderConstraint::MinimumQueryReaders {
                candidate: PhysicalCandidateId(1),
                query_outputs: vec![vec![0]],
                minimum: 2,
            })
        }),
        ("allowed outputs", |_, constraints| {
            constraints.push(ReaderConstraint::AllowedOutputs {
                candidate: PhysicalCandidateId(1),
                outputs: vec![0],
            })
        }),
        ("incompatible outputs", |_, constraints| {
            constraints.push(ReaderConstraint::IncompatibleOutputs {
                candidate: PhysicalCandidateId(1),
                left: 0,
                right: 1,
            })
        }),
    ];
    for (property, change) in changes {
        let mut second = copy_of(0, &[]);
        let mut constraints = Vec::new();
        change(&mut second, &mut constraints);
        let physical = hand_built(
            vec![values_node(1.0)],
            vec![copy_of(0, &[]), second],
            constraints,
        );
        assert_eq!(
            copies_per_class(&fold_copies_into_classes(&physical)),
            vec![ids(&[0]), ids(&[1])],
            "{property}"
        );
    }
    // The same constraint on both copies does not separate them.
    let same_constraint = |candidate| ReaderConstraint::MinimumQueryReaders {
        candidate: PhysicalCandidateId(candidate),
        query_outputs: vec![vec![0], vec![1]],
        minimum: 2,
    };
    let physical = hand_built(
        vec![values_node(1.0)],
        vec![copy_of(0, &[]), copy_of(0, &[])],
        vec![same_constraint(0), same_constraint(1)],
    );
    assert_eq!(
        copies_per_class(&fold_copies_into_classes(&physical)),
        vec![ids(&[0, 1])]
    );
}

// Readers of different producer classes stay apart, and the difference
// propagates up: their readers stay apart too (the fixpoint).
#[test]
fn copies_reading_different_producer_classes_stay_apart_transitively() {
    let mut materialized_leaf = copy_of(0, &[]);
    materialized_leaf.materialization = AT_QUERY;
    let physical = hand_built(
        vec![values_node(1.0), values_node(2.0), values_node(3.0)],
        vec![
            copy_of(0, &[]),
            materialized_leaf,
            copy_of(1, &[&[0]]),
            copy_of(1, &[&[1]]),
            copy_of(2, &[&[2]]),
            copy_of(2, &[&[3]]),
        ],
        Vec::new(),
    );
    let classes = fold_copies_into_classes(&physical);
    assert_eq!(classes.classes.len(), 6);
    assert!(classes.classes.iter().all(|class| class.capacity() == 1));
}

// Each consumer copy reads its own copy of the producer: the producers fold,
// so the consumers do too.
#[test]
fn consumer_copies_reading_different_copies_of_one_class_fold() {
    let physical = hand_built(
        vec![values_node(1.0), values_node(2.0)],
        vec![
            copy_of(0, &[]),
            copy_of(0, &[]),
            copy_of(1, &[&[0]]),
            copy_of(1, &[&[1]]),
            copy_of(1, &[&[0, 1]]),
        ],
        Vec::new(),
    );
    let classes = fold_copies_into_classes(&physical);
    assert_eq!(
        copies_per_class(&classes),
        vec![ids(&[0, 1]), ids(&[2, 3, 4])]
    );
    assert_eq!(
        classes
            .class(CandidateClassId(1))
            .producer_classes_per_input,
        vec![vec![CandidateClassId(0)]]
    );
}

// A producer set that mixes two classes is a set of two producer classes;
// repeated input slots keep their own sets.
#[test]
fn producer_classes_per_input_are_sets_of_classes() {
    let mut materialized_leaf = copy_of(0, &[]);
    materialized_leaf.materialization = AT_QUERY;
    let physical = hand_built(
        vec![values_node(1.0), values_node(2.0)],
        vec![
            copy_of(0, &[]),
            materialized_leaf,
            copy_of(0, &[]),
            copy_of(1, &[&[2, 1, 0], &[0]]),
        ],
        Vec::new(),
    );
    let classes = fold_copies_into_classes(&physical);
    assert_eq!(
        copies_per_class(&classes),
        vec![ids(&[0, 2]), ids(&[1]), ids(&[3])]
    );
    assert_eq!(
        classes
            .class(CandidateClassId(2))
            .producer_classes_per_input,
        vec![
            vec![CandidateClassId(0), CandidateClassId(1)],
            vec![CandidateClassId(0)]
        ]
    );
}

// An activation requirement that the capacity cannot absorb (between two
// classes, or from a lower copy to a higher one) keeps both of its
// candidates in classes of their own.
#[test]
fn activation_requirements_capacity_cannot_absorb_isolate_their_candidates() {
    // Copies 0 and 2 are equal, copy 1 is not; Stage 2's chain 0 ← 1 ← 2
    // crosses classes, so all three stay alone.
    let mut differing = copy_of(0, &[]);
    differing.may_keep_across_evaluations_per_sole_reader_query = vec![true];
    let physical = hand_built(
        vec![values_node(1.0)],
        vec![copy_of(0, &[]), differing, copy_of(0, &[])],
        activation_chain(&[0, 1, 2]),
    );
    assert_eq!(
        copies_per_class(&fold_copies_into_classes(&physical)),
        vec![ids(&[0]), ids(&[1]), ids(&[2])]
    );
    // A lower copy requiring a higher one: the first-`j`-copies mapping
    // would break it, so both stay alone; copy 2 still has none.
    let physical = hand_built(
        vec![values_node(1.0)],
        vec![copy_of(0, &[]), copy_of(0, &[]), copy_of(0, &[])],
        vec![ReaderConstraint::ActivationRequires {
            candidate: PhysicalCandidateId(0),
            required_candidate: PhysicalCandidateId(1),
        }],
    );
    assert_eq!(
        copies_per_class(&fold_copies_into_classes(&physical)),
        vec![ids(&[0]), ids(&[1]), ids(&[2])]
    );
}

/// `summary` (a `SummaryAgg`) as a pane chain of `pane_count` one-minute
/// panes.
fn pane_chain_of(summary: &OperatorNode, pane_count: u32) -> Rc<OperatorNode> {
    let Some(ASAPOp::SummaryAgg {
        child,
        family,
        input,
        reduction,
        grouping,
        filter,
    }) = summary.asap().cloned()
    else {
        panic!("a SummaryAgg")
    };
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::PaneChain {
        child,
        family,
        input,
        reduction,
        grouping,
        filter,
        pane_width_ms: 60_000,
        pane_count,
    }))
    .unwrap()
}

// Each kind of state #1095 Step 2 lists makes a class shareable; everything
// else is stateless.
#[test]
fn classes_holding_state_are_shareable_and_the_others_stateless() {
    use asap_logical_asap_optimization::test_support::{cms_summary, metric_scan};
    let summary = Rc::new(cms_summary("count_over_time(m[5m])"));
    let pane_chain = pane_chain_of(&summary, 5);
    let scan = metric_scan(&["job"]);
    let values = values_node(1.0);
    let ingestion = ExecutionTiming::IngestionTime;
    let at_ingestion = Materialization::MatAtIngestion(Medium::Memory);
    let kept = Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory);
    let cases = [
        (
            &pane_chain,
            QUERY_TIME,
            kept,
            Some(HeldState::KeptAcrossEvaluations),
        ),
        (
            &scan,
            QUERY_TIME,
            AT_QUERY,
            Some(HeldState::MaterializedAtQuery),
        ),
        (
            &values,
            QUERY_TIME,
            AT_QUERY,
            Some(HeldState::MaterializedAtQuery),
        ),
        (
            &summary,
            ingestion,
            at_ingestion,
            Some(HeldState::MaterializedAtIngestion),
        ),
        (
            &pane_chain,
            QUERY_TIME,
            NOT_MATERIALIZED,
            Some(HeldState::PaneChain),
        ),
        (
            &summary,
            QUERY_TIME,
            NOT_MATERIALIZED,
            Some(HeldState::SummaryState),
        ),
        (
            &scan,
            QUERY_TIME,
            NOT_MATERIALIZED,
            Some(HeldState::RetainedRawData),
        ),
        (&scan, ingestion, NOT_MATERIALIZED, None),
        (&values, QUERY_TIME, NOT_MATERIALIZED, None),
        (&values, ingestion, NOT_MATERIALIZED, None),
    ];
    // Through classes: one candidate per case, each its own operator node.
    let nodes: Vec<_> = cases.iter().map(|(node, ..)| Rc::clone(node)).collect();
    let candidates = cases
        .iter()
        .enumerate()
        .map(
            |(node, &(_, timing, materialization, _))| PhysicalCandidate {
                timing,
                materialization,
                ..copy_of(node, &[])
            },
        )
        .collect();
    let classes = fold_copies_into_classes(&hand_built(nodes, candidates, Vec::new()));
    for ((node, timing, materialization, expected), class) in cases.iter().zip(&classes.classes) {
        let described = format!("{:?} {timing:?} {materialization:?}", node.operator)
            .chars()
            .take(60)
            .collect::<String>();
        assert_eq!(class.held_state, *expected, "{described}");
        assert_eq!(class.is_shareable(), expected.is_some(), "{described}");
    }
}

/// Stage 2's output for `queries` (PromQL over metric `m`, each repeating
/// every minute with a 1 % error target) through the real Stage 1 and
/// Stage 2, with raw data retained.
fn through_stages_1_and_2(queries: &[&str]) -> PhysicalASAPMemoCandidates {
    use asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::pass2::memo::candidates::try_populate_pass1;
    use asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain;
    use asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups;
    use asap_logical_asap_optimization::test_support::{
        entry_with, lower_promql, sharing_rules_and_shared_state_groups,
    };
    use asap_types::stages::LogicalASAPMemoCandidates;
    use asap_types::workload::{
        DataWorkload, DurationMs, QueryRecurrence, RepeatedDemand, RepetitionInterval,
    };
    let target = AccuracyTarget::max_error(0.01).failure_probability(0.01);
    let roots: Vec<_> = queries
        .iter()
        .enumerate()
        .map(|(index, query)| {
            (
                index,
                QueryRoot::Operator(lower_promql(query, target.clone())),
            )
        })
        .collect();
    let per_computation = generate_per_computation_candidates(roots.clone()).unwrap();
    let memo =
        try_populate_pass1(DagOfLogicalMemoGroups::from_roots(&roots), &per_computation).unwrap();
    let logical = LogicalASAPMemoCandidates {
        memo: sharing_rules_and_shared_state_groups(memo, PaneWidthDomain::Complete).unwrap(),
    };
    let every_minute =
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(60_000)));
    let entries: Vec<_> = queries
        .iter()
        .map(|_| entry_with(target.clone(), every_minute.clone()))
        .collect();
    let data = DataWorkload::sampled_every(["m".to_string()], DurationMs(1_000));
    asap_physical_asap_optimization::catalog::generate_physical_asap_memo_candidates(
        &logical, &entries, &data, true, true, None,
    )
    .unwrap()
}

// Through the real Stages 1 and 2: two queries over two windows of one
// metric read one raw scan, which Stage 2 copies once per query. The copies
// fold into one class per setting (capacity 2), each class's copies are
// exactly one Stage 2 activation chain, and every reader of the scan reads
// both scan classes.
#[test]
fn stage_2_scan_copies_fold_through_the_real_pipeline() {
    let physical = through_stages_1_and_2(&[
        "quantile_over_time(0.9, m[1m])",
        "quantile_over_time(0.9, m[5m])",
    ]);
    let classes = fold_copies_into_classes(&physical);
    assert!(classes.classes.len() < physical.candidates.len());
    let scan_classes: Vec<_> = (0..classes.classes.len())
        .map(CandidateClassId)
        .filter(|&class| {
            matches!(
                physical
                    .physical_node(classes.class(class).copies[0])
                    .operator,
                Operator::NonASAP(NonASAPOp::Scan { .. })
            )
        })
        .collect();
    let scan_states: Vec<_> = scan_classes
        .iter()
        .map(|&class| {
            let class = classes.class(class);
            assert_eq!(class.capacity(), 2);
            class.held_state
        })
        .collect();
    assert_eq!(
        scan_states,
        vec![
            Some(HeldState::RetainedRawData),
            Some(HeldState::MaterializedAtQuery)
        ]
    );
    // Every copy chain Stage 2 emitted lies inside one class.
    for constraint in &physical.reader_constraints {
        if let ReaderConstraint::ActivationRequires {
            candidate,
            required_candidate,
        } = constraint
        {
            assert_eq!(
                classes.class_of(*candidate),
                classes.class_of(*required_candidate)
            );
        }
    }
    // Copies are interchangeable: every member of a class has the class's
    // producer classes.
    for class in &classes.classes {
        for &copy in &class.copies {
            let producer_classes: Vec<Vec<_>> = physical.candidates[copy.0]
                .producer_candidates_per_input
                .iter()
                .map(|producers| {
                    let mut classes: Vec<_> = producers
                        .iter()
                        .map(|&producer| classes.class_of(producer))
                        .collect();
                    classes.sort_unstable();
                    classes.dedup();
                    classes
                })
                .collect();
            assert_eq!(producer_classes, class.producer_classes_per_input);
        }
    }
    let readers_of_scans: Vec<_> = classes
        .classes
        .iter()
        .filter(|class| {
            class
                .producer_classes_per_input
                .iter()
                .any(|input| input.iter().any(|class| scan_classes.contains(class)))
        })
        .collect();
    assert!(!readers_of_scans.is_empty());
    for reader in readers_of_scans {
        assert_eq!(
            reader.producer_classes_per_input,
            vec![scan_classes.clone()]
        );
    }
}

// Folding depends only on the input: two runs (each with its own hash
// seeds) give the same classes, numbered by smallest copy.
#[test]
fn folding_is_deterministic() {
    let physical = through_stages_1_and_2(&[
        "quantile_over_time(0.9, m[1m])",
        "quantile_over_time(0.9, m[5m])",
        "sum_over_time(m[5m])",
    ]);
    let first = fold_copies_into_classes(&physical);
    for _ in 0..3 {
        assert_eq!(fold_copies_into_classes(&physical), first);
    }
    let smallest_copies: Vec<_> = first.classes.iter().map(|class| class.copies[0]).collect();
    assert!(smallest_copies.windows(2).all(|pair| pair[0] < pair[1]));
    for class in &first.classes {
        assert!(class.copies.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
