use super::*;
use crate::ir::operator::arguments::Reduction;
use crate::ir::scalar::expr_ir::ColumnRef;
use crate::ir::ASAPOp;
use crate::summary_type::{ExactKind, SummaryUpdate};

/// `SummaryAgg` over group 0 building a state of `summary_type`.
fn summary_agg(summary_type: FieldDataType) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryAgg {
        child: MemoGroupId(0),
        summary_type,
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
    })
}

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
        summary_type_of_operator(&summary_agg(kll.clone())),
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
        listed_build_parameters(&summary_agg(FieldDataType::Sketch(kll))),
        parameter_list(kll)
            .iter()
            .cloned()
            .map(Some)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        listed_build_parameters(&summary_agg(FieldDataType::exact(ExactKind::Sum))),
        vec![None]
    );
}
