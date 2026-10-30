//! Original issue workloads against the production candidate-connection planner.
use asap_cost::CostInputs;
use asap_devtools::planner_fuzz::{read_seed_file, FuzzSettings, FuzzWorkload, Planner};
use asap_devtools::planning_stages::run_search_over;
use asap_plan_selection::{PlanningModels, Stage1ToStage3Search};
use serde_json::json;
use std::path::Path;

fn workload(issue: u32) -> FuzzWorkload {
    let mut w = read_seed_file(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/sharing_issue_fixtures/{issue}.json")),
    )
    .unwrap();
    w.tables = vec![json!({"name":"metrics", "time_index":0, "columns":[
        {"name":"ts","type":"timestamp","nullable":false},
        {"name":"city","type":"utf8","nullable":false},
        {"name":"host","type":"utf8","nullable":false},
        {"name":"value","type":"float64","nullable":false}
    ]})];
    if issue == 902 || issue == 886 {
        for field in w.tables[0]["columns"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .skip(1)
        {
            field["nullable"] = json!(true);
        }
    }
    w
}

fn cost(w: &FuzzWorkload) -> f64 {
    // Each regression has a wall-clock planning budget. Concurrent solver
    // runs in this test binary must not consume one another's CPU budget.
    static PLANNING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _planning = PLANNING.lock().unwrap();
    let roots = Planner::new(FuzzSettings::default()).lower(w).unwrap();
    let planning = w.planning_workload();
    let entries: Vec<_> = planning.query_workload.entries().collect();
    let settings = CostInputs::default();
    let deployment = w.deployment();
    let models = PlanningModels::builtin()
        .with_plan_selection(&settings)
        .with_deployment_capabilities(&deployment);
    let run = run_search_over(
        roots,
        &entries,
        &w.data,
        models,
        Stage1ToStage3Search {
            milp_time_limit_ms: Some(30_000),
            ..Default::default()
        },
    )
    .unwrap();
    let selection = run.stage3_plan_selection.as_ref().unwrap();
    let value = selection.costs[&selection.selected]
        .cost_per_second
        .unwrap();
    println!("cost={value}; selected={}", selection.selected);
    value
}

/// #923: deriving the coarse sum costs only a sum over the 150 fine groups.
#[test]
fn exact_coarse_sum_reuses_fine_sum() {
    let related = workload(923);
    let mut base = related.clone();
    base.queries.truncate(1);
    let base_cost = cost(&base);
    let related_cost = cost(&related);
    assert!(
        related_cost <= base_cost + 0.00004,
        "{related_cost} vs {base_cost}"
    );
}

/// #924: the 1m sum reads the newest pane of the existing 2m chain.
#[test]
fn nested_exact_sum_reuses_newest_panes() {
    let related = workload(924);
    let mut base = related.clone();
    base.queries.truncate(1);
    let base_cost = cost(&base);
    let related_cost = cost(&related);
    assert!(
        related_cost <= base_cost * 1.001,
        "{related_cost} vs {base_cost}"
    );
}

/// #925: the slower copy reads the same maintained sum.
#[test]
fn slower_repeat_reuses_faster_sum() {
    let related = workload(925);
    let mut base = related.clone();
    base.queries.truncate(1);
    let base_cost = cost(&base);
    let related_cost = cost(&related);
    assert!(
        related_cost <= base_cost * 1.001,
        "{related_cost} vs {base_cost}"
    );
}

/// #902: the filtered reader avoids its independent scan and aggregation.
#[test]
fn grouped_filtered_quantile_plans() {
    assert!(
        cost(&workload(902)) < 0.25,
        "filtered query must reuse the grouped result"
    );
}

/// #886: SQL count-ranked TopK offers a heap over raw rows, absorbing the count.
#[test]
fn sql_count_topk_has_a_fused_raw_row_heap() {
    use asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates;
    let roots = Planner::new(FuzzSettings::default())
        .lower(&workload(886))
        .unwrap();
    let inventory = generate_per_computation_candidates(
        &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
            &roots.into_iter().enumerate().collect::<Vec<_>>(),
        ),
    )
    .unwrap();
    let (index, target) = inventory
        .targets
        .iter()
        .enumerate()
        .find(|(_, target)| target.fused_computation.is_some())
        .expect("SQL TopK must offer a fused raw-row heap");
    for choice in target.ways_to_compute.len()..target.choice_count() {
        let mut selected = vec![0; inventory.targets.len()];
        selected[index] = choice;
        let composed =
            asap_logical_asap_optimization::pass1::per_computation_candidates::compose_logical_candidate(
                &inventory, &selected,
            )
            .unwrap();
        let roots: Vec<_> = composed;
        let root = roots[0].as_operator().unwrap();
        for node in asap_types::ir::OperatorNode::reachable(root) {
            if let Some(asap_types::ir::ASAPOp::SummaryAgg {
                family: asap_types::ir::schema::FieldDataType::Sketch(_, _),
                input,
                child,
                ..
            }) = node.asap()
            {
                assert_eq!(
                    input.weight,
                    asap_types::ir::schema::SummaryInputExpr::Constant(1.0)
                );
                assert!(!child.contains_asap(), "heap consumes raw rows");
            }
        }
        let guarded = Planner::new(FuzzSettings::default()).price(&workload(886), &roots, false);
        assert!(
            guarded.unwrap_err().contains("NoAccuracyGuarantee"),
            "frequency intervals must not be claimed as Top-K membership guarantees"
        );
        use asap_executor::values::Value;
        let rows = (0..7)
            .map(|_| {
                vec![
                    Value::Timestamp(0),
                    Value::Utf8("NYC".into()),
                    Value::Utf8("one-host".into()),
                    Value::Float64(1.0),
                ]
            })
            .collect();
        let actual = physical_common::execute_raw_rows_at(root, rows, 1000);
        assert_eq!(actual.len(), 1, "{actual:?}");
        assert!(
            actual[0]
                .iter()
                .any(|v| matches!(v, Value::Utf8(s) if s.as_ref() == "one-host")),
            "{actual:?}"
        );
        assert!(
            actual[0]
                .iter()
                .any(|v| matches!(v, Value::Float64(count) if *count == 7.0)),
            "{actual:?}"
        );
        let compiled = Planner::new(FuzzSettings {
            enforce_accuracy_guarantees: false,
            ..Default::default()
        })
        .price(&workload(886), &roots, false);
        assert!(compiled.is_ok(), "fused heap must compile: {compiled:?}");
    }
}

