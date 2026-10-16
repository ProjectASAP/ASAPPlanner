//! The deployment's capabilities (#509's execution-capability input) over
//! the planner-layering examples: the summaries the executor supports
//! restrict Stage 1 (Pass 1, and so Pass 2).

mod planner_layering_common;

use asap_aware_mapping::stage3_plan_selection::PlanSelectionSettings;
use asap_aware_mapping::{DeploymentCapabilities, SupportedSummary};
use asap_devtools::planning_stages::lower_sql_roots;
use asap_types::ir::flat::FlatDag;
use asap_types::post_asap::{SketchAlgorithm, StatisticKind};
use planner_layering_common::*;

/// A deployment whose executor supports only `supported`: each family with
/// its readouts.
fn supporting(supported: &[(SketchAlgorithm, &[StatisticKind])]) -> DeploymentCapabilities {
    DeploymentCapabilities {
        supported_summaries: Some(
            supported
                .iter()
                .map(|(family, readouts)| SupportedSummary {
                    family: family.clone(),
                    readouts: readouts.to_vec(),
                })
                .collect(),
        ),
        ..DeploymentCapabilities::default()
    }
}

/// Every (family, readout) a logical candidate's DAG uses: each sketch
/// build with each statistic estimated from it.
fn summaries_used(dag: &FlatDag) -> Vec<(SketchAlgorithm, StatisticKind)> {
    sketch_builds(dag)
        .into_iter()
        .flat_map(|(build, family, _)| {
            estimates_of(dag, build)
                .into_iter()
                .map(move |(_, statistic)| (family.clone(), statistic.kind()))
        })
        .collect()
}

/// Example 2 (three SQL flow statistics) planned for `deployment`.
fn example2_for(deployment: &DeploymentCapabilities) -> Run {
    let (workload, catalog) = example2();
    let roots = lower_sql_roots(&workload, &catalog).expect("example 2 lowers");
    run_stages_with(
        &workload,
        roots,
        &PlanSelectionSettings::default(),
        deployment,
    )
}

/// Restricting the supported summaries removes exactly the Stage 1
/// candidates (Pass 1's and Pass 2's) that build an unsupported family or
/// read an unsupported readout from one, and keeps the others in order:
///
/// * Example 1 without Count-Min heaps: the top-k keeps its CountSketch
///   heap;
/// * Example 2 with UnivMon for distinct counts and L2 only (no entropy),
///   and HLL: KMV and UnivMon's entropy go, so the entropy is exact, and
///   the shared UnivMon builds for the distinct count and L2 stay (Pass
///   2's summary-capability rule);
/// * Example 3's Pattern B with KLL only: DDSketch goes, whole-window and
///   tumbling panes alike (Pass 2's window-composition rule).
#[test]
fn restricting_summaries_removes_exactly_the_candidates_that_use_them() {
    let all = DeploymentCapabilities::default();
    let heap = supporting(&[(SketchAlgorithm::CountSketchWithHeap, &[StatisticKind::TopK])]);
    assert_removes_exactly(
        "example 1",
        &run_promql_for(&example1(), &all),
        &run_promql_for(&example1(), &heap),
        &heap,
    );
    let distinct_and_l2 = supporting(&[
        (
            SketchAlgorithm::UnivMon,
            &[StatisticKind::Distinct, StatisticKind::FrequencyL2],
        ),
        (SketchAlgorithm::Hll, &[StatisticKind::Distinct]),
    ]);
    assert_removes_exactly(
        "example 2",
        &example2_for(&all),
        &example2_for(&distinct_and_l2),
        &distinct_and_l2,
    );
    let kll = supporting(&[(SketchAlgorithm::Kll, &[StatisticKind::Quantile])]);
    assert_removes_exactly(
        "pattern B",
        &run_promql_for(&pattern_b(), &all),
        &run_promql_for(&pattern_b(), &kll),
        &kll,
    );
}

/// `restricted` (planned for `deployment`) has exactly `unrestricted`'s
/// Stage 1 candidates whose every summary `deployment` supports, in order;
/// some are removed, a supported summary stays, and the cheapest valid
/// candidate is selected.
fn assert_removes_exactly(
    name: &str,
    unrestricted: &Run,
    restricted: &Run,
    deployment: &DeploymentCapabilities,
) {
    let expected: Vec<&FlatDag> = unrestricted
        .logical
        .iter()
        .map(|c| &c.dag)
        .filter(|dag| {
            summaries_used(dag)
                .iter()
                .all(|(family, readout)| deployment.supports(family, *readout))
        })
        .collect();
    let found: Vec<&FlatDag> = restricted.logical.iter().map(|c| &c.dag).collect();
    assert!(
        expected.len() < unrestricted.logical.len(),
        "{name}: the restriction removes some candidate"
    );
    assert!(
        found.iter().any(|dag| !summaries_used(dag).is_empty()),
        "{name}: a supported summary stays"
    );
    assert_eq!(found, expected, "{name}");
    assert_selects_cheapest_valid(restricted);
}
