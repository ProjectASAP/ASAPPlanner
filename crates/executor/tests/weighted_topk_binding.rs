//! Planner output binds directly to the shared runtime over raw counter samples.
use planner_types::ir::schema::*;
use planner_types::summary_type::*;

#[test]
fn rate_updates_cannot_enter_integer_heap_factory() {
    let family = FieldDataType::Sketch(SketchAlgorithm::CmsWithHeap);
    let input = SummaryUpdate {
        item: Some(SummaryInputExpr::Column(
            planner_types::ir::scalar::ColumnRef::Named("service".into()),
        )),
        weight: SummaryInputExpr::Column(planner_types::ir::scalar::ColumnRef::SampleValue),
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::ResetAwareCounterDerivative,
        },
    };
    assert!(asap_executor::factory::create_planner_accumulator(&family, None, &input).is_err());
}
