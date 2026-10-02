//! Logical heap alternatives that need the PromQL series identity are part of
//! Planner's search space: `enumerate_candidate_dags_for_root` lists
//! current-series TopK heaps without a caller-side series-identity pass, cost
//! ranking, or workload Cartesian expansion. Placement variants are not listed.
mod common;
use common::compile_post_asap_dag;
use planner_types::ir::OperatorNode as QueryExpr;

use asap_aware_mapping::{
    accuracy::{AccuracyEvidenceProvider, DefaultAccuracyModel, PropagationStats},
    cost_model::DefaultCostModel,
    replacement::{default_strategies_with_evidence, ReplacementProvenance},
    search_workload_with_targets, Proposals, ReplacementStrategy, ReplacementSubDAG, TargetSubDAG,
};
use asap_physical_operators::physical_planner::promql_rows::{
    compile_current_series_evaluation, SERIES_IDENTITY_COLUMN,
};
use planner_types::{
    post_asap::*,
    types::AccuracyTarget,
    workload::{
        AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence as WorkloadEvidence,
        PlanningWorkload, Predictability, Query, QueryLanguage, QueryRequirements, QueryWorkload,
        TimeSelection,
    },
};
use std::rc::Rc;

struct Evidence;
impl AccuracyEvidenceProvider for Evidence {
    fn topk_max_distinct_items(&self, _: &QueryExpr) -> Option<u64> {
        Some(1000)
    }
    fn propagation_stats(
        &self,
        op: &CompositionOperator,
        _: &FieldDataType,
        _: Option<&SketchStatistic>,
    ) -> PropagationStats {
        if matches!(op, CompositionOperator::TopKSelection) {
            PropagationStats {
                topk_selected_lower_bound: Some(101.),
                topk_excluded_upper_bound: Some(100.),
                topk_interval_failure_probability: Some(0.001),
                ..Default::default()
            }
        } else {
            Default::default()
        }
    }
}

/// Forwards everything except whole-root proposals: the pre-change search.
struct LogicalOnly(Box<dyn ReplacementStrategy>);
impl ReplacementStrategy for LogicalOnly {
    fn name(&self) -> &'static str {
        self.0.name()
    }
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        self.0.matches(target)
    }
    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        self.0.replacements(target)
    }
    fn propose(&self, target: &TargetSubDAG<'_>) -> Proposals {
        self.0.propose(target)
    }
}

