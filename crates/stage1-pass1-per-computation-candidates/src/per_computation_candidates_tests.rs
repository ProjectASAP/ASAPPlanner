//! Frontend-to-Pass-1 acceptance: candidate discovery precedes empirical selection.
use crate::per_computation_candidates::computation_candidates_for_intent;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::schema::SketchAlgorithm;

use asap_types::stages::HowToComputeAggregateMeasure;

fn approximate() -> AccuracyTarget {
    AccuracyTarget::max_error(0.05).failure_probability(0.01)
}
fn candidates_for(intent: AggIntent) -> Vec<HowToComputeAggregateMeasure> {
    computation_candidates_for_intent(&intent)
        .unwrap()
        .candidates
}
fn algorithms(choices: &[HowToComputeAggregateMeasure]) -> Vec<SketchAlgorithm> {
    choices
        .iter()
        .filter_map(|choice| match choice {
            HowToComputeAggregateMeasure::Summary(algorithm) => Some(*algorithm),
            _ => None,
        })
        .collect()
}

/// Example 2 preserves specialized distinct summaries and the universal alternative.
#[test]
fn cardinality_keeps_exact_specialized_and_universal_candidates() {
    let choices = candidates_for(AggIntent::Cardinality {
        cols: vec![0],
        accuracy: approximate(),
    });
    assert_eq!(choices[0], HowToComputeAggregateMeasure::Exact);
    assert_eq!(
        algorithms(&choices),
        vec![
            SketchAlgorithm::Hll,
            SketchAlgorithm::Kmv,
            SketchAlgorithm::UnivMon
        ]
    );
    let tuple = candidates_for(AggIntent::Cardinality {
        cols: vec![0, 1],
        accuracy: approximate(),
    });
    assert!(!algorithms(&tuple).contains(&SketchAlgorithm::UnivMon));
}

/// Frequency moments retain exact execution and a universal sketch without certification.
#[test]
fn frequency_statistics_keep_universal_choices() {
    for intent in [
        AggIntent::FrequencyL2 {
            col: Some(0),
            accuracy: approximate(),
        },
        AggIntent::FrequencyEntropy {
            col: Some(0),
            accuracy: approximate(),
        },
    ] {
        let choices = candidates_for(intent);
        assert_eq!(choices[0], HowToComputeAggregateMeasure::Exact);
        assert_eq!(algorithms(&choices), vec![SketchAlgorithm::UnivMon]);
    }
}

/// TopK keeps both declared heap substrates without choosing an implementation.
#[test]
fn topk_keeps_both_specialized_heap_choices() {
    let choices = candidates_for(AggIntent::TopK {
        k: 10,
        accuracy: approximate(),
    });
    assert_eq!(choices[0], HowToComputeAggregateMeasure::Exact);
    assert_eq!(
        algorithms(&choices),
        vec![
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap
        ]
    );
}

/// Summary candidates are unsized, so they do not depend on the requested error.
#[test]
fn summary_candidates_are_unsized() {
    let loose = candidates_for(AggIntent::Count {
        accuracy: approximate(),
    });
    let tight = candidates_for(AggIntent::Count {
        accuracy: AccuracyTarget::max_error(0.0001).failure_probability(0.0001),
    });
    assert_eq!(loose, tight);
}
