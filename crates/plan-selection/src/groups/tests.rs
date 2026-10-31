use std::rc::Rc;

use super::*;
use asap_logical_asap_optimization::PaneWidthDomain;
use asap_types::ir::properties::{AccuracyMetric, BoundExpr, ProbabilityExpr};
use asap_types::ir::scalar::expr_ir::ScalarValue;
use asap_types::ir::schema::SketchAlgorithm;
use asap_types::ir::schema::{DataType, Field};
use asap_types::ir::{QueryRoot, ScalarExpr};
use asap_types::physical::Medium;
use asap_types::stages::{
    CandidateForMemoGroupId, OperatorNodeOfCandidateForMemoGroup,
    OperatorNodeOfCandidateForMemoGroupId, PhysicalCandidate, ProducerCandidates,
};

// ---------------------------------------------------------------------------
// Hand-built catalogs
// ---------------------------------------------------------------------------

/// A one-row `VALUES` node of `value`.
fn values_node(value: f64) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Values {
        rows: vec![vec![ScalarExpr::Literal(ScalarValue::Float64(value))]],
        schema: Schema::lifted(vec![Field::plain("value", DataType::Float64, false)], None),
    }))
    .unwrap()
}

/// A `VALUES` node of `rows` one-column rows.
fn rows_node(rows: usize) -> Rc<OperatorNode> {
    let schema = Schema::lifted(vec![Field::plain("value", DataType::Float64, false)], None);
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Values {
        rows: vec![vec![ScalarExpr::Literal(ScalarValue::Float64(1.0))]; rows],
        schema,
    }))
    .unwrap()
}

/// A stateless one-input reader of `child`.
fn reader_of(child: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Dedup {
        cols: Vec::new(),
        child: Rc::clone(child),
    }))
    .unwrap()
}

/// A catalog of `nodes`, node `i` in memo group `nodes[i].1` (each node its
/// own candidate of that group, all implementation roots), with
/// `candidates` and one query root per entry of `roots`.
fn catalog(
    nodes: Vec<(Rc<OperatorNode>, u32)>,
    candidates: Vec<PhysicalCandidate>,
    roots: Vec<QueryRoot<MemoGroupId>>,
) -> PhysicalASAPMemoCandidates {
    let mut index_in_group: HashMap<u32, usize> = HashMap::new();
    PhysicalASAPMemoCandidates::new(
        nodes
            .into_iter()
            .map(|(physical_node, group)| {
                let index = index_in_group.entry(group).or_default();
                *index += 1;
                OperatorNodeOfCandidateForMemoGroup {
                    candidates_for_memo_group: vec![CandidateForMemoGroupId {
                        group: MemoGroupId(group),
                        index: *index - 1,
                    }],
                    is_implementation_root: true,
                    accuracy_budget_parts: 1,
                    physical_node,
                }
            })
            .collect(),
        candidates,
        roots,
        Vec::new(),
    )
}

/// One candidate of operator node `node` at query time, not materialized,
/// reading `producers` per input, that may produce a query's output when
/// `output`.
fn candidate(node: usize, producers: &[&[usize]], output: bool) -> PhysicalCandidate {
    PhysicalCandidate {
        operator_node: OperatorNodeOfCandidateForMemoGroupId(node),
        timing: ExecutionTiming::QueryTime,
        materialization: Materialization::NotMaterialized,
        producer_candidates_per_input: producers
            .iter()
            .map(|input| {
                input
                    .iter()
                    .map(|&id| PhysicalCandidateId(id))
                    .collect::<ProducerCandidates>()
            })
            .collect(),
        may_produce_query_output: output,
        may_keep_across_evaluations_per_sole_reader_query: Vec::new(),
    }
}

fn root(group: u32) -> QueryRoot<MemoGroupId> {
    QueryRoot::Operator(MemoGroupId(group))
}

