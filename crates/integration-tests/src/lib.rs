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
    /// any label columns referenced in the query, in the order the SchemaResolver
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

/// Timing and physical ASAP DAG export helpers for planned candidates.
pub mod physical_export {
    use asap_types::ir::physical_export::{compile_physical_asap_dag, PhysicalASAPDAG};
    use asap_types::ir::{
        apply_materialization_timings, MaterializationAssignment, OperatorNode, TimingMemo,
    };
    use asap_types::physical::{Materialization, Medium};
    use std::rc::Rc;

    /// Time `root` under the default assignment (every sub-DAG
    /// `NotMaterialized`, so every summary computed at query time). Returns the timed copy; read `node.timing` on it.
    pub fn timed(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        timed_with(root, &MaterializationAssignment::default())
    }

    /// Time `root` with every sub-DAG `MatAtIngestion` where the DAG allows it.
    pub fn at_ingestion(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        timed_with(
            root,
            &MaterializationAssignment::with_default(Materialization::MatAtIngestion(
                Medium::Memory,
            )),
        )
    }

    fn timed_with(
        root: &Rc<OperatorNode>,
        assignment: &MaterializationAssignment,
    ) -> Rc<OperatorNode> {
        apply_materialization_timings(root, assignment, &mut TimingMemo::new())
            .expect("materialization timing failed")
    }

    /// Time `root` (default assignment), then export the physical DAG.
    pub fn physical_asap_dag(root: &Rc<OperatorNode>) -> PhysicalASAPDAG {
        compile_physical_asap_dag(&timed(root)).expect("physical ASAP DAG export failed")
    }

    /// Time `root` with every sub-DAG `MatAtIngestion` where the DAG allows
    /// it, then export the physical DAG.
    pub fn at_ingestion_physical_asap_dag(root: &Rc<OperatorNode>) -> PhysicalASAPDAG {
        compile_physical_asap_dag(&at_ingestion(root)).expect("physical ASAP DAG export failed")
    }
}

