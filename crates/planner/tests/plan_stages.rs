//! The planner pipeline ([`plan_stages`]): the planning stages in order over
//! the frontends' roots, enumerating every whole-workload candidate.

use asap_aware_mapping::pass::{plan_stages, PlanningModels, StagePipelineRun};
use asap_frontend_promql::lower_promql_query_workload;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence, PlanningWorkload, Query,
    QueryLanguage, QueryRequirements, QueryWorkload,
};

const NOW_MS: u64 = 1_700_000_000_000;

fn promql_stages(query: &str, epsilon: f64) -> StagePipelineRun<usize> {
    let target = AccuracyTarget::Epsilon(epsilon);
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(target.clone()),
                    ..Default::default()
                },
                predictability: Default::default(),
                invocations: 1,
                execute_at: None,
                time_selection: Default::default(),
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(15_000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    let roots = lower_promql_query_workload(&workload, NOW_MS).expect("lowers");
    plan_stages(
        roots.into_iter().enumerate().collect(),
        &[Some(target)],
        &workload.data_workload.unwrap(),
        PlanningModels::builtin(),
    )
    .expect("Pass 1 accepts the query")
}

/// Every whole-workload candidate is enumerated, in mixed-radix order (the
/// last target fastest), built and priced; the cheapest valid one is
/// selected and every other one is reported.
#[test]
fn enumerates_every_combination_and_selects_one() {
    let run = promql_stages("count(a) + count(b)", 0.01);
    let sizes: Vec<_> = run
        .per_computation
        .targets
        .iter()
        .map(|target| target.choices.candidates.len())
        .collect();
    assert_eq!(sizes.len(), 2);
    assert_eq!(run.combinations, sizes[0] * sizes[1]);
    assert_eq!(run.logical.len(), run.combinations);
    assert_eq!(run.logical[0].choice, vec![0, 0]);
    assert_eq!(run.logical[1].choice, vec![0, 1]);
    assert_eq!(run.logical[sizes[1]].choice, vec![1, 0]);
    assert_eq!(
        run.physical.len() + run.pruned.len(),
        run.logical.iter().filter(|l| l.roots.is_ok()).count()
    );
    let selection = run.selection.as_ref().unwrap();
    let selected = run.selected().expect("the selected candidate is listed");
    assert_eq!(selected.id, selection.selected);
    assert_eq!(selection.rejected.len(), run.physical.len() - 1);
}