fn prepared(physical: &PhysicalASAPMemoCandidates) -> MilpInputs {
    prepare_milp_inputs(
        physical,
        &[],
        &DataWorkload::default(),
        &CostInputs::default(),
    )
}

fn id(candidate: usize) -> PhysicalCandidateId {
    PhysicalCandidateId(candidate)
}

// ---------------------------------------------------------------------------
// Dominance
// ---------------------------------------------------------------------------

fn usage() -> CandidateUsage {
    CandidateUsage {
        cpu_ops_per_execution: 10.0,
        scanned_bytes_per_execution: 10.0,
        work_ms_per_execution: 10.0,
        own_latency_ms: 10.0,
        cost_per_execution: Some(1.0),
        standing_cost_per_second: Some(1.0),
        average_memory_bytes: 10.0,
        peak_memory_bytes: 10,
        disk_bytes: 10,
        held_bytes: 10,
        raw_data_kept_bytes: 10,
        maintained_window_bytes: 10,
        output_rows: 10,
        output_bytes: 10,
    }
}

fn data_with(usage: CandidateUsage) -> CandidateData {
    CandidateData {
        approximate: true,
        accuracy_budget_parts: 1,
        usage: Ok(usage),
        ..CandidateData::default()
    }
}

fn relative(error: f64, failure_probability: f64) -> AccuracyGuarantee {
    AccuracyGuarantee::new(
        AccuracyMetric::Relative,
        BoundExpr::Constant { value: error },
        ProbabilityExpr::Constant {
            value: failure_probability,
        },
        Vec::new(),
    )
}