fn lower(query: &str, accuracy: &AccuracyTarget) -> Rc<QueryExpr> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(accuracy.clone()),
                    ..Default::default()
                },
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
                time_selection: TimeSelection::default(),
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: WorkloadEvidence {
                value: Some(DurationMs(1_000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    asap_frontend_promql::lower_promql_workload(&workload, 0)
        .unwrap()
        .remove(0)
}

type Dag = Vec<(usize, Rc<planner_types::ir::OperatorNode>)>;

/// Candidate DAGs for query 1 of a two-query workload, with and without
/// whole-root proposals. Query 0 is a bystander that must not multiply them.
fn inventories(query: &str, accuracy: AccuracyTarget) -> (Vec<Dag>, Vec<Dag>) {
    let roots = vec![
        (
            0,
            lower("sum by(job)(m)", &AccuracyTarget::Exact),
            Some(AccuracyTarget::Exact),
        ),
        (1, lower(query, &accuracy), Some(accuracy)),
    ];
    let full = default_strategies_with_evidence(&DefaultCostModel, &Evidence);
    let logical: Vec<Box<dyn ReplacementStrategy>> =
        default_strategies_with_evidence(&DefaultCostModel, &Evidence)
            .into_iter()
            .map(|strategy| Box::new(LogicalOnly(strategy)) as Box<dyn ReplacementStrategy>)
            .collect();
    let enumerate = |strategies: &[Box<dyn ReplacementStrategy>]| {
        search_workload_with_targets(roots.clone(), strategies, &DefaultAccuracyModel)
            .enumerate_candidate_dags_for_root(&1, 65_536)
            .unwrap()
            .candidates
    };
    (enumerate(&full), enumerate(&logical))
}

fn carries_identity(dag: &Dag) -> bool {
    dag.iter().any(|(_, root)| {
        compile_post_asap_dag(root)
            .unwrap()
            .nodes
            .iter()
            .any(|node| {
                node.output_schema
                    .fields
                    .iter()
                    .any(|field| field.name == SERIES_IDENTITY_COLUMN)
            })
    })
}

/// Shared acceptance checks; returns the added identity-carrying alternatives.
fn added_alternatives(
    query: &str,
    accuracy: AccuracyTarget,
) -> Vec<Rc<planner_types::ir::OperatorNode>> {
    let (full, logical) = inventories(query, accuracy);
    for (index, dag) in full.iter().enumerate() {
        assert_eq!(dag.len(), 1, "one root per candidate, no workload product");
        assert!(
            !full[..index].contains(dag),
            "{query}: identical DAG listed twice"
        );
    }
    let (added, kept): (Vec<_>, Vec<_>) = full.into_iter().partition(carries_identity);
    assert_eq!(
        kept, logical,
        "{query}: existing candidates must be unchanged"
    );
    added.into_iter().map(|mut dag| dag.remove(0).1).collect()
}

const CURRENT_SERIES_TOPK: &str = "topk by(job)(1, m)";

// Instant-vector TopK lists finalized current-series heap evaluations.
#[test]
fn current_series_topk_lists_heap_evaluations() {
    let added = added_alternatives(CURRENT_SERIES_TOPK, AccuracyTarget::Epsilon(0.1));
    assert!(!added.is_empty());
    for root in added {
        assert!(!matches!(
            root.operator,
            planner_types::ir::Operator::ASAP(planner_types::ir::ASAPOp::SummaryAgg { .. })
        ));
        assert!(
            compile_current_series_evaluation(&root).is_ok(),
            "unbindable alternative {root:?}"
        );
    }
}

// Rate queries gain no fixed-window or query-time placement variants.
#[test]
fn rate_placement_variants_are_not_listed() {
    for (query, accuracy) in [
        ("sum by(job)(rate(m[1m]))", AccuracyTarget::Exact),
        ("topk by(job)(2, rate(m[1m]))", AccuracyTarget::Epsilon(0.1)),
        ("rate(m[1m])", AccuracyTarget::Exact),
    ] {
        assert!(added_alternatives(query, accuracy).is_empty(), "{query}");
    }
}

// Queries without a current-series heap realization are unchanged.
#[test]
fn unrelated_queries_keep_their_inventory() {
    for (query, accuracy) in [
        ("sum by(job)(m)", AccuracyTarget::Exact),
        (
            "quantile_over_time(0.9, m[1m])",
            AccuracyTarget::Epsilon(0.05),
        ),
        ("max_over_time(m[1m])", AccuracyTarget::Exact),
    ] {
        assert!(added_alternatives(query, accuracy).is_empty(), "{query}");
    }
}

// Default cost-based selection keeps the logical plan; deployment prices heaps.
#[test]
fn global_selection_never_commits_a_series_identity_heap() {
    let accuracy = AccuracyTarget::Epsilon(0.1);
    let root = lower(CURRENT_SERIES_TOPK, &accuracy);
    let strategies = default_strategies_with_evidence(&DefaultCostModel, &Evidence);
    let space = search_workload_with_targets(
        vec![(0, root, Some(accuracy))],
        &strategies,
        &DefaultAccuracyModel,
    );
    let selected = space
        .global_selection(&DefaultCostModel)
        .assemble_selected_dag(&space.roots[0].1)
        .unwrap()
        .unwrap();
    assert!(!carries_identity(&vec![(0, selected)]));
}

// A query repeated in the workload is proposed once, not once per copy.
#[test]
fn repeated_roots_do_not_duplicate_alternatives() {
    let accuracy = AccuracyTarget::Epsilon(0.1);
    let strategies = default_strategies_with_evidence(&DefaultCostModel, &Evidence);
    let count = |copies: usize| {
        let roots = (0..copies)
            .map(|id| {
                (
                    id,
                    lower(CURRENT_SERIES_TOPK, &accuracy),
                    Some(accuracy.clone()),
                )
            })
            .collect();
        let space = search_workload_with_targets(roots, &strategies, &DefaultAccuracyModel);
        space
            .candidates_for_target(&space.roots[0].1)
            .unwrap()
            .candidates
            .iter()
            .filter(|candidate| {
                candidate.provenance == ReplacementProvenance::RootPhysicalRealization
            })
            .count()
    };
    assert!(count(1) > 0);
    assert_eq!(count(2), count(1));
}
