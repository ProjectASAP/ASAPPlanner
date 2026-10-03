mod support;
use asap_aware_mapping::maintained_population::MaintainedPopulationStrategy;
use asap_types::ir::operator::maintained_population::PopulationInput;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::types::AccuracyTarget;

// A population for a one-second selector must expire members after one second.
#[test]
fn population_preserves_selector_horizon() {
    let root = support::lower_promql("sum(a)", AccuracyTarget::Exact).unwrap();
    let candidate = MaintainedPopulationStrategy::new(std::slice::from_ref(&root))
        .candidate(&root)
        .unwrap();
    // The evaluation sits over the maintained population.
    let Operator::ASAP(ASAPOp::EvaluatePopulation { child, .. }) = &candidate.operator else {
        panic!()
    };
    let Operator::ASAP(ASAPOp::MaintainPopulation { population, .. }) = &child.operator else {
        panic!()
    };
    let PopulationInput::CurrentSeries(spec) = &population.input else {
        panic!()
    };
    assert_eq!(spec.lookback_ms, 1_000);
    support::post_asap_dag(&candidate);
    let NonASAPOp::Aggregate { child: source, .. } = root.expect_non_asap() else {
        panic!()
    };
    assert!(spec.matches_node(source));
    let mut wrong = spec.clone();
    wrong.lookback_ms = 300_000;
    assert!(!wrong.matches_node(source));
}