// Candidate dominance needs every condition: a candidate with equal keys, a
// guarantee at least as strong and no larger number dominates; changing any one condition stops it.
#[test]
fn dominance_needs_every_condition() {
    let producers = vec![vec![PhysicalCandidateId(7)]];
    let other_producers = vec![vec![PhysicalCandidateId(8)]];
    let keep = vec![false];
    let other_keep = vec![true];
    let schema = rows_node(1).schema.clone();
    let other_schema = Schema::lifted(vec![Field::plain("v", DataType::Int64, false)], None);
    let strong = relative(0.01, 0.01);
    let weak = relative(0.02, 0.01);
    let values_kind =
        OperatorKindForReaders::Other(std::mem::discriminant(&rows_node(1).operator), "Values");
    let key = DominanceKey {
        group: MemoGroupId(3),
        timing: ExecutionTiming::QueryTime,
        materialization: Materialization::NotMaterialized,
        shareable: false,
        producer_candidates_per_input: &producers,
        readers: vec![(PhysicalCandidateId(9), 0)],
        output_references: vec![0],
        reader_constraint_ids: vec![1],
        may_keep_across_evaluations_per_sole_reader_query: &keep,
        output_schema: &schema,
        operator_kind: values_kind,
    };
    let profile = |guarantee| DominanceProfile {
        same: key.clone(),
        guarantee,
    };
    let better = profile(Some(&strong));
    let worse = profile(Some(&weak));
    let mut worse_usage = usage();
    worse_usage.cpu_ops_per_execution = 20.0;
    let (better_data, worse_data) = (data_with(usage()), data_with(worse_usage));
    assert!(dominates((&better, &better_data), (&worse, &worse_data)));
    assert!(!dominates((&worse, &worse_data), (&better, &better_data)));

    // Each key field.
    let changed_keys = [
        (
            "group",
            DominanceKey {
                group: MemoGroupId(4),
                ..key.clone()
            },
        ),
        (
            "timing",
            DominanceKey {
                timing: ExecutionTiming::IngestionTime,
                ..key.clone()
            },
        ),
        (
            "materialization",
            DominanceKey {
                materialization: Materialization::MatAtQuery(Medium::Memory),
                ..key.clone()
            },
        ),
        (
            "shareable",
            DominanceKey {
                shareable: true,
                ..key.clone()
            },
        ),
        (
            "producers",
            DominanceKey {
                producer_candidates_per_input: &other_producers,
                ..key.clone()
            },
        ),
        (
            "readers",
            DominanceKey {
                readers: vec![(PhysicalCandidateId(10), 0)],
                ..key.clone()
            },
        ),
        (
            "outputs",
            DominanceKey {
                output_references: vec![1],
                ..key.clone()
            },
        ),
        (
            "reader constraints",
            DominanceKey {
                reader_constraint_ids: vec![2],
                ..key.clone()
            },
        ),
        (
            "keep flags",
            DominanceKey {
                may_keep_across_evaluations_per_sole_reader_query: &other_keep,
                ..key.clone()
            },
        ),
        (
            "schema",
            DominanceKey {
                output_schema: &other_schema,
                ..key.clone()
            },
        ),
        (
            "operator kind",
            DominanceKey {
                operator_kind: OperatorKindForReaders::PaneChain {
                    pane_width_ms: 60_000,
                    pane_count: 5,
                },
                ..key.clone()
            },
        ),
    ];
    for (name, changed) in changed_keys {
        let by = DominanceProfile {
            same: changed,
            ..better.clone()
        };
        assert!(
            !dominates((&by, &better_data), (&worse, &worse_data)),
            "{name}"
        );
    }
    // (approx, parts), guarantee.
    let mut data = better_data.clone();
    data.approximate = false;
    assert!(
        !dominates((&better, &data), (&worse, &worse_data)),
        "approx"
    );
    let mut data = better_data.clone();
    data.accuracy_budget_parts = 2;
    assert!(!dominates((&better, &data), (&worse, &worse_data)), "parts");
    let by = profile(Some(&weak));
    let class = profile(Some(&strong));
    assert!(
        !dominates((&by, &better_data), (&class, &worse_data)),
        "guarantee"
    );
    // Each number.
    for number in 0..compared_numbers(&usage()).len() {
        let mut larger = usage();
        match number {
            0 => larger.cpu_ops_per_execution = 30.0,
            1 => larger.scanned_bytes_per_execution = 30.0,
            2 => larger.work_ms_per_execution = 30.0,
            3 => larger.own_latency_ms = 30.0,
            4 => larger.average_memory_bytes = 30.0,
            5 => larger.peak_memory_bytes = 30,
            6 => larger.disk_bytes = 30,
            7 => larger.held_bytes = 30,
            8 => larger.raw_data_kept_bytes = 30,
            9 => larger.maintained_window_bytes = 30,
            10 => larger.output_rows = 30,
            _ => larger.output_bytes = 30,
        }
        assert!(
            !dominates((&better, &data_with(larger)), (&worse, &worse_data)),
            "number {number}"
        );
    }
    // A plan-dependent or uncosted usage is never compared.
    let mut data = better_data.clone();
    data.usage_depends_on = vec![UsageContext::ProducerOutput];
    assert!(
        !dominates((&better, &data), (&worse, &worse_data)),
        "context"
    );
    let mut data = better_data.clone();
    data.usage = Err("uncosted".into());
    assert!(!dominates((&better, &data), (&worse, &worse_data)), "error");
}

// A guarantee is at least as strong when equal, exact, or no larger in one
// metric and scope; different metrics and unknown bounds do not compare.
#[test]
fn guarantee_strength_compares_constant_bounds_in_one_metric() {
    let strong = relative(0.01, 0.01);
    let weak = relative(0.02, 0.02);
    let exact = AccuracyGuarantee::exact("exact");
    let rank = AccuracyGuarantee::new(
        AccuracyMetric::Rank,
        BoundExpr::Constant { value: 0.001 },
        ProbabilityExpr::Zero,
        Vec::new(),
    );
    assert!(guarantee_at_least_as_strong(Some(&strong), Some(&weak)));
    assert!(!guarantee_at_least_as_strong(Some(&weak), Some(&strong)));
    assert!(guarantee_at_least_as_strong(Some(&exact), Some(&weak)));
    assert!(guarantee_at_least_as_strong(None, None));
    assert!(!guarantee_at_least_as_strong(Some(&strong), None));
    assert!(!guarantee_at_least_as_strong(Some(&rank), Some(&weak)));
}

