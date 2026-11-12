use super::*;
use crate::summary_kernels::weighted_frequency::{FrequencyAlgorithm, WeightedFrequency};
use planner_types::summary_type::SketchAlgorithm;

// A state cannot acquire a different family merely by relabeling its
// batch; states of different shapes (θ) do not merge.
#[test]
fn weighted_state_family_must_match_and_shapes_must_merge() {
    let cms = SummaryFamilyType::Sketch(SketchAlgorithm::CmsWithHeap);
    let cs = SummaryFamilyType::Sketch(SketchAlgorithm::CountSketchWithHeap);
    let state = WeightedFrequency::new(FrequencyAlgorithm::CountSketch, 32, 5, 8).unwrap();
    assert!(validate_state(&cs, &state).is_ok());
    assert!(validate_state(&cms, &state).is_err());
    // A Count Sketch without a heap is not a heap family's state.
    let no_heap = WeightedFrequency::new(FrequencyAlgorithm::CountSketch, 32, 5, 0).unwrap();
    assert!(validate_state(&cs, &no_heap).is_err());
    let wrong_shape = WeightedFrequency::new(FrequencyAlgorithm::CountSketch, 64, 5, 8).unwrap();
    assert!(validate_state(&cs, &wrong_shape).is_ok());
    assert!(state.merge(&wrong_shape).is_err());
}