/// #923: label projection differences below the same per-series average do not
/// prevent an exact sum from rolling up the finer sum's output.
#[test]
fn exact_rollup_catalog_crosses_identical_per_series_average() {
    use asap_logical_asap_optimization::{
        pass1::per_computation_candidates::generate_per_computation_candidates,
        pass2::memo::{
            candidates::try_populate_pass1, shared_configurations::PaneWidthDomain,
            DagOfLogicalMemoGroups,
        },
    };
    use asap_types::stages::OriginOfCandidateForMemoGroup;
    let roots: Vec<_> = Planner::new(FuzzSettings::default())
        .lower(&workload(923))
        .unwrap()
        .into_iter()
        .enumerate()
        .collect();
    let initial = DagOfLogicalMemoGroups::from_roots(&roots);
    let inventory = generate_per_computation_candidates(
        &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(&roots),
    )
    .unwrap();
    let memo = try_populate_pass1(initial, &inventory).unwrap();
    let expanded =
        asap_logical_asap_optimization::test_support::sharing_rules_and_shared_state_groups(
            memo,
            PaneWidthDomain::SeedGrids,
        )
        .unwrap();
    let owner = expanded.query_roots[1].as_operator().unwrap().0 as usize;
    assert!(
        expanded.candidates[owner]
            .iter()
            .any(|c| c.origin == OriginOfCandidateForMemoGroup::Rollup),
        "coarse root has no rollup candidate"
    );
}

use crate::physical_common;