// Among dominated candidates the dominating one is reported; of two equal
// candidates the smaller id is kept; a candidate better in one number and worse in
// another is kept.
#[test]
fn pruning_keeps_the_smallest_equal_class_and_incomparable_ones() {
    let producers = vec![vec![PhysicalCandidateId(9)]];
    let keep = Vec::new();
    let schema = rows_node(1).schema.clone();
    let key = DominanceKey {
        group: MemoGroupId(0),
        timing: ExecutionTiming::QueryTime,
        materialization: Materialization::NotMaterialized,
        shareable: false,
        producer_candidates_per_input: &producers,
        readers: Vec::new(),
        output_references: vec![0],
        reader_constraint_ids: Vec::new(),
        may_keep_across_evaluations_per_sole_reader_query: &keep,
        output_schema: &schema,
        operator_kind: OperatorKindForReaders::Other(
            std::mem::discriminant(&rows_node(1).operator),
            "Values",
        ),
    };
    let profiles = vec![
        DominanceProfile {
            same: key.clone(),
            guarantee: None,
        };
        4
    ];
    let mut worse = usage();
    worse.output_rows = 20;
    let mut incomparable = usage();
    incomparable.cpu_ops_per_execution = 5.0;
    incomparable.held_bytes = 20;
    let data = vec![
        data_with(usage()),
        data_with(usage()),
        data_with(worse),
        data_with(incomparable),
    ];
    let pruned = prune_dominated_candidates(&profiles, &data);
    let summary: Vec<_> = pruned
        .iter()
        .map(|pruned| (pruned.class.0, pruned.dominated_by.0))
        .collect();
    assert_eq!(summary, vec![(1, 0), (2, 0)]);
}

// Through `prepare_milp_inputs`: of two deduplications in one memo group
// over one leaf, read by one reader and costing the same, the later one is
// deleted with its links, and the reader reads the other.
#[test]
fn a_dominated_class_is_deleted_with_its_links() {
    let schema = Schema::lifted(
        vec![
            Field::plain("a", DataType::Float64, false),
            Field::plain("b", DataType::Float64, false),
        ],
        None,
    );
    let leaf = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Values {
        rows: vec![vec![ScalarExpr::Literal(ScalarValue::Float64(1.0)); 2]; 4],
        schema,
    }))
    .unwrap();
    let dedup = |cols: Vec<usize>| {
        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Dedup {
            cols,
            child: Rc::clone(&leaf),
        }))
        .unwrap()
    };
    // Two equal nodes: equal schemas and costs.
    let (first, second) = (dedup(vec![0]), dedup(vec![0]));
    let reader = reader_of(&first);
    let physical = catalog(
        vec![(leaf, 0), (first, 1), (second, 1), (reader, 2)],
        vec![
            candidate(0, &[], false),
            candidate(1, &[&[0]], false),
            candidate(2, &[&[0]], false),
            candidate(3, &[&[1, 2]], true),
        ],
        vec![root(2)],
    );
    let inputs = prepared(&physical);
    let (leaf, kept, deleted, top) = (id(0), id(1), id(2), id(3));
    assert_eq!(
        inputs.candidate_data[kept.0].usage,
        inputs.candidate_data[deleted.0].usage
    );
    assert_eq!(
        inputs
            .pruned_candidates
            .iter()
            .map(|pruned| (pruned.class, pruned.dominated_by))
            .collect::<Vec<_>>(),
        vec![(deleted, kept)]
    );
    assert_eq!(
        inputs.producer_candidates_per_input[top.0],
        vec![vec![kept]]
    );
    assert!(inputs.producer_candidates_per_input[deleted.0].is_empty());
    assert_eq!(inputs.contexts[0].options, vec![leaf, kept, top]);
}

