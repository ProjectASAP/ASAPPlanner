//! Pruning after expansion: alternatives that cannot be built or that no
//! query output reads, and logical candidates Pass 2 eliminated.
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::ScalarValue;
use asap_types::ir::{ExprSemantics, QueryRoot, ScalarExpr};
use asap_types::stages::{PruneCause, PruneReason, PrunedLogicalCandidate, RootPruneReason};

use super::{generate, offered, physical_properties_list, pruned, pruned_with};
use crate::test_fixtures::{
    example, example_read_by, ingesting, retaining_everything, QT_ACROSS, QT_NONE, QT_ONE,
};

/// A logical edge with $\alpha^{\log}$ false is kept but not admissible:
/// a readout whose only producer fails it has no buildable producer, and
/// what only it reads reaches no query output.
#[test]
fn an_input_without_an_admissible_producer_is_not_buildable() {
    let mut example = example();
    for logical_edge in &mut example.logical.edges {
        if logical_edge.consumer_candidate_id == example.readout {
            logical_edge.satisfies_logical_requirement = false;
        }
    }
    let physical = generate(&example.logical, &ingesting(None), &retaining_everything());
    assert!(physical.physical_candidates.is_empty());
    assert_eq!(
        pruned(&physical, example.readout),
        pruned_with(
            &[QT_NONE, QT_ONE, QT_ACROSS],
            PruneReason::InputHasNoBuildableProducer
        )
    );
    assert_eq!(
        pruned(&physical, example.build),
        pruned_with(
            &[QT_NONE, QT_ONE, QT_ACROSS],
            PruneReason::ReachesNoQueryOutput
        )
    );
}

/// A candidate Pass 2 eliminated is not expanded: the build over the last
/// minute, here eliminated with its input, leaves the merge as the
/// readout's only producer.
#[test]
fn candidates_pass_2_eliminated_are_not_expanded() {
    let mut example = example();
    let (build, last_minute) = (example.build, example.last_minute);
    example.logical.edges.retain(|edge| {
        ![edge.producer_candidate_id, edge.consumer_candidate_id].contains(&build)
            && edge.consumer_candidate_id != last_minute
    });
    example.logical.pruned = [build, last_minute]
        .map(|candidate| PrunedLogicalCandidate {
            candidate,
            causes: vec![PruneCause {
                through_pruned_candidates: vec![],
                root: RootPruneReason::NoConsumer,
            }],
        })
        .to_vec();
    let physical = generate(&example.logical, &ingesting(None), &retaining_everything());
    for candidate in [build, last_minute] {
        assert_eq!(offered(&physical, candidate), vec![]);
        assert_eq!(pruned(&physical, candidate), vec![]);
    }
    assert_eq!(
        offered(&physical, example.readout),
        physical_properties_list(&[QT_NONE, QT_ONE, QT_ACROSS])
    );
}

/// A scalar query's operand is a query output too: `latency_p95 * 2` over
/// the readout keeps the readout's query-time alternatives.
#[test]
fn a_scalar_roots_operand_is_a_query_output() {
    let example = example_read_by(|readout| {
        QueryRoot::Scalar(ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Mul,
            left: Box::new(ScalarExpr::PromqlScalarFromVector(readout)),
            right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(2.0))),
            semantics: ExprSemantics::Promql,
        })
    });
    let physical = generate(&example.logical, &ingesting(None), &retaining_everything());
    assert_eq!(
        offered(&physical, example.readout),
        physical_properties_list(&[QT_NONE, QT_ONE, QT_ACROSS])
    );
}
