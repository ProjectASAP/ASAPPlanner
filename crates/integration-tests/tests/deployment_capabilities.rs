//! The deployment's capabilities (#509's execution-capability input) over
//! the planner-layering examples: the summaries the executor supports
//! restrict Stage 1 (Pass 1, and so Pass 2).

mod planner_layering_common;

use asap_devtools::planning_stages::lower_sql_roots;
use asap_logical_asap_optimization::{DeploymentCapabilities, SupportedSummary};
use asap_plan_selection::PlanSelectionSettings;
use asap_types::ir::flat::FlatDag;
use asap_types::ir::schema::{SketchAlgorithm, StatisticKind};
use asap_types::ir::{ASAPOp, Operator};
use asap_types::physical::Materialization;
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

/// Pattern B with a p50 panel beside its p99, both every minute over the
/// same 5-min window. One 1-min tumbling KLL chain serves both panels.
fn pattern_b_with_p50() -> asap_types::workload::PlanningWorkload {
    let mut workload = pattern_b();
    let repeating = workload
        .query_workload
        .repeating_queries
        .as_mut()
        .expect("pattern B repeats");
    let mut p50 = repeating[0].clone();
    p50.query = asap_types::workload::Query("quantile_over_time(0.5, latency_ms[5m])".into());
    repeating.push(p50);
    workload
}

/// Physical candidate ids with their cost per second.
type CandidateCosts = Vec<(String, f64)>;

/// The per-second cost of every physical candidate whose pane chain both
/// panels read is materialized at query time: shared within one evaluation
/// (`MatAtQuery`), and kept across evaluations
/// (`MatAtQueryKeptAcrossEvaluations`, which Stage 3 prices as such).
fn shared_chain_costs(run: &Run) -> (CandidateCosts, CandidateCosts) {
    let (mut within_one, mut kept) = (Vec::new(), Vec::new());
    for p in &run.physical {
        for n in &p.dag().nodes {
            let shared_chain = matches!(n.payload, Operator::ASAP(ASAPOp::PaneChain { .. }))
                && readers(p.dag(), &p.query_roots, n.id).len() == 2;
            let Some(cost) = run.selection.costs.get(&p.id).filter(|_| shared_chain) else {
                continue;
            };
            let entry = (p.id.clone(), cost.cost_per_second.expect("cost model 1"));
            let kept_detail = cost.per_node[&n.id].detail.contains("across evaluations");
            match n.materialization {
                Materialization::MatAtQuery(_) => within_one.push(entry),
                Materialization::MatAtQueryKeptAcrossEvaluations(_) if kept_detail => {
                    kept.push(entry)
                }
                Materialization::MatAtQueryKeptAcrossEvaluations(_) => {
                    panic!("{}: a kept chain priced as not kept", p.id)
                }
                _ => {}
            }
        }
    }
    (within_one, kept)
}

/// With query-time results retained, a chain two panels read is offered
/// both ways: shared within one evaluation (rebuilt from the 5-min window
/// every minute, then reused by both panels) and kept across evaluations
/// (B3: one new pane per evaluation, the 4 older panes held in memory
/// between evaluations). Holding 1M per-series KLLs between evaluations
/// costs far more than rebuilding them, so the shared option is cheaper
/// and the kept one is not selected. Without query-time retention, the
/// kept option is not offered.
#[test]
fn query_time_results_retained_still_offers_the_chain_shared_within_one_evaluation() {
    let keeping = DeploymentCapabilities {
        query_time_results_retained: true,
        ..DeploymentCapabilities::default()
    };
    let run = run_promql_for(&pattern_b_with_p50(), &keeping);
    let (within_one, kept) = shared_chain_costs(&run);
    assert!(!within_one.is_empty(), "no shared option: kept {kept:?}");
    assert!(!kept.is_empty(), "no kept option: shared {within_one:?}");
    let cheapest = |options: &[(String, f64)]| {
        options
            .iter()
            .map(|(_, cost)| *cost)
            .fold(f64::INFINITY, f64::min)
    };
    assert!(
        cheapest(&within_one) < cheapest(&kept),
        "shared {within_one:?}, kept {kept:?}"
    );
    assert!(
        kept.iter().all(|(id, _)| *id != run.selection.selected),
        "{}",
        run.selection.selected
    );
    assert_selects_cheapest_valid(&run);

    let (_, kept) = shared_chain_costs(&run_promql_for(
        &pattern_b_with_p50(),
        &DeploymentCapabilities::default(),
    ));
    assert!(kept.is_empty(), "{kept:?}");
}
