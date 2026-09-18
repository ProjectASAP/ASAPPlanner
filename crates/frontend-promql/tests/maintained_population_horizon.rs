mod support;
use asap_aware_mapping::maintained_population::MaintainedPopulationStrategy;
use asap_types::post_asap::maintained_population::PopulationInput;
use asap_types::post_asap::{SummaryExpr, ValueOperation};
use asap_types::types::AccuracyTarget;
use std::rc::Rc;

// A population for a one-second selector must expire members after one second.
#[test]
fn population_preserves_selector_horizon() {
    let root = Rc::new(support::lower_promql("sum(a)", AccuracyTarget::Exact).unwrap());
    let candidate = MaintainedPopulationStrategy::new(std::slice::from_ref(&root))
        .candidate(&root)
        .unwrap();
    let SummaryExpr::ValueOperation { child, .. } = &candidate.expr else {
        panic!()
    };
    let SummaryExpr::ValueOperation {
        operation: ValueOperation::MaintainPopulation { population },
        ..
    } = &child.expr
    else {
        panic!()
    };
    let PopulationInput::CurrentSeries(spec) = &population.input else {
        panic!()
    };
    assert_eq!(spec.lookback_ms, 1_000);
    asap_types::post_asap::compile_executable_dag(&candidate).unwrap();
    let asap_types::pre_asap::QueryExpr::Aggregate { child: source, .. } = root.as_ref() else {
        panic!()
    };
    assert!(spec.matches_input(source));
    let mut wrong = spec.clone();
    wrong.lookback_ms = 300_000;
    assert!(!wrong.matches_input(source));
}
