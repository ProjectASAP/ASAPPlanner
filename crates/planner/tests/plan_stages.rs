//! The planner pipeline ([`run_stage1_to_stage3`]): the planning stages in order over
//! the frontends' roots, enumerating every whole-workload candidate.

use asap_frontend_promql::lower_promql_query_workload;
use asap_plan_selection::pass::{run_stage1_to_stage3_with, PlanningModels, Stage1ToStage3Run};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::workload::{
    BatchEntry, DataWorkload, DurationMs, PlanningWorkload, Query, QueryLanguage,
    QueryRequirements, QueryWorkload,
};

fn promql_stages(query: &str, epsilon: f64) -> Stage1ToStage3Run<usize> {
    let target = AccuracyTarget::max_error(epsilon);
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: target.clone(),
                    ..Default::default()
                },
                predictability: Default::default(),
                invocations: 1,
                execute_at: None,
                range_evaluation: None,
            }]),
            repeating_queries: None,
        },
        data_workload: Some(
            DataWorkload::sampled_every(
                asap_frontend_promql::promql_metric_names(query).unwrap_or_default(),
                DurationMs(15_000),
            )
            .with_series_count(1_000),
        ),
    };
    let roots = lower_promql_query_workload(&workload).expect("lowers");
    run_stage1_to_stage3_with(
        roots.into_iter().enumerate().collect(),
        &workload.query_workload.entries().collect::<Vec<_>>(),
        &workload.data_workload.unwrap(),
        PlanningModels::builtin(),
        asap_plan_selection::Stage1ToStage3Search::exhaustive(),
    )
    .expect("Pass 1 accepts the query")
}

/// Reconstructed candidate assignments retain their stage provenance, and
/// selection references an archived physical plan.
#[test]
fn archives_connected_candidates_and_selects_one() {
    let run = promql_stages("count(a) + count(b)", 0.01);
    let sizes: Vec<_> = run
        .stage1_pass1_per_computation
        .targets
        .iter()
        .map(|target| target.choice_count())
        .collect();
    assert_eq!(sizes.len(), 2);
    assert_eq!(run.combinations, sizes[0] * sizes[1]);
    // The archive contains connected physical assignments, not the Cartesian
    // product of Pass 1 choices. Sharing and materialization add decisions.
    let logical_ids: std::collections::BTreeSet<_> = run
        .stage1_logical_asap_optimization
        .iter()
        .map(|candidate| &candidate.id)
        .collect();
    assert_eq!(
        logical_ids.len(),
        run.stage1_logical_asap_optimization.len()
    );
    for physical in &run.stage2_physical_asap_optimization {
        assert!(logical_ids.contains(&physical.from_logical));
    }
    assert_eq!(
        run.stage2_physical_asap_optimization.len() + run.stage2_pruned.len(),
        run.stage1_logical_asap_optimization
            .iter()
            .filter(|l| l.roots.is_ok())
            .count()
    );
    let selection = run.stage3_plan_selection.as_ref().unwrap();
    let selected = run.selected().expect("the selected candidate is listed");
    assert_eq!(selected.id, selection.selected);
    assert_eq!(
        selection.rejected.len(),
        run.stage2_physical_asap_optimization.len() - 1
    );
}
