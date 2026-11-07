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
        match run_all_stages(root, accuracy).run.selected_plan.as_deref() {
            Some([QueryRoot::Operator(node)]) => Rc::clone(node),
            other => panic!("one selected operator root: {other:?}"),
        }
    }

    /// Exact computation and each way Pass 1 and Pass 2 offer for the
    /// root group of `root`, each realized as one plan: for each root
    /// candidate run at query time and not materialized, each input reads
    /// its first admissible producer of the same kind
    /// ([`realize_from_root_candidates`]). Accuracy guarantees are ignored,
    /// so every summary Pass 1 offers stays in the memo.
    pub fn explicit_local_candidates(root: Rc<OperatorNode>) -> Vec<Rc<OperatorNode>> {
        realize_from_root_candidates(root, |_| false)
            .into_iter()
            .map(|plan| plan_root(&plan))
            .collect()
    }

    /// One plan of `root` that reads `family` wherever an input can: each
    /// input prefers a producer whose plan builds a `family` sketch
    /// ([`realize_from_root_candidates`]); the first such plan.
    pub fn explicit_summary_candidate(
        root: Rc<OperatorNode>,
        family: asap_types::ir::schema::SketchAlgorithm,
    ) -> Rc<OperatorNode> {
        let builds_family =
            move |operator: &asap_types::ir::Operator<asap_types::stages::MemoGroupId>| {
                matches!(
                    operator,
                    asap_types::ir::Operator::ASAP(asap_types::ir::ASAPOp::SummaryAgg {
                        family: asap_types::ir::schema::FieldDataType::Sketch(algorithm, _),
                        ..
                    }) if *algorithm == family
                )
            };
        realize_from_root_candidates(root, builds_family)
            .iter()
            .map(plan_root)
            .find(|plan| {
                asap_types::ir::OperatorNode::reachable(plan)
                    .iter()
                    .any(|node| {
                        matches!(
                            node.asap(),
                            Some(asap_types::ir::ASAPOp::SummaryAgg {
                                family: asap_types::ir::schema::FieldDataType::Sketch(algorithm, _),
                                ..
                            }) if *algorithm == family
                        )
                    })
            })
            .unwrap_or_else(|| panic!("no plan builds a {family:?} sketch"))
    }

    /// The single operator root of a realized plan.
    fn plan_root(plan: &asap_types::stages::RealizedPlan) -> Rc<OperatorNode> {
        match plan.plan.roots.as_slice() {
            [QueryRoot::Operator(node)] => Rc::clone(node),
            other => panic!("one operator root: {other:?}"),
        }
    }

    /// For each candidate of `root`'s output group run at query time and
    /// not materialized, the plan Stage 2's realizer builds when each input
    /// reads, among its admissible producers run at query time and not
    /// materialized, the first whose own plan contains an operator
    /// `preferred` accepts, else the first; each a distinct plan, in
    /// candidate order. Stages run with accuracy guarantees ignored and a
    /// target of 0.05, so every summary Pass 1 offers stays.
    pub fn realize_from_root_candidates(
        root: Rc<OperatorNode>,
        preferred: impl Fn(&asap_types::ir::Operator<asap_types::stages::MemoGroupId>) -> bool,
    ) -> Vec<asap_types::stages::RealizedPlan> {
        use asap_types::physical::{ExecutionTiming, Lifetime};
        use asap_types::stages::{
            AssignmentRealizer, DagOfSelectedCandidates, PhysicalASAPMemoCandidates,
            PhysicalCandidateId, SelectedEdge, SelectedNodeId,
        };
        let models = asap_planner::PlanningModels::builtin().with_ignore_accuracy_guarantees(true);
        let data = declared_data(&root);
        let entries = [asap_types::workload::QueryWorkloadEntry {
            query: Default::default(),
            requirements: asap_types::workload::QueryRequirements {
                accuracy: AccuracyTarget::max_error(0.05),
                max_latency_ms: None,
            },
            predictability: Default::default(),
            recurrence: Default::default(),
            range_evaluation: None,
        }];
        let stages = run_all_stages_with(root, entries[0].requirements.accuracy.clone(), models);
        let Some(physical) = stages.run.physical_asap_candidates.as_ref() else {
            return Vec::new();
        };
        let memo = &physical.logical_candidates.memo;
        let edges: Vec<_> = physical.edges().map(|(_, edge)| edge).collect();
        let unmaterialized_at_query_time = |id: PhysicalCandidateId| {
            let properties = physical.physical_candidates[id.0].physical_properties;
            properties.timing() == ExecutionTiming::QueryTime
                && properties.lifetime() == Lifetime::None
        };
        let inputs_of = |id: PhysicalCandidateId| {
            memo.candidate(physical.physical_candidates[id.0].logical_candidate_id)
                .expression
                .operator
                .children()
                .len()
        };
        // Whether the plan below `id`, choosing first producers, contains a
        // preferred operator (depth-bounded: the memo may have cycles).
        fn contains_preferred(
            physical: &PhysicalASAPMemoCandidates,
            edges: &[asap_types::stages::PhysicalEdge],
            id: PhysicalCandidateId,
            preferred: &dyn Fn(&asap_types::ir::Operator<asap_types::stages::MemoGroupId>) -> bool,
            depth: usize,
        ) -> bool {
            let memo = &physical.logical_candidates.memo;
            let operator = &memo
                .candidate(physical.physical_candidates[id.0].logical_candidate_id)
                .expression
                .operator;
            if preferred(operator) {
                return true;
            }
            depth > 0
                && (0..operator.children().len()).any(|input| {
                    physical
                        .admissible_edges_into(id, input)
                        .iter()
                        .any(|edge| {
                            contains_preferred(
                                physical,
                                edges,
                                edges[edge.0].producer_candidate_id,
                                preferred,
                                depth - 1,
                            )
                        })
                })
        }
        let mut plans: Vec<asap_types::stages::RealizedPlan> = Vec::new();
        let realizer = asap_stage2_physical_optimization::Stage2AssignmentRealizer {
            entries: &entries,
            data: &data,
        };
        let output_group = match &physical.query_roots[0] {
            QueryRoot::Operator(group) => *group,
            QueryRoot::Scalar(_) => return Vec::new(),
        };
        for root_candidate in (0..physical.physical_candidates.len())
            .map(PhysicalCandidateId)
            .filter(|&id| {
                physical.physical_candidates[id.0]
                    .logical_candidate_id
                    .group
                    == output_group
                    && unmaterialized_at_query_time(id)
            })
        {
            let mut selected = DagOfSelectedCandidates {
                candidate_of_node: vec![root_candidate],
                edges: Vec::new(),
                node_of_output_reference: vec![SelectedNodeId(0)],
            };
            let mut pending = vec![(root_candidate, SelectedNodeId(0), 0usize)];
            let mut feasible = true;
            while let Some((candidate, node, depth)) = pending.pop() {
                for input in 0..inputs_of(candidate) {
                    let producers: Vec<PhysicalCandidateId> = physical
                        .admissible_edges_into(candidate, input)
                        .iter()
                        .map(|edge| edges[edge.0].producer_candidate_id)
                        .filter(|&producer| unmaterialized_at_query_time(producer))
                        .collect();
                    let producer = producers
                        .iter()
                        .copied()
                        .find(|&producer| {
                            contains_preferred(physical, &edges, producer, &preferred, 8)
                        })
                        .or_else(|| producers.first().copied());
                    let (Some(producer), true) = (producer, depth < 64) else {
                        feasible = false;
                        break;
                    };
                    let producer_node = match selected
                        .candidate_of_node
                        .iter()
                        .position(|&known| known == producer)
                    {
                        Some(known) => SelectedNodeId(known),
                        None => {
                            selected.candidate_of_node.push(producer);
                            let new = SelectedNodeId(selected.candidate_of_node.len() - 1);
                            pending.push((producer, new, depth + 1));
                            new
                        }
                    };
                    selected.edges.push(SelectedEdge {
                        producer: producer_node,
                        consumer: node,
                        input_index_of_consumer: input,
                    });
                }
                if !feasible {
                    break;
                }
            }
            if !feasible {
                continue;
            }
            if let Ok(plan) = realizer.realize(physical, &selected) {
                if !plans
                    .iter()
                    .any(|known| known.plan.roots == plan.plan.roots)
                {
                    plans.push(plan);
                }
            }
        }
        plans
    }

    /// Logical plans visited by the bounded search for `root`, in archive order.
    /// This is not the generated candidate catalog. Rule-existence tests should
    /// use explicit fixtures; failed compositions in this archive still panic.
    pub fn logical_candidates(
        root: Rc<OperatorNode>,
        accuracy: AccuracyTarget,
    ) -> Vec<Rc<OperatorNode>> {
        require_composed_operator_candidates(run_all_stages(root, accuracy).logical_candidates())
    }

    /// [`logical_candidates`] with every output accepted whatever its
    /// accuracy guarantee (`ignore_accuracy_guarantees`), so Pass 1 keeps
    /// the summaries no query can use for `accuracy`
    /// (`PruneReason::NoErrorBoundForQueryTarget`). For tests of a
    /// rewrite's structure or execution over a summary whose output has no
    /// guarantee that meets `accuracy` (e.g. a value filter over an
    /// estimate), not of accuracy acceptance.
    pub fn logical_candidates_ignoring_guarantees(
        root: Rc<OperatorNode>,
        accuracy: AccuracyTarget,
    ) -> Vec<Rc<OperatorNode>> {
        let models = asap_planner::PlanningModels::builtin().with_ignore_accuracy_guarantees(true);
        require_composed_operator_candidates(
            run_all_stages_with(root, accuracy, models).logical_candidates(),
        )
    }

    /// Fail on rejected composition rather than silently skipping a candidate.
    pub fn require_composed_operator_candidates(
        candidates: Vec<asap_devtools::planning_stages::ShownLogicalCandidate>,
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
                memo.query_roots[q]
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