/// #902: selecting one group preserves values and SQL's one NULL row when
/// the group is absent, including empty inputs and groups of NULL samples.
#[test]
fn filtered_group_preserves_sql_empty_and_null_semantics() {
    use asap_executor::values::Value;
    use asap_logical_asap_optimization::{
        pass1::per_computation_candidates::generate_per_computation_candidates,
        pass2::memo::{
            candidates::try_populate_pass1, shared_configurations::PaneWidthDomain,
            DagOfLogicalMemoGroups,
        },
    };
    use asap_types::ir::{operator::AggIntent, NonASAPOp};
    use asap_types::stages::OriginOfCandidateForMemoGroup;
    let roots: Vec<_> = Planner::new(FuzzSettings::default())
        .lower(&workload(902))
        .unwrap()
        .into_iter()
        .enumerate()
        .collect();
    let initial = DagOfLogicalMemoGroups::from_roots(&roots);
    let inventory = generate_per_computation_candidates(
        &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(&roots),
    )
    .unwrap();
    let memo = try_populate_pass1(initial, &inventory).unwrap();
    let expanded =
        asap_logical_asap_optimization::test_support::sharing_rules_and_shared_state_groups(
            memo,
            PaneWidthDomain::SeedGrids,
        )
        .unwrap();
    let (owner, candidate) = expanded.candidates.iter().enumerate().flat_map(|(owner, candidates)| candidates.iter().map(move |c| (owner,c)))
        .find(|(_, c)| c.origin == OriginOfCandidateForMemoGroup::Rollup && matches!(c.root_node.non_asap(), Some(NonASAPOp::Aggregate { measures, child, .. })
            if matches!(measures.as_slice(), [AggIntent::Min { .. }]) && matches!(child.non_asap(), Some(NonASAPOp::Filter { .. }))))
        .expect("filtered group read candidate");
    // The same readout keeps a KLL's rank bound: it selects a whole group,
    // never predicates on an approximate value or combines populations.
    use asap_accuracy::propagation::recompose_guarantees;
    use asap_logical_asap_optimization::pass1::per_computation_candidates::{
        compose_logical_candidate, HowToComputeAggregateMeasure,
    };
    use asap_types::ir::{schema::SketchAlgorithm, QueryRoot};
    use std::rc::Rc;
    let filter = candidate.root_node.children()[0];
    let grouped = filter.children()[0];
    let inv = generate_per_computation_candidates(
        &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[(
            0,
            QueryRoot::Operator(Rc::clone(grouped)),
        )]),
    )
    .unwrap();
    let choices: Vec<_> = inv
        .targets
        .iter()
        .map(|target| {
            target
                .ways_to_compute
                .iter()
                .position(|c| {
                    matches!(
                        c,
                        HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll)
                    )
                })
                .unwrap_or(0)
        })
        .collect();
    let approximate = compose_logical_candidate(&inv, &choices).unwrap().remove(0);
    let approximate = approximate.as_operator().unwrap();
    let filtered = Rc::new(
        filter
            .with_new_children(|_| Rc::clone(approximate))
            .unwrap(),
    );
    let readout = Rc::new(
        candidate
            .root_node
            .with_new_children(|_| Rc::clone(&filtered))
            .unwrap(),
    );
    let recomposed = recompose_guarantees(&[QueryRoot::Operator(readout)]).unwrap();
    assert!(approximate.guarantee.is_some());
    assert_eq!(
        recomposed[0].as_operator().unwrap().guarantee,
        approximate.guarantee
    );
    // A range of keys can select multiple populations; their extrema do not
    // inherit one selected quantile's rank bound.
    let mut many_groups = (*filtered).clone();
    if let asap_types::ir::Operator::NonASAP(NonASAPOp::Filter { pred, .. }) =
        &mut many_groups.operator
    {
        if let asap_types::ir::ScalarExpr::Compare { op, .. } = &mut pred.0 {
            *op = asap_types::ir::scalar::CompareOpKind::Gt;
        }
    }
    let many_groups = Rc::new(many_groups);
    let invalid = Rc::new(
        candidate
            .root_node
            .with_new_children(|_| Rc::clone(&many_groups))
            .unwrap(),
    );
    assert!(
        recompose_guarantees(&[QueryRoot::Operator(invalid)]).unwrap()[0]
            .as_operator()
            .unwrap()
            .guarantee
            .is_none()
    );
    // A filter that also reads the estimate decides by an approximate value
    // which group is kept, so the selected group's bound does not hold.
    let mut on_estimate = (*filtered).clone();
    if let asap_types::ir::Operator::NonASAP(NonASAPOp::Filter { pred, .. }) =
        &mut on_estimate.operator
    {
        pred.0 = asap_types::ir::ScalarExpr::BoolAnd(vec![
            pred.0.clone(),
            asap_types::ir::ScalarExpr::IsNotNull(Box::new(asap_types::ir::ScalarExpr::Column(1))),
        ]);
    }
    let on_estimate = Rc::new(on_estimate);
    let invalid = Rc::new(
        candidate
            .root_node
            .with_new_children(|_| Rc::clone(&on_estimate))
            .unwrap(),
    );
    assert!(
        recompose_guarantees(&[QueryRoot::Operator(invalid)]).unwrap()[0]
            .as_operator()
            .unwrap()
            .guarantee
            .is_none()
    );
    let row = |city: &str, value| {
        vec![
            Value::Timestamp(0),
            Value::Utf8(city.into()),
            Value::Utf8("host".into()),
            value,
        ]
    };
    for rows in [
        vec![],
        vec![row("LA", Value::Float64(5.0))],
        vec![row("NYC", Value::Null)],
        vec![
            row("NYC", Value::Float64(10.0)),
            row("NYC", Value::Float64(30.0)),
            row("LA", Value::Float64(90.0)),
        ],
    ] {
        let before = physical_common::execute_raw_rows_at(
            &expanded.groups[owner].logical_node,
            rows.clone(),
            1000,
        );
        let after = physical_common::execute_raw_rows_at(&candidate.root_node, rows, 1000);
        assert_eq!(format!("{after:?}"), format!("{before:?}"));
        assert_eq!(after.len(), 1);
    }
}

