use super::*;
use crate::ir::operator::arguments::Reduction;
use crate::ir::scalar::expr_ir::ColumnRef;
use crate::ir::ASAPOp;
use crate::stages::Memo;
use crate::summary_type::{ExactKind, SummaryUpdate};
use crate::test_support::memo_builders::scan_group;
use crate::test_support::memo_fixtures::state;
use crate::workload::DataWorkload;

/// `summary_type_of_operator` reads the summary type of a `SummaryAgg`
/// and of a chain, and of no other operator.
#[test]
fn summary_type_of_operator_reads_the_builds_summary_type() {
    let kll = FieldDataType::Sketch(SketchAlgorithm::KLL);
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
/// without θ, and a Count-Min build over weights not proven non-negative
/// none (a `CountMinSketch` over the sample value of a gauge with no declared sign).
#[test]
fn listed_build_parameters_lists_a_sketch_builds_parameters() {
    let mut memo = Memo::new(&DataWorkload::default());
    let scan = scan_group(&mut memo, "m", &[], vec![]);
    let mut listed = |summary_type: FieldDataType| {
        let build = state(summary_type, scan);
        let group = memo.find_or_add_group(build.clone()).unwrap();
        listed_build_parameters(&build, &memo.groups()[group.0 as usize].group_properties)
    };
    let kll = SketchAlgorithm::KLL;
    assert_eq!(
        listed(FieldDataType::Sketch(kll)),
        parameter_list(kll)
            .iter()
            .cloned()
            .map(Some)
            .collect::<Vec<_>>()
    );
    assert_eq!(listed(FieldDataType::exact(ExactKind::Sum)), vec![None]);
    assert_eq!(
        listed(FieldDataType::Sketch(SketchAlgorithm::CountMinSketch)),
        vec![]
    );
}