// ---------------------------------------------------------------------------
// Execution contexts
// ---------------------------------------------------------------------------

/// A leaf, a materialized (shareable) middle, and two queries each reading
/// the middle through its own stateless top; a third query reads only its
/// own leaf. Candidate `i` runs operator node `i`.
fn shared_middle() -> PhysicalASAPMemoCandidates {
    let leaf = values_node(1.0);
    let middle = reader_of(&leaf);
    let top_0 = reader_of(&middle);
    let top_1 = reader_of(&middle);
    let lone = values_node(2.0);
    let candidates = vec![
        candidate(0, &[], false),
        PhysicalCandidate {
            materialization: Materialization::MatAtQuery(Medium::Memory),
            ..candidate(1, &[&[0]], false)
        },
        candidate(2, &[&[1]], true),
        candidate(3, &[&[1]], true),
        candidate(4, &[], true),
    ];
    catalog(
        vec![(leaf, 0), (middle, 1), (top_0, 2), (top_1, 3), (lone, 4)],
        candidates,
        vec![root(2), root(3), root(4)],
    )
}

// Each query context holds its own top and the shared middle as a read
// across its boundary; the middle's own context holds the leaf below it;
// `I` holds nothing; a query that cannot reach the middle holds only its
// own candidate.
#[test]
fn execution_contexts_on_a_hand_built_catalog() {
    let physical = shared_middle();
    let inputs = prepared(&physical);
    let (leaf, middle, top_0, top_1, lone) = (id(0), id(1), id(2), id(3), id(4));
    assert!(CandidateLinks::new(&physical).is_shareable(middle));
    assert!(inputs.pruned_candidates.is_empty());
    let contexts: Vec<(ContextKind, Vec<PhysicalCandidateId>)> = inputs
        .contexts
        .iter()
        .map(|context| (context.kind, context.options.clone()))
        .collect();
    assert_eq!(
        contexts,
        vec![
            (ContextKind::Query(0), vec![middle, top_0]),
            (ContextKind::Query(1), vec![middle, top_1]),
            (ContextKind::Query(2), vec![lone]),
            (ContextKind::Ingestion, Vec::new()),
            (ContextKind::Shared(middle), vec![leaf]),
        ]
    );
    assert_eq!(inputs.context_of_shareable, BTreeMap::from([(middle, 4)]));
    assert!(inputs.contexts[4].computes(RunKind::PerContext));
    assert!(!inputs.contexts[4].computes(RunKind::Shareable));
    assert_eq!(
        inputs.output_candidates,
        vec![vec![top_0], vec![top_1], vec![lone]]
    );
}

/// Stage 2's output for `queries` and what Stage 3 also reads.
struct ThroughStages1And2 {
    physical: PhysicalASAPMemoCandidates,
    entries: Vec<QueryWorkloadEntry>,
    data: DataWorkload,
}

/// `queries` (PromQL over metric `m`, each predictable and repeating every
/// minute with a 1 % error target, over metric `m` with 100 series)
/// through Stage 1 and Stage 2 as the facade runs them, over the complete pane-width domain, with raw data and
/// query-time results retained.
fn through_stages_1_and_2(queries: &[&str]) -> ThroughStages1And2 {
    through_stages_1_and_2_over(queries, PaneWidthDomain::Complete)
}