/// #923: SQL counts roll up by merging integer states, without float casts.
#[test]
fn sql_count_rollup_executes_with_integer_results() {
    use asap_executor::values::Value;
    use asap_logical_asap_optimization::{
        pass1::per_computation_candidates::generate_per_computation_candidates,
        pass2::memo::{
            candidates::try_populate_pass1, shared_configurations::PaneWidthDomain,
            DagOfLogicalMemoGroups,
        },
    };
    use asap_types::ir::ASAPOp;
    use asap_types::stages::OriginOfCandidateForMemoGroup;
    let mut w = workload(902);
    w.queries.truncate(2);
    w.queries[0].entry.query.0 =
        "SELECT city, host, COUNT(*) AS c FROM metrics GROUP BY city, host".into();
    for query in [
        "SELECT city, COUNT(*) AS c FROM metrics GROUP BY city",
        "SELECT COUNT(*) AS c FROM metrics",
    ] {
        w.queries[1].entry.query.0 = query.into();
        let roots: Vec<_> = Planner::new(FuzzSettings::default())
            .lower(&w)
            .unwrap()
            .into_iter()
            .enumerate()
            .collect();
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let inventory = generate_per_computation_candidates(
            &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                &roots,
            ),
        )
        .unwrap();
        let memo = try_populate_pass1(initial, &inventory).unwrap();
        let expanded =
            asap_logical_asap_optimization::test_support::sharing_rules_and_shared_state_groups(
                memo,
                PaneWidthDomain::SeedGrids,
            )
            .unwrap();
        let (owner, candidate) = expanded
            .candidates
            .iter()
            .enumerate()
            .flat_map(|(owner, candidates)| candidates.iter().map(move |c| (owner, c)))
            .find(|(_, c)| {
                c.origin == OriginOfCandidateForMemoGroup::Rollup
                    && asap_types::ir::OperatorNode::reachable(&c.root_node)
                        .iter()
                        .any(|node| {
                            matches!(node.asap(), Some(ASAPOp::FinalizeExactAccumulator { child })
            if matches!(child.asap(), Some(ASAPOp::SummaryMerge { group_by:Some(_), .. })))
                        })
            })
            .expect("integer count state rollup candidate");
        let rows = vec![
            vec![
                Value::Timestamp(0),
                Value::Utf8("NYC".into()),
                Value::Utf8("a".into()),
                Value::Null,
            ],
            vec![
                Value::Timestamp(0),
                Value::Utf8("NYC".into()),
                Value::Utf8("b".into()),
                Value::Float64(10.0),
            ],
            vec![
                Value::Timestamp(0),
                Value::Null,
                Value::Utf8("a".into()),
                Value::Null,
            ],
        ];
        let before =
            physical_common::execute_raw_rows(&expanded.groups[owner].logical_node, rows.clone());
        let after = physical_common::execute_raw_rows(&candidate.root_node, rows);
        let sorted = |rows: Vec<Vec<Value>>| {
            let mut rows: Vec<_> = rows.iter().map(|row| format!("{row:?}")).collect();
            rows.sort();
            rows
        };
        assert!(after
            .iter()
            .all(|row| matches!(row.last(), Some(Value::Int64(_)))));
        assert_eq!(sorted(before), sorted(after));
        let before_empty =
            physical_common::execute_raw_rows(&expanded.groups[owner].logical_node, vec![]);
        let after_empty = physical_common::execute_raw_rows(&candidate.root_node, vec![]);
        assert_eq!(sorted(before_empty), sorted(after_empty));
    }
}

/// #922: duplicate exact TopK readers share their result and inner window work.
#[test]
fn duplicate_exact_topk_shares_work() {
    let mut w = workload(924);
    for query in &mut w.queries {
        query.entry.query.0 = "topk(3, sum_over_time(m_gauge[2m]))".into();
    }
    let mut base = w.clone();
    base.queries.truncate(1);
    let single = cost(&base);
    let duplicate = cost(&w);
    assert!(duplicate <= single * 1.001, "{duplicate} vs {single}");
}

