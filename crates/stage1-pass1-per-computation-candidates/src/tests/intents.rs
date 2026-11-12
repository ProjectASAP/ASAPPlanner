use super::topk;
use crate::computation_targets::{ways_to_compute_intent, HowToComputeAggregateMeasure};
use asap_types::ir::operator::AggIntent;
use asap_types::summary_type::{ExactKind, SketchAlgorithm};

fn algorithms(choices: &[HowToComputeAggregateMeasure]) -> Vec<SketchAlgorithm> {
    choices
        .iter()
        .filter_map(|choice| match choice {
            HowToComputeAggregateMeasure::Summary(algorithm) => Some(*algorithm),
            _ => None,
        })
        .collect()
}

/// Example 2 preserves specialized distinct summaries and the universal
/// alternative; plain exact execution is the group's own expression.
#[test]
fn cardinality_keeps_specialized_and_universal_summaries() {
    let choices = ways_to_compute_intent(&AggIntent::Cardinality { cols: vec![0] });
    assert!(choices
        .iter()
        .all(|choice| matches!(choice, HowToComputeAggregateMeasure::Summary(_))));
    assert_eq!(
        algorithms(&choices),
        vec![
            SketchAlgorithm::Hll,
            SketchAlgorithm::Kmv,
            SketchAlgorithm::UnivMon
        ]
    );
    let tuple = ways_to_compute_intent(&AggIntent::Cardinality { cols: vec![0, 1] });
    assert!(!algorithms(&tuple).contains(&SketchAlgorithm::UnivMon));
}

/// Frequency moments offer a universal sketch without certification.
#[test]
fn frequency_statistics_keep_universal_choices() {
    for intent in [
        AggIntent::FrequencyL2 { col: Some(0) },
        AggIntent::FrequencyEntropy { col: Some(0) },
    ] {
        let choices = ways_to_compute_intent(&intent);
        assert_eq!(
            choices,
            [HowToComputeAggregateMeasure::Summary(
                SketchAlgorithm::UnivMon
            )]
        );
    }
}

/// TopK keeps both declared heap substrates without choosing an implementation.
#[test]
fn topk_keeps_both_specialized_heap_choices() {
    let choices = ways_to_compute_intent(&topk(10));
    assert_eq!(
        algorithms(&choices),
        vec![
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap
        ]
    );
}

/// Approximate count lists its exact accumulator first, beside the
/// summaries (the plain aggregate is the group's own expression).
#[test]
fn approximate_count_lists_its_accumulator_and_summaries() {
    let choices = ways_to_compute_intent(&AggIntent::Count);
    assert_eq!(
        choices[0],
        HowToComputeAggregateMeasure::ExactAccumulator(vec![ExactKind::Count])
    );
    assert!(choices.contains(&HowToComputeAggregateMeasure::Summary(
        SketchAlgorithm::UnivMon
    )));
}

/// Count Sketch's signed counters give no total, so Pass 1 offers no Count
/// Sketch for a count.
#[test]
fn a_count_offers_no_count_sketch() {
    assert!(!ways_to_compute_intent(&AggIntent::Count).contains(
        &HowToComputeAggregateMeasure::Summary(SketchAlgorithm::CountSketch)
    ));
}
