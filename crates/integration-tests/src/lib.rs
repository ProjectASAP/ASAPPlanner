//! Shared test fixtures for the ASAP e2e test suite.
//!
//! This crate owns the integration tests that verify correct pre-ASAP IR
//! output for each input **PromQL** query workload (it depends on the PromQL
//! front end only). The *cross-language* equivalence tests — semantically
//! equivalent SQL and PromQL mapping to the same canonical pre-ASAP IR —
//! live in `crates/devtools/tests/cross_language.rs`, where both front ends
//! are in scope.
//!
//! `fixtures` provides column/schema constructors used across test files.
//! Expected IR DAGs are always hand-constructed inside each test — nothing
//! here derives or computes expected outputs.

pub mod fixtures {

    use asap_types::ir::properties::AccuracyTarget;

    use asap_types::ir::schema::{DataType, Field, Schema};
    use asap_types::ir::OperatorNode;
    use asap_types::workload::{
        BatchEntry, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query,
        QueryLanguage, QueryRequirements, QueryWorkload,
    };
    use std::rc::Rc;

    /// Lower one query through the plan-ready workload API using the test
    /// suite's declared one-second source cadence.
    pub fn lower_promql(
        query: &str,
        accuracy: AccuracyTarget,
    ) -> Result<Rc<OperatorNode>, asap_frontend_promql::PromqlError> {
        match lower_promql_root(query, accuracy)? {
            asap_types::ir::QueryRoot::Operator(node) => Ok(node),
            _ => Err(asap_frontend_promql::PromqlError::UnsupportedFeature(
                "expected vector root".into(),
            )),
        }
    }

    pub fn lower_promql_root(
        query: &str,
        accuracy: AccuracyTarget,
    ) -> Result<asap_types::ir::QueryRoot, asap_frontend_promql::PromqlError> {
        let workload = PlanningWorkload {
            query_workload: QueryWorkload {
                language: QueryLanguage::PromQL,
                query_batch: Some(vec![BatchEntry {
                    query: Query(query.into()),
                    requirements: QueryRequirements {
                        accuracy,
                        ..Default::default()
                    },
                    predictability: Predictability::Unknown,
                    invocations: 1,
                    execute_at: None,
                    range_evaluation: None,
                }]),
                repeating_queries: None,
            },
            data_workload: Some(DataWorkload::sampled_every(
                asap_frontend_promql::promql_metric_names(query).unwrap_or_default(),
                DurationMs(1_000),
            )),
        };
        let mut lowered = asap_frontend_promql::lower_promql_query_workload(&workload)?;
        Ok(lowered.remove(0))
    }

    pub fn ts_col() -> Field {
        Field::plain("ts", DataType::Timestamp, false)
    }

    pub fn value_col() -> Field {
        Field::plain("value", DataType::Float64, false)
    }

    pub fn label_col(name: &str) -> Field {
        Field::plain(name, DataType::Utf8, true)
    }

    /// Canonical PromQL leaf schema: `(ts: Timestamp, value: Float64)` plus
    /// any label columns referenced in the query, in the order the schema resolver
    /// appends them (alphabetical after dedup).
    pub fn metric_schema(labels: &[&str]) -> Schema {
        let mut cols = vec![ts_col(), value_col()];
        cols.extend(labels.iter().map(|n| label_col(n)));
        Schema {
            fields: cols,
            time_index: Some(0),
            unique_keys: vec![],
            // Schemaless PromQL leaf: open (the metric's full label set is
            // runtime-only; this lists just the referenced labels).
            closed: false,
        }
    }
}

/// Physical ASAP DAG export helpers for planned candidates.
pub mod physical_export {
    use asap_types::ir::physical_export::PhysicalASAPDAG;
    use asap_types::ir::OperatorNode;
    use std::rc::Rc;

    /// Export the physical DAG of `root`.
    pub fn physical_asap_dag(root: &Rc<OperatorNode>) -> PhysicalASAPDAG {
        asap_types::test_support::physical_asap_dag_at_query_time(root)
            .expect("physical ASAP DAG export failed")
    }
}

/// Plans and candidates from the planner pipeline (the facade's
/// `run_planning`) for one
/// query, with the built-in models.
pub mod planning {
    use asap_types::ir::physical_export::PhysicalASAPDAG;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::{NonASAPOp, OperatorNode, QueryRoot};
    use asap_types::workload::{DataArrival, DataWorkload, SourceFacts};
    use std::rc::Rc;