/// Plans and candidates from the planner pipeline (`run_stage1_to_stage3`) for one
/// query, with the built-in models.
pub mod planning {
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
            },
            sources,
        }
    }

    /// Every planning stage's output for the single query `root`.
    pub fn run_stage1_to_stage3(
        root: Rc<OperatorNode>,
        accuracy: AccuracyTarget,
    ) -> asap_plan_selection::Stage1ToStage3Run<usize> {
        let data = declared_data(&root);
        asap_plan_selection::run_stage1_to_stage3_with(
            vec![(0, QueryRoot::Operator(root))],
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
            asap_plan_selection::PlanningModels::builtin(),
            asap_plan_selection::Stage1ToStage3Search::exhaustive(),
        )
        .expect("Pass 1 accepts the query")
    }

    /// The logical DAG the planner pipeline selects for the single query
    /// `root` at `accuracy`.
    pub fn selected_dag(root: Rc<OperatorNode>, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
        match run_stage1_to_stage3(root, accuracy).selected_logical() {
            Some([QueryRoot::Operator(node)]) => Rc::clone(node),
            other => panic!("one selected operator root: {other:?}"),
        }
    }

    /// Exact computation and each explicit single-target rule application.
    /// This fixture catalog is independent of the bounded MILP's visited plans.
    pub fn explicit_local_candidates(root: Rc<OperatorNode>) -> Vec<Rc<OperatorNode>> {
        use asap_logical_asap_optimization::per_computation_candidates::{
            compose_logical_candidate, generate_per_computation_candidates,
        };
        let local =
            generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))]).unwrap();
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
                    .1
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
        use asap_logical_asap_optimization::per_computation_candidates::{
            compose_logical_candidate, generate_per_computation_candidates, ComputationCandidate,
        };
        let local =
            generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))]).unwrap();
        let choices = local.targets.iter().map(|target| target.choices.candidates.iter()
            .position(|candidate| matches!(candidate, ComputationCandidate::Summary(found) if *found == family)).unwrap_or(0)).collect::<Vec<_>>();
        match compose_logical_candidate(&local, &choices)
            .expect("explicit family choices compose")
            .remove(0)
            .1
        {
            QueryRoot::Operator(node) => node,
            QueryRoot::Scalar(_) => panic!("operator fixture"),
        }
    }

    /// Logical plans visited by the bounded search for `root`, in archive order.
    /// This is not the generated candidate catalog. Rule-existence tests should
    /// use explicit fixtures; failed compositions in this archive still panic.
    pub fn logical_candidates(
        root: Rc<OperatorNode>,
        accuracy: AccuracyTarget,
    ) -> Vec<Rc<OperatorNode>> {
        require_composed_operator_candidates(
            run_stage1_to_stage3(root, accuracy).stage1_logical_asap_optimization,
        )
    }

    /// Fail on rejected composition rather than silently skipping a candidate.
    pub fn require_composed_operator_candidates(
        candidates: Vec<asap_plan_selection::search::LogicalCandidate>,
    ) -> Vec<Rc<OperatorNode>> {
        candidates
            .into_iter()
            .filter_map(|candidate| {
                match candidate
                    .roots
                    .unwrap_or_else(|e| panic!("{} does not compose: {e}", candidate.id))
                    .remove(0)
                {
                    QueryRoot::Operator(node) => Some(node),
                    QueryRoot::Scalar(_) => None,
                }
            })
            .collect()
    }
    /// Pass 1, local panes for `entries`' recurrences and every Pass 2 rule
    /// on the seed grids (no query pruning). Rule tests read a group's
    /// candidates by `CandidateOrigin`.
    pub fn expanded_memo(
        roots: Vec<(usize, QueryRoot)>,
        entries: &[asap_types::workload::QueryWorkloadEntry],
    ) -> asap_logical_asap_optimization::memo::candidates::CandidateMemo {
        use asap_logical_asap_optimization::memo::{
            candidates::try_populate_pass1, state_configurations::PaneWidthDomain,
            state_groups::expand_concrete_summary_state_groups_in_domain,
            window_candidates::add_local_window_composition, LogicalMemo,
        };
        use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
        let initial = LogicalMemo::from_roots(&roots);
        let inventory = generate_per_computation_candidates(roots).unwrap();
        let mut memo = try_populate_pass1(initial, &inventory).unwrap();
        add_local_window_composition(&mut memo, entries);
        expand_concrete_summary_state_groups_in_domain(memo, PaneWidthDomain::SeedGrids)
            .unwrap()
            .memo
    }

    /// One plan of `memo`'s first `queries` queries (`memo::enumeration::compose`):
    /// each query's first root group's DAG, every group `choice` does not
    /// name implemented by its first candidate.
    pub fn compose_memo_plan(
        memo: &asap_logical_asap_optimization::memo::candidates::CandidateMemo,
        queries: usize,
        choice: &std::collections::BTreeMap<usize, usize>,
    ) -> Vec<Rc<OperatorNode>> {
        use asap_logical_asap_optimization::memo::{enumeration::compose, MemoGroupId};
        let choice = choice
            .iter()
            .map(|(&group, &index)| (MemoGroupId(group as u32), index))
            .collect();
        let roots = compose(memo, &choice).expect("the choice composes");
        (0..queries)
            .map(|query| {
                let (_, root) = roots
                    .iter()
                    .find(|(group, _)| memo.groups[group.0 as usize].roots_of.contains(&query))
                    .expect("the query's root group");
                Rc::clone(root)
            })
            .collect()
    }

    /// Stage 2 and plan selection over fixed workload plans, without a timed
    /// search: each `(name, roots)` is implemented by Stage 2 (plan ids start
    /// with `name`), and all are priced together.
    pub fn select_among(
        variants: &[(&str, Vec<QueryRoot>)],
        entries: &[asap_types::workload::QueryWorkloadEntry],
        data: &DataWorkload,
    ) -> (
        Vec<asap_physical_asap_optimization::PhysicalWorkloadPlan>,
        asap_plan_selection::Selection,
    ) {
        let models = asap_plan_selection::PlanningModels::builtin();
        let mut physical = Vec::new();
        for (name, roots) in variants {
            // Composed nodes lose their guarantees; recompose them as
            // `run_explicit_candidates` does.
            let roots =
                asap_logical_asap_optimization::per_computation_candidates::recompose_guarantees(
                    roots,
                )
                .unwrap_or_else(|error| panic!("{name} composes guarantees: {error}"));
            let mut plans = asap_physical_asap_optimization::optimize_physical_asap(
                name,
                &roots,
                entries,
                data,
                models.deployment_capabilities,
            )
            .unwrap_or_else(|error| panic!("Stage 2 implements {name}: {error}"));
            for (index, plan) in plans.iter_mut().enumerate() {
                plan.id = format!("{name}-{index}");
            }
            physical.extend(plans);
        }
        let selection = asap_plan_selection::select_plan(&physical, entries, data, models).unwrap();
        (physical, selection)
    }

    /// Whether `node` builds a summary.
    fn is_build(node: &Rc<OperatorNode>) -> bool {
        matches!(
            node.asap(),
            Some(
                asap_types::ir::ASAPOp::SummaryAgg { .. }
                    | asap_types::ir::ASAPOp::PaneChain { .. }
            )
        )
    }

    /// `(group, candidate index)` of query `q`'s own groups' (read by `q`
    /// alone) candidates `keep` accepts; those of the topmost group (the
    /// fewest child edges below `q`'s root) first.
    pub fn own_choices(
        memo: &asap_logical_asap_optimization::memo::candidates::CandidateMemo,
        q: usize,
        keep: impl Fn(
            &asap_logical_asap_optimization::memo::candidates::ImplementationCandidate,
        ) -> bool,
    ) -> Vec<(usize, usize)> {
        let mut found: Vec<_> = memo
            .candidates
            .iter()
            .enumerate()
            .filter(|(group, _)| memo.groups[*group].queries == [q])
            .flat_map(|(group, candidates)| {
                candidates
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| keep(c))
                    .map(move |(index, _)| (group, index))
                    .collect::<Vec<_>>()
            })
            .collect();
        // Group indices don't give the order: Pass 2 appends groups at the
        // end. Depth from the root does.
        let mut depth = vec![usize::MAX; memo.groups.len()];
        let mut pending: std::collections::VecDeque<_> = memo
            .groups
            .iter()
            .enumerate()
            .filter(|(_, group)| group.roots_of.contains(&q))
            .map(|(group, _)| (group, 0))
            .collect();
        while let Some((group, d)) = pending.pop_front() {
            if depth[group] <= d {
                continue;
            }
            depth[group] = d;
            pending.extend(
                memo.groups[group]
                    .children
                    .iter()
                    .map(|child| (child.0 as usize, d + 1)),
            );
        }
        found.sort_by_key(|(group, _)| depth[*group]);
        found
    }

    /// The selected plan's roots among fixed plans of `memo`'s queries, and
    /// the selection's lowest prices of `separate` and of the `shared` plans:
    /// - `separate{k}`: each query reads its own summary build, its topmost
    ///   own group's `k`-th Pass 1 candidate that builds one (the last when
    ///   it has fewer);
    /// - `shared{g}`: every query reads the summary state group `g`, its own
    ///   estimate or a regrouping merge of it, when every query can.
    pub fn select_rollup(
        memo: &asap_logical_asap_optimization::memo::candidates::CandidateMemo,
        queries: usize,
        entries: &[asap_types::workload::QueryWorkloadEntry],
        data: &DataWorkload,
    ) -> (Vec<Rc<OperatorNode>>, f64, f64) {
        let builds = |node: &Rc<OperatorNode>| OperatorNode::reachable(node).iter().any(is_build);
        let own_builds: Vec<Vec<(usize, usize)>> = (0..queries)
            .map(|q| {
                let found = own_choices(memo, q, |c| {
                    c.origin
                        == asap_logical_asap_optimization::memo::candidates::CandidateOrigin::Pass1
                        && builds(&c.node)
                });
                let top = found
                    .first()
                    .unwrap_or_else(|| panic!("Q{} has a summary candidate", q + 1))
                    .0;
                found
                    .into_iter()
                    .filter(|(group, _)| *group == top)
                    .collect()
            })
            .collect();
        let separate_plans = own_builds.iter().map(Vec::len).max().unwrap_or(0);
        let mut plans: Vec<(String, Vec<Rc<OperatorNode>>)> = (0..separate_plans)
            .map(|k| {
                let choice = own_builds
                    .iter()
                    .map(|found| found[k.min(found.len() - 1)])
                    .collect();
                (
                    format!("separate{k}"),
                    compose_memo_plan(memo, queries, &choice),
                )
            })
            .collect();
        for (state, group) in memo.groups.iter().enumerate() {
            if !is_build(&group.node) || group.queries.len() < queries {
                continue;
            }
            let reading: Option<std::collections::BTreeMap<usize, usize>> = (0..queries)
                .map(|q| {
                    own_choices(memo, q, |c| {
                        c.children.iter().any(|child| child.0 as usize == state)
                    })
                    .into_iter()
                    .next()
                })
                .collect();
            if let Some(choice) = reading {
                plans.push((
                    format!("shared{state}"),
                    compose_memo_plan(memo, queries, &choice),
                ));
            }
        }
        assert!(
            plans.len() > separate_plans,
            "a plan in which every query reads one build"
        );
        let named: Vec<(&str, Vec<QueryRoot>)> = plans
            .iter()
            .map(|(name, roots)| {
                (
                    name.as_str(),
                    roots.iter().cloned().map(QueryRoot::Operator).collect(),
                )
            })
            .collect();
        let (physical, selection) = select_among(&named, entries, data);
        let selected = physical
            .iter()
            .find(|plan| plan.id == selection.selected)
            .expect("the selected plan");
        let roots = plans
            .iter()
            .find(|(name, _)| *name == selected.from_logical)
            .map(|(_, roots)| roots.clone())
            .unwrap();
        let cheapest = |plans: &[(String, Vec<Rc<OperatorNode>>)]| {
            plans
                .iter()
                .map(|(name, _)| cheapest_of(&selection, name))
                .fold(f64::INFINITY, f64::min)
        };
        let separate = cheapest(&plans[..separate_plans]);
        // Every separate plan rejected would make `shared < separate` hold
        // trivially.
        assert!(separate.is_finite(), "a valid plan with a build per query");
        (roots, separate, cheapest(&plans[separate_plans..]))
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

    /// The lowest price per second among `selection`'s plans whose id starts
    /// with `name`.
    pub fn cheapest_of(selection: &asap_plan_selection::Selection, name: &str) -> f64 {
        selection
            .costs
            .iter()
            .filter(|(id, _)| id.starts_with(&format!("{name}-")))
            .filter_map(|(_, cost)| cost.cost_per_second)
            .fold(f64::INFINITY, f64::min)
    }

    /// Apply sharing rules and Stage 2 to a small explicitly supplied fixture set,
    /// then price it with the production evaluator. No workload product search.
    pub fn explicit_assignment_plans(
        inventory: asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidates<usize>,
        assignments: Vec<Vec<usize>>,
        entries: &[asap_types::workload::QueryWorkloadEntry],
        data: &asap_types::workload::DataWorkload,
    ) -> asap_plan_selection::Stage1ToStage3Run<usize> {
        use asap_logical_asap_optimization::per_computation_candidates::recompose_guarantees;
        use asap_logical_asap_optimization::workload_candidates::stage1_candidates_of_choice;
        use asap_plan_selection::{search::LogicalCandidate, PlanningModels, Stage1ToStage3Run};
        let mut logical = Vec::new();
        let mut physical = Vec::new();
        for assignment in assignments {
            let source_position = logical.len();
            for mut candidate in
                stage1_candidates_of_choice(&inventory, entries, &assignment, &|_| true)
            {
                if let Some(pass2) = &mut candidate.pass2 {
                    pass2.from_pass1_position = source_position;
                }
                let roots = recompose_guarantees(&candidate.roots.unwrap()).unwrap();
                let id = format!("L{}", logical.len() + 1);
                physical.extend(
                    asap_physical_asap_optimization::optimize_physical_asap(
                        &id,
                        &roots,
                        entries,
                        data,
                        PlanningModels::builtin().deployment_capabilities,
                    )
                    .expect("explicit rewrite is executable"),
                );
                logical.push(LogicalCandidate {
                    id,
                    choice: candidate.choice,
                    roots: Ok(roots),
                    pass2: candidate.pass2,
                });
            }
        }
        for (index, plan) in physical.iter_mut().enumerate() {
            plan.id = format!("fixture-{index}");
        }
        let selection =
            asap_plan_selection::select_plan(&physical, entries, data, PlanningModels::builtin())
                .unwrap();
        let selected = physical
            .iter()
            .find(|plan| plan.id == selection.selected)
            .unwrap()
            .clone();
        let source = logical
            .iter()
            .find(|candidate| candidate.id == selected.from_logical)
            .unwrap();
        let selected_logical = LogicalCandidate {
            id: source.id.clone(),
            choice: source.choice.clone(),
            roots: Ok(source.roots.as_ref().unwrap().clone()),
            pass2: source.pass2.clone(),
        };
        Stage1ToStage3Run {
            combinations: inventory.combinations(),
            stage1_pass1_per_computation: inventory,
            stage1_logical_asap_optimization: logical,
            stage2_physical_asap_optimization: physical,
            stage2_pruned: vec![],
            stage3_selected_logical: Some(selected.roots.clone()),
            stage3_selected: Some(selected),
            stage1_selected: Some(selected_logical),
            stage3_plan_selection: Ok(selection),
            memo_steps: None,
        }
    }
}