/// [`through_stages_1_and_2`] over the pane-width domain `domain`.
fn through_stages_1_and_2_over(queries: &[&str], domain: PaneWidthDomain) -> ThroughStages1And2 {
    use asap_logical_asap_optimization::generate_per_computation_logical_memo_candidates;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_logical_asap_optimization::{generate_logical_asap_memo_candidates, Pass2RoundState};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::workload::{
        DurationMs, Predictability, QueryRecurrence, RepeatedDemand, RepetitionInterval,
    };
    let target = AccuracyTarget::max_error(0.01).failure_probability(0.01);
    let roots: Vec<QueryRoot> = queries
        .iter()
        .map(|query| QueryRoot::Operator(lower_promql(query, target.clone())))
        .collect();
    let every_minute =
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(60_000)));
    let entries: Vec<_> = queries
        .iter()
        .map(|_| QueryWorkloadEntry {
            predictability: Predictability::Predictable { known_at: None },
            ..entry_with(target.clone(), every_minute.clone())
        })
        .collect();
    let mut data = DataWorkload::sampled_every(["m".to_string()], DurationMs(1_000));
    let source = data.sources.get_mut("m").expect("m");
    source.series_count = Some(100);
    source.values_non_negative = true;
    let targets = vec![target.clone(); queries.len()];
    let pass1 = generate_per_computation_logical_memo_candidates(
        asap_logical_asap_optimization::generate_dag_of_logical_memo_groups(&roots),
        &targets,
        &asap_accuracy::BuiltinAccuracyModel,
        &asap_accuracy::NoAccuracyEvidence,
        &data,
        None,
        None,
    )
    .unwrap();
    let mut round = Pass2RoundState::new(pass1);
    let logical =
        generate_logical_asap_memo_candidates(&mut round, &entries, domain, None).unwrap();
    let physical = asap_physical_asap_optimization::generate_physical_asap_memo_candidates(
        &logical, &entries, &data, true, true, None,
    )
    .unwrap();
    // Stage 2 copies each node's `accuracy_budget_parts` from its memo
    // candidates, the largest for a factored node.
    for node in &physical.operator_nodes_of_candidates_for_memo_groups {
        let largest = node
            .candidates_for_memo_group
            .iter()
            .map(|id| logical.memo.candidates[id.group.0 as usize][id.index].accuracy_budget_parts)
            .max();
        assert_eq!(Some(node.accuracy_budget_parts), largest);
    }
    ThroughStages1And2 {
        physical,
        entries,
        data,
    }
}

