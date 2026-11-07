//! Planner output binds directly to the shared runtime over raw counter samples.
use planner_types::ir::schema::*;

// The untyped heap updater must not silently round a Planner rate update.
#[test]
fn rate_updates_cannot_enter_integer_heap_factory() {
    let family = FieldDataType::Sketch(SketchAlgorithm::CmsWithHeap, Default::default());
    let input = SummaryUpdate {
        item: Some(SummaryInputExpr::Column(
            planner_types::ir::scalar::ColumnRef::Named("service".into()),
        )),
        weight: SummaryInputExpr::Column(planner_types::ir::scalar::ColumnRef::SampleValue),
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::ResetAwareCounterDerivative,
        },
    };
    assert!(asap_executor::factory::create_planner_accumulator(
        &family,
        Some(&SketchParams::CmsWithHeap {
            width: 272,
            depth: 5,
            heap_size: 100,
        }),
        &input,
        &Default::default()
    )
    .is_err());
}