/// #924/#925 SQL variants: nested windows and slower readers reuse one sum.
#[test]
fn sql_sum_shares_nested_windows_and_slower_readers() {
    let mut w = workload(922);
    w.queries.truncate(1);
    w.queries[0].entry.query.0 =
        "SELECT SUM(value) FROM metrics WHERE ts > now() - INTERVAL '5' MINUTE".into();
    w.queries[0].entry.demand = serde_json::from_value(json!({"fixed_interval":60000})).unwrap();
    let single = cost(&w);
    let mut nested = w.clone();
    let mut shorter = w.queries[0].clone();
    shorter.entry.query.0 = shorter.entry.query.0.replace("'5'", "'1'");
    nested.queries.push(shorter);
    let nested_cost = cost(&nested);
    assert!(nested_cost <= single * 1.001, "{nested_cost} vs {single}");
    let mut slower = w.queries[0].clone();
    slower.entry.demand = serde_json::from_value(json!({"fixed_interval":120000})).unwrap();
    w.queries.push(slower);
    let repeated_cost = cost(&w);
    assert!(
        repeated_cost <= single * 1.001,
        "{repeated_cost} vs {single}"
    );
}

/// #923: sketches over an inner range function expose the same finer rollup.
#[test]
fn sketch_rollup_crosses_inner_range_function() {
    use asap_logical_asap_optimization::{
        pass1::per_computation_candidates::generate_per_computation_candidates,
        pass2::memo::{
            candidates::try_populate_pass1, shared_configurations::PaneWidthDomain,
            DagOfLogicalMemoGroups,
        },
    };
    use asap_types::stages::OriginOfCandidateForMemoGroup;
    let mut w = workload(923);
    for (query, keys) in w.queries.iter_mut().zip(["job, region", "job"]) {
        query.entry.query.0 = format!("quantile by ({keys}) (0.95, sum_over_time(m_gauge[5m]))");
        query.entry.requirements.accuracy =
            asap_types::ir::properties::AccuracyTarget::max_error(0.05).failure_probability(0.01);
    }
    let roots: Vec<_> = Planner::new(FuzzSettings::default())
        .lower(&w)
        .unwrap()
        .into_iter()
        .enumerate()
        .collect();
    let initial = DagOfLogicalMemoGroups::from_roots(&roots);
    let inventory = generate_per_computation_candidates(
        &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(&roots),
    )
    .unwrap();
    let memo = try_populate_pass1(initial, &inventory).unwrap();
    let expanded =
        asap_logical_asap_optimization::test_support::sharing_rules_and_shared_state_groups(
            memo,
            PaneWidthDomain::SeedGrids,
        )
        .unwrap();
    let owner = expanded.query_roots[1].as_operator().unwrap().0 as usize;
    assert!(expanded.candidates[owner]
        .iter()
        .any(|c| c.origin == OriginOfCandidateForMemoGroup::Rollup));
}

/// #886: unsupported per-group SQL count sketches are explicitly pruned;
/// the exact counter and separately typed raw-row TopK fusion remain available.
#[test]
fn sql_grouped_count_sketches_are_not_offered() {
    use asap_logical_asap_optimization::pass1::per_computation_candidates::{
        generate_per_computation_candidates, HowToComputeAggregateMeasure,
    };
    use asap_types::ir::{operator::AggIntent, NonASAPOp};
    use asap_types::stages::PruneReason;
    let w = workload(886);
    let roots = Planner::new(FuzzSettings::default()).lower(&w).unwrap();
    let inventory = generate_per_computation_candidates(
        &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
            &roots.into_iter().enumerate().collect::<Vec<_>>(),
        ),
    )
    .unwrap();
    let (index, target) = inventory.targets.iter().enumerate().find(|(_, target)| matches!(inventory.aggregate(target).non_asap(), Some(NonASAPOp::Aggregate { measures, .. }) if matches!(measures.as_slice(), [AggIntent::Count { .. }]))).unwrap();
    assert!(target
        .ways_to_compute
        .iter()
        .all(|candidate| !matches!(candidate, HowToComputeAggregateMeasure::Summary(_))));
    assert_eq!(
        inventory
            .pruned_of_target(index, false)
            .iter()
            .filter(|(_, reason)| *reason == PruneReason::NoRelationalCountSketchRealization)
            .count(),
        2
    );
    assert!(inventory
        .targets
        .iter()
        .any(|target| target.fused_computation.is_some()));
}
