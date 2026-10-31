//! The deployment's capabilities (#509's execution-capability input) over
//! the planner-layering examples: the summaries the executor supports
//! restrict Stage 1 (Pass 1, and so Pass 2).

use crate::planner_layering_common;

use asap_cost::CostInputs;
use asap_devtools::planning_stages::lower_sql_roots;
use asap_logical_asap_optimization::{DeploymentCapabilities, SupportedSummary};
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
                    family: *family,
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
                .map(move |(_, statistic)| (family, statistic.kind()))
        })
        .collect()
}

/// Capabilities filter each local computation before physical selection.
/// Compare explicit one-rule applications, not a timed search's archive.
#[test]
fn restricting_summaries_removes_exactly_the_candidates_that_use_them() {
    let heap = supporting(&[(SketchAlgorithm::CountSketchWithHeap, &[StatisticKind::TopK])]);
    assert_removes_exactly("example 1", lower_promql(&example1()), &heap);
    let distinct_and_l2 = supporting(&[
        (
            SketchAlgorithm::UnivMon,
            &[StatisticKind::Distinct, StatisticKind::FrequencyL2],
        ),
        (SketchAlgorithm::Hll, &[StatisticKind::Distinct]),
    ]);
    let (workload, catalog) = example2();
    assert_removes_exactly(
        "example 2",
        lower_sql_roots(&workload, &catalog).unwrap(),
        &distinct_and_l2,
    );
    let kll = supporting(&[(SketchAlgorithm::Kll, &[StatisticKind::Quantile])]);
    assert_removes_exactly("pattern B", lower_promql(&pattern_b()), &kll);
}

fn assert_removes_exactly(
    name: &str,
    roots: Vec<asap_types::ir::QueryRoot>,
    deployment: &DeploymentCapabilities,
) {
    use asap_logical_asap_optimization::test_support::{
        compose_logical_candidate, generate_per_computation_candidates,
    };
    let local = generate_per_computation_candidates(
        &asap_logical_asap_optimization::DagOfLogicalMemoGroups::from_roots(&roots),
    )
    .unwrap();
    let enumerate_local =
        |inventory: &asap_logical_asap_optimization::test_support::PerComputationCandidates| {
            let mut result = Vec::new();
            for (target, options) in inventory.targets.iter().enumerate() {
                for choice in 0..options.choice_count() {
                    let mut choices = vec![0; inventory.targets.len()];
                    choices[target] = choice;
                    if let Ok(roots) = compose_logical_candidate(inventory, &choices) {
                        result.push(asap_types::ir::flat::flatten(&roots).0);
                    }
                }
            }
            result
        };
    let unrestricted = enumerate_local(&local);
    let restricted = enumerate_local(
        &local.without_unsupported_summaries(deployment.supported_summaries.as_deref()),
    );
    let expected = unrestricted
        .iter()
        .filter(|dag| {
            summaries_used(dag)
                .iter()
                .all(|(family, readout)| deployment.supports(family, *readout))
        })
        .collect::<Vec<_>>();
    assert!(
        expected.len() < unrestricted.len(),
        "{name}: unsupported candidates removed"
    );
    assert!(
        restricted.iter().any(|dag| !summaries_used(dag).is_empty()),
        "{name}: supported summary retained"
    );
    assert_eq!(restricted.iter().collect::<Vec<_>>(), expected, "{name}");
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

/// One explicitly shared KLL pane chain, passed through normal physical rules.
fn shared_pane_configurations(deployment: &DeploymentCapabilities) -> Run {
    use asap_logical_asap_optimization::test_support::{
        pane_candidates, window_composition_pass2_candidates,
    };
    use asap_logical_asap_optimization::{
        test_support::{compose_logical_candidate, generate_per_computation_candidates},
        HowToComputeAggregateMeasure,
    };
    use asap_types::ir::QueryRoot;
    let workload = pattern_b_with_p50();
    let original = lower_promql(&workload);
    let local = generate_per_computation_candidates(
        &asap_logical_asap_optimization::DagOfLogicalMemoGroups::from_roots(&original),
    )
    .unwrap();
    let choices = local
        .targets
        .iter()
        .map(|target| {
            target
                .ways_to_compute
                .iter()
                .position(|candidate| {
                    matches!(
                        candidate,
                        HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll)
                    )
                })
                .unwrap()
        })
        .collect::<Vec<_>>();
    let composed = compose_logical_candidate(&local, &choices).unwrap();
    let shared = asap_logical_asap_optimization::test_support::share_common_sub_dags(
        composed
            .into_iter()
            .enumerate()
            .map(|(id, root)| match root {
                QueryRoot::Operator(node) => (id, node),
                _ => unreachable!(),
            })
            .collect(),
    )
    .into_iter()
    .map(|(_, node)| QueryRoot::Operator(node))
    .collect::<Vec<_>>();
    let panes = pane_candidates(&shared, &[Some(60_000), Some(60_000)]);
    assert_eq!(panes.len(), 1, "both estimators share one build");
    let pane_roots = window_composition_pass2_candidates(&shared, &panes)
        .unwrap()
        .remove(0)
        .roots
        .unwrap();
    let settings = CostInputs::default();
    run_explicit_candidates(
        &workload,
        original,
        vec![pane_roots],
        asap_planner::PlanningModels::builtin()
            .with_cost_inputs(&settings)
            .with_deployment_capabilities(deployment),
    )
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
    let run = shared_pane_configurations(&keeping);
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

    let (_, kept) = shared_chain_costs(&shared_pane_configurations(
        &DeploymentCapabilities::default(),
    ));
    assert!(kept.is_empty(), "{kept:?}");
}