    /// The data these tests plan over, declared: 1,000 rows arriving per
    /// second, each source `root` scans holding 1,000 series, and each SQL
    /// table a minute of rows (60,000), which bounds a scan with no window.
    pub fn declared_data(root: &Rc<OperatorNode>) -> DataWorkload {
        let sources = OperatorNode::reachable(root)
            .into_iter()
            .filter_map(|node| match node.non_asap() {
                Some(NonASAPOp::Scan { source, .. }) => Some(match source {
                    asap_types::ir::operator::Source::TimeSeries { metric } => {
                        (metric.clone(), None)
                    }
                    asap_types::ir::operator::Source::Table { table_ref } => {
                        (table_ref.clone(), Some(60_000))
                    }
                }),
                _ => None,
            })
            .map(|(source, rows_held)| {
                let facts = SourceFacts {
                    series_count: Some(1_000),
                    rows_held,
                    ..SourceFacts::default()
                };
                (source, facts)
            })
            .collect();
        DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 1_000.0,
                max_out_of_order_ms: None,
            },
            sources,
        }
    }

    /// Every planning stage's output for the single query `root`.
    pub fn run_all_stages(
        root: Rc<OperatorNode>,
        accuracy: AccuracyTarget,
    ) -> asap_devtools::planning_stages::PlanningStagesRun {
        run_all_stages_with(root, accuracy, asap_planner::PlanningModels::builtin())
    }

    /// [`run_all_stages`] with `models`.
    fn run_all_stages_with(
        root: Rc<OperatorNode>,
        accuracy: AccuracyTarget,
        models: asap_planner::PlanningModels<'_>,
    ) -> asap_devtools::planning_stages::PlanningStagesRun {
        let data = declared_data(&root);
        asap_devtools::planning_stages::run_planning_stages_over(
            vec![QueryRoot::Operator(root)],
            &[asap_types::workload::QueryWorkloadEntry {
                query: Default::default(),
                requirements: asap_types::workload::QueryRequirements {
                    accuracy,
                    max_latency_ms: None,
                },
                predictability: Default::default(),
                recurrence: Default::default(),
                range_evaluation: None,
            }],
            &data,
            models,
        )
        .expect("Pass 1 accepts the query")
    }

    /// The logical DAG the planner pipeline selects for the single query
    /// `root` at `accuracy`.
    pub fn selected_dag(root: Rc<OperatorNode>, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
        match selected_operator_roots(&run_all_stages(root, accuracy)).as_slice() {
            [node] => Rc::clone(node),
            other => panic!("one selected operator root: {other:?}"),
        }
    }

    /// `plan`'s cost per second (`asap_cost::plan_cost`) for queries with
    /// `entries` over `data`, under `settings`' cost model 1 or 2.
    pub fn plan_cost_per_second(
        plan: &PhysicalASAPDAG,
        entries: &[asap_types::workload::QueryWorkloadEntry],
        data: &DataWorkload,
        settings: &asap_cost::CostInputs,
    ) -> f64 {
        let recurrence: Vec<_> = entries
            .iter()
            .map(|entry| entry.recurrence.clone())
            .collect();
        let range_evaluations: Vec<_> =
            entries.iter().map(|entry| entry.range_evaluation).collect();
        let (cost, _) = asap_cost::plan_cost(
            plan,
            &asap_cost::node_map(plan),
            &recurrence,
            &range_evaluations,
            data,
            settings,
            1.0,
        )
        .unwrap();
        cost.cost_per_second.expect("cost model 1 or 2")
    }

    /// The operator roots of the plan Stage 3 built in `run`, as IR.
    fn selected_operator_roots(
        run: &asap_devtools::planning_stages::PlanningStagesRun,
    ) -> Vec<Rc<OperatorNode>> {
        let (plan, _) = run.run.selection.as_ref().expect("a plan is selected");
        plan.query_roots()
            .unwrap()
            .into_iter()
            .filter_map(|root| match root {
                QueryRoot::Operator(node) => Some(node),
                QueryRoot::Scalar(_) => None,
            })
            .collect()
    }

    /// Exact computation and each explicit single-target rule application.
    /// This fixture catalog is independent of the bounded MILP's visited plans.
    pub fn explicit_local_candidates(root: Rc<OperatorNode>) -> Vec<Rc<OperatorNode>> {
        use asap_logical_asap_optimization::test_support::{
            compose_logical_candidate, generate_per_computation_candidates,
        };
        let local = generate_per_computation_candidates(
            &asap_logical_asap_optimization::memo_of_queries(
                &[QueryRoot::Operator(root)],
                &[None],
                &Default::default(),
            )
            .unwrap(),
        )
        .unwrap();
        let mut choices = std::collections::BTreeSet::from([vec![0; local.targets.len()]]);
        for (target, options) in local.targets.iter().enumerate() {
            for choice in 1..options.choice_count() {
                let mut selected = vec![0; local.targets.len()];
                selected[target] = choice;
                choices.insert(selected);
            }
        }
        choices
            .into_iter()
            .map(|choice| {
                match compose_logical_candidate(&local, &choice)
                    .expect("explicit local rule composes")
                    .remove(0)
                {
                    QueryRoot::Operator(node) => node,
                    QueryRoot::Scalar(_) => panic!("operator fixture"),
                }
            })
            .collect()
    }

    /// One explicit plan using `family` wherever a local target supports it,
    /// keeping all other computations exact. No workload Cartesian enumeration.
    pub fn explicit_summary_candidate(
        root: Rc<OperatorNode>,
        family: asap_types::ir::schema::SketchAlgorithm,
    ) -> Rc<OperatorNode> {
        use asap_logical_asap_optimization::test_support::{
            compose_logical_candidate, generate_per_computation_candidates,
        };
        use asap_types::stages::HowToComputeAggregateMeasure;
        let local = generate_per_computation_candidates(
            &asap_logical_asap_optimization::memo_of_queries(
                &[QueryRoot::Operator(root)],
                &[None],
                &Default::default(),
            )
            .unwrap(),
        )
        .unwrap();
        let choices = local.targets.iter().map(|target| target.ways_to_compute.iter()
            .position(|candidate| matches!(candidate, HowToComputeAggregateMeasure::Summary(found) if *found == family)).unwrap_or(0)).collect::<Vec<_>>();
        match compose_logical_candidate(&local, &choices)
            .expect("explicit family choices compose")
            .remove(0)
        {
            QueryRoot::Operator(node) => node,
            QueryRoot::Scalar(_) => panic!("operator fixture"),
        }
    }

    /// The selected plan's operator roots for `root`. This is not the
    /// generated candidate catalog: rule-existence tests should use explicit
    /// fixtures.
    pub fn logical_candidates(
        root: Rc<OperatorNode>,
        accuracy: AccuracyTarget,
    ) -> Vec<Rc<OperatorNode>> {
        selected_operator_roots(&run_all_stages(root, accuracy))
    }

    /// [`logical_candidates`] with accuracy not enforced
    /// (`ignore_accuracy_guarantees`): no readout gets a required accuracy.
    /// For tests of a rewrite's structure or execution over a summary whose
    /// output has no guarantee that meets `accuracy` (e.g. a value filter
    /// over an estimate), not of accuracy acceptance.
    pub fn logical_candidates_ignoring_guarantees(
        root: Rc<OperatorNode>,
        accuracy: AccuracyTarget,
    ) -> Vec<Rc<OperatorNode>> {
        let models = asap_planner::PlanningModels::builtin().with_ignore_accuracy_guarantees(true);
        selected_operator_roots(&run_all_stages_with(root, accuracy, models))
    }

    /// Whether `node` builds a summary.
    fn is_build(node: &Rc<OperatorNode>) -> bool {
        matches!(
            node.asap(),
            Some(
                asap_types::ir::ASAPOp::SummaryAgg { .. }
                    | asap_types::ir::ASAPOp::SummaryTumblingWindowChain { .. }
            )
        )
    }

    /// `(group, candidate index)` of query `q`'s own groups' (read by `q`
    /// alone) candidates `keep` accepts; those of the topmost group (the
    /// fewest child edges below `q`'s root) first.
    pub fn own_choices(
        memo: &asap_types::stages::Memo,
        q: usize,
        keep: impl Fn(&asap_types::stages::CandidateForMemoGroup) -> bool,
    ) -> Vec<(usize, usize)> {
        let mut found: Vec<_> = memo
            .groups()
            .iter()
            .enumerate()
            .filter(|(_, group)| group.reached_by_queries() == [q])
            .flat_map(|(group, memo_group)| {
                memo_group
                    .candidates
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| keep(c))
                    .map(move |(index, _)| (group, index))
                    .collect::<Vec<_>>()
            })
            .collect();
        // Group indices don't give the order: Pass 2 appends groups at the
        // end. Depth from the root does.
        let mut depth = vec![usize::MAX; memo.groups().len()];
        let mut pending: std::collections::VecDeque<_> = memo
            .groups()
            .iter()
            .enumerate()
            .filter(|(group, _)| {
                memo.queries[q]
                    .root
                    .operator_refs()
                    .contains(&&asap_types::stages::MemoGroupId(*group as u32))
            })
            .map(|(group, _)| (group, 0))
            .collect();
        while let Some((group, d)) = pending.pop_front() {
            if depth[group] <= d {
                continue;
            }
            depth[group] = d;
            pending.extend(
                memo.groups()[group]
                    .expression
                    .children()
                    .into_iter()
                    .map(|child| (child.0 as usize, d + 1)),
            );
        }
        found.sort_by_key(|(group, _)| depth[*group]);
        found
    }

    /// The summary builds and rollups (regrouping merges) `roots` read, each
    /// once.
    pub fn builds_and_rollups(roots: &[Rc<OperatorNode>]) -> (usize, usize) {
        let mut seen = std::collections::HashSet::new();
        let (mut builds, mut rollups) = (0, 0);
        for node in roots.iter().flat_map(OperatorNode::reachable) {
            if !seen.insert(Rc::as_ptr(&node)) {
                continue;
            }
            match node.asap() {
                _ if is_build(&node) => builds += 1,
                Some(asap_types::ir::ASAPOp::SummaryMerge {
                    group_by: Some(_), ..
                }) => rollups += 1,
                _ => {}
            }
        }
        (builds, rollups)
    }
}