// Through the real Stages 1 and 2: two quantile queries over 1m and 5m
// windows of one metric can share a pane chain. Both queries reach it, it
// is an option of both query contexts with a context of its own, and every
// context computes only what runs there.
// No quotient: every approximate candidate weighs 1.
#[test]
fn two_queries_share_a_pane_chain_through_the_real_pipeline() {
    let stages = through_stages_1_and_2(&[
        "quantile_over_time(0.9, m[15m])",
        "quantile_over_time(0.9, m[1h])",
    ]);
    let physical = &stages.physical;
    let links = CandidateLinks::new(physical);
    let inputs = prepare_milp_inputs(
        physical,
        &stages.entries,
        &stages.data,
        &CostInputs::default(),
    );
    let shared_chains: Vec<PhysicalCandidateId> = (0..physical.candidates.len())
        .map(PhysicalCandidateId)
        .filter(|candidate| {
            matches!(
                physical.physical_node(*candidate).asap(),
                Some(ASAPOp::PaneChain { .. })
            ) && links.is_shareable(*candidate)
                && inputs.contexts[0].options.contains(candidate)
                && inputs.contexts[1].options.contains(candidate)
        })
        .collect();
    assert!(!shared_chains.is_empty());
    for &chain in &shared_chains {
        let context = inputs.context_of_shareable[&chain];
        assert_eq!(inputs.contexts[context].kind, ContextKind::Shared(chain));
    }
    // Every context computes only candidates of its own kind and reads the
    // others across its boundary.
    for context in &inputs.contexts {
        for &option in &context.options {
            let kind = inputs.kind[option.0];
            assert!(
                context.computes(kind)
                    || kind == RunKind::Shareable
                    || (kind == RunKind::Ingestion && context.kind != ContextKind::Ingestion)
            );
        }
    }
    for weights in &inputs.accuracy_weights {
        assert!(!weights.is_empty());
        assert!(
            weights
                .values()
                .all(|&weight| weight == AccuracyWeight::One),
            "{weights:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Accuracy weights
// ---------------------------------------------------------------------------

/// The sketch algorithm and `parts` of each approximate candidate of
/// `physical`, with its weight for query 0 (`None`: no weight).
fn sketch_weights(
    physical: &PhysicalASAPMemoCandidates,
    inputs: &MilpInputs,
) -> Vec<(SketchAlgorithm, usize, Option<AccuracyWeight>)> {
    (0..physical.candidates.len())
        .filter(|&class| inputs.candidate_data[class].approximate)
        .map(|class| {
            let id = PhysicalCandidateId(class);
            let algorithm = match physical.physical_node(id).asap() {
                Some(
                    ASAPOp::SummaryAgg {
                        family: FieldDataType::Sketch(kind, _),
                        ..
                    }
                    | ASAPOp::PaneChain {
                        family: FieldDataType::Sketch(kind, _),
                        ..
                    },
                ) => *kind.algorithm(),
                _ => unreachable!("an approximate candidate is a sketch"),
            };
            (
                algorithm,
                inputs.candidate_data[class].accuracy_budget_parts,
                inputs.accuracy_weights[0].get(&id).copied(),
            )
        })
        .collect()
}

// `q0.5 / q0.9` through the real stages, weights from the composition: a
// KLL rank error has no rule through a quotient, so Pass 1 prunes every KLL
// candidate (#1129); a DDSketch relative
// error composes, but a lone denominator sized for the whole target
// (`parts = 1`) misses it (`ε/(1 − ε) > ε`) while one sized for two parts
// meets it.
#[test]
fn ratio_weights_come_from_the_composition() {
    let stages = through_stages_1_and_2(&[
        "quantile_over_time(0.5, m[5m]) / quantile_over_time(0.9, m[5m])",
    ]);
    let inputs = prepare_milp_inputs(
        &stages.physical,
        &stages.entries,
        &stages.data,
        &CostInputs::default(),
    );
    let weights = sketch_weights(&stages.physical, &inputs);
    let of = |algorithm: SketchAlgorithm, parts: usize| -> BTreeSet<Option<AccuracyWeight>> {
        weights
            .iter()
            .filter(|(a, p, _)| *a == algorithm && *p == parts)
            .map(|(_, _, weight)| *weight)
            .collect()
    };
    let missed = Some(AccuracyWeight::Unusable(
        UnusableReason::AccuracyTargetMissed,
    ));
    // Pass 1 prunes every KLL: its rank error composes no guarantee
    // through the quotient.
    for parts in [1, 2] {
        assert_eq!(
            of(SketchAlgorithm::Kll, parts),
            BTreeSet::new(),
            "{weights:?}"
        );
    }
    // At one part: numerators meet the target, denominators miss it.
    assert_eq!(
        of(SketchAlgorithm::DDSketch, 1),
        BTreeSet::from([Some(AccuracyWeight::One), missed]),
        "{weights:?}"
    );
    assert_eq!(
        of(SketchAlgorithm::DDSketch, 2),
        BTreeSet::from([Some(AccuracyWeight::One)]),
        "{weights:?}"
    );
    // Two DDSketches at two parts compose: no pair is excluded.
    assert!(inputs.accuracy_excluded_pairs[0].is_empty());
}

// RangeHeavy T8+T5's shape: `q0.9 / q0.5` with both operands also queried
// alone. One DDSketch pane chain sized for two shares of the budget serves
// both quotient operands and both lone quantiles, as today's engine plans
// it: the check accepts the quotient over it (`(α + α)/(1 − α) ≤ ε` at
// `α = 1 − (1 + ε)^{−1/2}`), so it weighs 1 for every query. A chain sized
// for the whole budget (`α = ε`) never serves the quotient's denominator.
#[test]
fn one_pane_chain_serves_a_ratio_and_its_operands() {
    let stages = through_stages_1_and_2_over(
        &[
            "quantile_over_time(0.9, m[5m]) / quantile_over_time(0.5, m[5m])",
            "quantile_over_time(0.9, m[5m])",
            "quantile_over_time(0.5, m[5m])",
        ],
        PaneWidthDomain::SeedGrids,
    );
    let inputs = prepare_milp_inputs(
        &stages.physical,
        &stages.entries,
        &stages.data,
        &CostInputs::default(),
    );
    let alpha = |class: usize| match stages
        .physical
        .physical_node(PhysicalCandidateId(class))
        .asap()
    {
        Some(ASAPOp::PaneChain {
            family: FieldDataType::Sketch(kind, _),
            ..
        }) => match kind.params() {
            asap_types::ir::schema::SketchParams::DDSketch { alpha } => Some(*alpha),
            _ => None,
        },
        _ => None,
    };
    let weight = |query: usize, class: usize| {
        inputs.accuracy_weights[query]
            .get(&PhysicalCandidateId(class))
            .copied()
    };
    let two_shares = 1.0 - 1.01f64.powf(-0.5);
    let shared: Vec<usize> = (0..stages.physical.candidates.len())
        .filter(|&class| alpha(class).is_some())
        .filter(|&class| (0..3).all(|query| weight(query, class) == Some(AccuracyWeight::One)))
        .collect();
    assert!(!shared.is_empty());
    for &class in &shared {
        assert!(
            (alpha(class).unwrap() - two_shares).abs() < 1e-9,
            "{:?}",
            alpha(class)
        );
    }
    // A chain sized for the whole budget weighs 1 for the ratio only where
    // its values are the numerator's alone, so it never also serves both
    // lone quantiles.
    for class in (0..stages.physical.candidates.len()).filter(|&class| alpha(class) == Some(0.01)) {
        assert!(
            !(0..3).all(|query| weight(query, class) == Some(AccuracyWeight::One)),
            "{class}"
        );
    }
}

// The checks keep what they build for later checks: on RangeHeavy T8+T5's
// shape (a quotient and both operands alone) and on a quotient of three
// quantiles (where pairs are excluded), the weights and excluded pairs
// equal those of building every check from scratch.
#[test]
fn kept_builds_give_the_weights_of_fresh_builds() {
    kept_builds_match_fresh_builds(&[
        "quantile_over_time(0.9, m[5m]) / quantile_over_time(0.5, m[5m])",
        "quantile_over_time(0.9, m[5m])",
        "quantile_over_time(0.5, m[5m])",
    ]);
    let three = kept_builds_match_fresh_builds(&[
        "quantile_over_time(0.9, m[5m]) / quantile_over_time(0.5, m[5m]) / quantile_over_time(0.1, m[5m])",
    ]);
    assert!(!three.accuracy_excluded_pairs[0].is_empty());
}

/// Asserts that `queries`' accuracy weights and excluded pairs are the
/// same with builds kept across checks and with fresh builds; returns the
/// former.
fn kept_builds_match_fresh_builds(queries: &[&str]) -> MilpInputs {
    let stages = through_stages_1_and_2_over(queries, PaneWidthDomain::SeedGrids);
    let prepare = |fresh: bool| {
        accuracy_positions::FRESH_BUILDS.with(|cell| cell.set(fresh));
        let inputs = prepare_milp_inputs(
            &stages.physical,
            &stages.entries,
            &stages.data,
            &CostInputs::default(),
        );
        accuracy_positions::FRESH_BUILDS.with(|cell| cell.set(false));
        inputs
    };
    let (kept, fresh) = (prepare(false), prepare(true));
    assert!(kept
        .accuracy_weights
        .iter()
        .any(|weights| !weights.is_empty()));
    assert_eq!(kept.accuracy_weights, fresh.accuracy_weights);
    assert_eq!(kept.accuracy_excluded_pairs, fresh.accuracy_excluded_pairs);
    kept
}
