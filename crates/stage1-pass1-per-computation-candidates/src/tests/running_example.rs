use super::{memo_printout, quantile, selector, state_group};
use crate::pass1_memo;
use crate::test_fixtures::aggregate_group;
use asap_types::ir::operator::arguments::{BinaryOpKind, Reduction};
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::{ASAPOp, BinaryOperator, NonASAPOp, Operator, TimeRangeKind};
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::summary_type::{parameter_list, SketchAlgorithm, SketchParams, SketchStatistic};
use asap_types::test_support::memo_builders::{candidates_of, group, query};
use asap_types::workload::DataWorkload;

/// The doc's running example: query A reads p95 of `latency`, query B
/// reads p99 / p50 of it. KLL state group holds one build per θ of KLL's
/// list, DDSketch's state group likewise; the p95, p99 and p50 groups each
/// read either state through their own readout groups. The metric rule
/// (B's relative ratio cannot read KLL's rank error) drops edges in Pass 2,
/// so Pass 1 offers both algorithms to every quantile group.
#[test]
fn running_example_gives_one_build_per_theta_and_separate_readouts() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = selector(&mut memo, TimeRangeKind::Instant, "latency", &[]);
    let g3 = aggregate_group(
        &mut memo,
        g1,
        Reduction::by(vec![]),
        vec![quantile(0.95)],
        None,
    );
    let g4 = aggregate_group(
        &mut memo,
        g1,
        Reduction::by(vec![]),
        vec![quantile(0.99)],
        None,
    );
    let g5 = aggregate_group(
        &mut memo,
        g1,
        Reduction::by(vec![]),
        vec![quantile(0.5)],
        None,
    );
    let g6 = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                checked_relative_division: false,
                checked_finite_division: false,
                kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
                vector_match: None,
            },
            return_bool: false,
            lhs: g4,
            rhs: g5,
        }),
    );
    query(&mut memo, g3, None);
    query(&mut memo, g6, None);
    let memo = pass1_memo(memo, None).unwrap();
    println!("{}", memo_printout(&memo));
    let (kll, _) = state_group(&memo, SketchAlgorithm::Kll);
    let (ddsketch, _) = state_group(&memo, SketchAlgorithm::DDSketch);
    for (state, algorithm) in [
        (kll, SketchAlgorithm::Kll),
        (ddsketch, SketchAlgorithm::DDSketch),
    ] {
        let thetas: Vec<SketchParams> = candidates_of(&memo, state)
            .iter()
            .map(|candidate| candidate.logical_properties.parameters().unwrap().clone())
            .collect();
        assert_eq!(thetas, parameter_list(algorithm));
        // θ is a property, never an operator argument: every build has the
        // group's own expression.
        assert!(candidates_of(&memo, state).iter().all(|candidate| {
            candidate.expression.operator == memo.groups()[state.0 as usize].expression
        }));
    }
    // Each quantile group: its exact aggregate and one readout per algorithm,
    // each through a readout group of its own over the state group.
    for (result, q) in [(g3, 0.95), (g4, 0.99), (g5, 0.5)] {
        let readouts: Vec<MemoGroupId> = candidates_of(&memo, result)[1..]
            .iter()
            .map(|candidate| *candidate.expression.operator.children()[0])
            .collect();
        assert_eq!(readouts.len(), 2, "G{}", result.0);
        for (readout, state) in readouts.iter().zip([kll, ddsketch]) {
            assert_eq!(
                memo.groups()[readout.0 as usize].expression,
                Operator::ASAP(ASAPOp::SummaryEstimate {
                    summary_input: state,
                    query: SketchStatistic::Quantile { q },
                })
            );
            assert!(candidates_of(&memo, *readout)[0]
                .logical_properties
                .parameters()
                .is_none());
        }
    }
    // The ratio's group keeps its one candidate, its own expression.
    assert_eq!(candidates_of(&memo, g6).len(), 1);
}
