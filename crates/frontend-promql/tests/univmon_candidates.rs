use std::rc::Rc;

use crate::planner_support::run_all_stages;
use crate::support;

use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
use asap_types::ir::{ASAPOp, Operator, OperatorNode};
use support::{logical_candidates, lower_promql};

/// The summary evaluation over a UnivMon state that `root` reads, if any.
fn univmon_estimate(root: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
    OperatorNode::reachable(root).into_iter().find(|node| {
        let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &node.operator else {
            return false;
        };
        matches!(&summary_input.operator, Operator::ASAP(ASAPOp::SummaryAgg { summary_type: FieldDataType::Sketch(kind), .. })
            if kind == &SketchAlgorithm::UnivMon)
    })
}

#[test]
fn uncalibrated_frequency_evaluations_do_not_bypass_accuracy_targets() {
    // An unmeasured heuristic remains inspectable but is never certified or
    // automatically selected for a caller-visible bounded-error result.
    // L2 is certified; entropy is not.
    for query in ["entropy_over_time(m[5m])"] {
        for target in [
            AccuracyTarget::exact(),
            AccuracyTarget::max_error(0.02),
            AccuracyTarget::max_error(0.02).failure_probability(0.01),
        ] {
            let root = lower_promql(query, target.clone()).unwrap();
            let generated = logical_candidates(Rc::clone(&root));
            let run = run_all_stages(root, target.clone());
            let unknown: Vec<_> = generated
                .iter()
                .filter(|node| univmon_estimate(node).is_some() && node.guarantee.is_none())
                .collect();
            if target.is_exact() {
                assert!(unknown.is_empty(), "{query}");
            } else {
                assert!(!unknown.is_empty(), "{query}");
                let (plan, _) = run
                    .selection
                    .as_ref()
                    .expect("a certified plan is selected");
                let roots = plan.query_roots().unwrap();
                let [asap_types::ir::QueryRoot::Operator(selected)] = roots.as_slice() else {
                    panic!("one operator result");
                };
                assert!(selected.guarantee.is_some(), "{query}");
            }
        }
    }
}
