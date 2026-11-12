use super::*;
use crate::ir::operator::arguments::Reduction;
use crate::ir::scalar::expr_ir::ColumnRef;
use crate::ir::ASAPOp;
use crate::summary_type::{ExactKind, SummaryUpdate};
use crate::test_support::memo_fixtures::state;

/// `summary_type_of_operator` reads the summary type of a `SummaryAgg`
/// and of a chain, and of no other operator.
#[test]
fn summary_type_of_operator_reads_the_builds_summary_type() {
    let kll = FieldDataType::Sketch(SketchAlgorithm::Kll);
    let chain = Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: MemoGroupId(0),
        summary_type: kll.clone(),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
        tumbling_window_width_ms: 5_000,
    });
    let merge = Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![MemoGroupId(0)],
        group_by: None,
    });
    assert_eq!(
        summary_type_of_operator(&state(kll.clone(), MemoGroupId(0))),
        Some(&kll)
    );
    assert_eq!(summary_type_of_operator(&chain), Some(&kll));
    assert_eq!(summary_type_of_operator(&merge), None);
}

/// A sketch build has one entry per listed θ, an exact build one
/// without θ.
#[test]
fn listed_build_parameters_lists_a_sketch_builds_parameters() {
    let kll = SketchAlgorithm::Kll;
    assert_eq!(
        listed_build_parameters(&state(FieldDataType::Sketch(kll), MemoGroupId(0))),
        parameter_list(kll)
            .iter()
            .cloned()
            .map(Some)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        listed_build_parameters(&state(FieldDataType::exact(ExactKind::Sum), MemoGroupId(0))),
        vec![None]
    );
}
