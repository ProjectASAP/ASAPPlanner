//! Physical edges: each logical edge expanded between the two candidates'
//! alternatives.
use asap_types::ir::properties::ExecutionTiming::IngestionTime;
use asap_types::stages::{Lifetime, PhysicalCandidateId};

use super::generate;
use crate::test_fixtures::{example, ingesting, retaining_everything};

/// Each logical edge becomes an edge between every pair of the two
/// candidates' emitted alternatives, with $\alpha^{\log}$ copied and
/// $\alpha^{\mathrm{phys}}$ from the producer's timing and lifetime and the
/// consumer's timing: the chain's four alternatives read the scan's five
/// (20 edges); a query-time chain reads the query-time scan and the scan
/// kept across evaluations at ingestion time, and the ingestion-time chain
/// only the ingestion-time scan.
#[test]
fn edges_carry_both_requirements() {
    let example = example();
    let physical = generate(&example.logical, &ingesting(None), &retaining_everything());
    let candidate = |id: PhysicalCandidateId| physical.physical_candidates[id.0];
    let into_chain: Vec<_> = physical
        .edges()
        .map(|(_, edge)| edge)
        .filter(|edge| candidate(edge.consumer_candidate_id).logical_candidate_id == example.chain)
        .collect();
    assert_eq!(into_chain.len(), 20);
    for edge in into_chain {
        let producer = candidate(edge.producer_candidate_id).physical_properties;
        let consumer = candidate(edge.consumer_candidate_id).physical_properties;
        assert!(edge.satisfies_logical_requirement);
        assert_eq!(
            edge.satisfies_physical_requirement,
            producer.timing() == consumer.timing()
                || (producer.timing() == IngestionTime
                    && matches!(producer.lifetime(), Lifetime::AcrossEvaluations(_))),
            "{producer:?} -> {consumer:?}"
        );
    }
}
