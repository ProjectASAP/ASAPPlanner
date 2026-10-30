use std::rc::Rc;

use super::*;
use asap_types::ir::properties::{AccuracyMetric, BoundExpr, ProbabilityExpr};
use asap_types::ir::scalar::expr_ir::ScalarValue;
use asap_types::ir::schema::{DataType, Field};
use asap_types::ir::{BinaryOperator, ExprSemantics};
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

/// `kind` over `lhs` and `rhs`, with `lhs`'s schema (no schema
/// derivation: only the operator is read).
fn binary(
    kind: ArithmeticOpKind,
    lhs: &Rc<OperatorNode>,
    rhs: &Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    Rc::new(OperatorNode::with_schema(
        Operator::NonASAP(NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                checked_relative_division: false,
                checked_finite_division: false,
                kind: BinaryOpKind::Arithmetic(kind),
                vector_match: None,
            },
            return_bool: false,
            lhs: Rc::clone(lhs),
            rhs: Rc::clone(rhs),
        }),
        lhs.schema.clone(),
    ))
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

fn usage() -> ClassUsage {
    ClassUsage {
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

fn data_with(usage: ClassUsage) -> ClassData {
    ClassData {
        approximate: true,
        accuracy_budget_parts: 1,
        usage: Ok(usage),
        usage_depends_on: Vec::new(),
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
        producer_classes_per_input: &producers,
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
                producer_classes_per_input: &other_producers,
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
        producer_classes_per_input: &producers,
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
    let pruned = prune_dominated_classes(&profiles, &data);
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
        inputs.class_data[kept.0].usage,
        inputs.class_data[deleted.0].usage
    );
    assert_eq!(
        inputs
            .pruned_classes
            .iter()
            .map(|pruned| (pruned.class, pruned.dominated_by))
            .collect::<Vec<_>>(),
        vec![(deleted, kept)]
    );
    assert_eq!(inputs.producer_classes_per_input[top.0], vec![vec![kept]]);
    assert!(inputs.producer_classes_per_input[deleted.0].is_empty());
    assert_eq!(inputs.query_owners[0].classes, vec![leaf, kept, top]);
}

// ---------------------------------------------------------------------------
// Owners
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

// Query owners hold their stateless tops and read the shared middle at
// their boundary; the middle's instance owners hold the leaf below it; a
// query that cannot reach the middle has only its own candidate. Two
// queries reach the middle, so it has `m_s = 2` instance owners.
#[test]
fn owners_and_regions_on_a_hand_built_catalog() {
    let physical = shared_middle();
    let inputs = prepared(&physical);
    let (leaf, middle, top_0, top_1, lone) = (id(0), id(1), id(2), id(3), id(4));
    assert!(CandidateLinks::new(&physical).is_shareable(middle));
    assert!(inputs.pruned_classes.is_empty());
    assert_eq!(
        inputs.query_owners,
        vec![
            OwnerRegion {
                classes: vec![top_0],
                links: Vec::new(),
                boundary_reads: vec![(top_0, 0, middle)],
                output_reads: Vec::new(),
            },
            OwnerRegion {
                classes: vec![top_1],
                links: Vec::new(),
                boundary_reads: vec![(top_1, 0, middle)],
                output_reads: Vec::new(),
            },
            OwnerRegion {
                classes: vec![lone],
                ..OwnerRegion::default()
            },
        ]
    );
    assert_eq!(
        inputs.instance_owners,
        vec![InstanceOwners {
            class: middle,
            instance_count: 2,
            region: OwnerRegion {
                classes: vec![leaf],
                links: vec![(middle, 0, leaf)],
                ..OwnerRegion::default()
            },
        }]
    );
    assert_eq!(inputs.queries_reaching[middle.0], vec![0, 1]);
    assert_eq!(inputs.queries_reaching[leaf.0], vec![0, 1]);
    assert_eq!(
        inputs.output_classes,
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
/// minute with a 1 % error target) through Stage 1 and Stage 2 as the
/// facade runs them, over the complete pane-width domain, with raw data and
/// query-time results retained.
fn through_stages_1_and_2(queries: &[&str]) -> ThroughStages1And2 {
    use asap_logical_asap_optimization::pass1::logical_memo_candidates::generate_per_computation_logical_memo_candidates;
    use asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::pass2::logical_memo_candidates::{
        generate_logical_asap_memo_candidates, Pass2RoundState,
    };
    use asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain;
    use asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::workload::{
        DurationMs, Predictability, QueryRecurrence, RepeatedDemand, RepetitionInterval,
    };
    let target = AccuracyTarget::max_error(0.01).failure_probability(0.01);
    let roots: Vec<_> = queries
        .iter()
        .enumerate()
        .map(|(index, query)| {
            (
                index,
                QueryRoot::Operator(lower_promql(query, target.clone())),
            )
        })
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
    let data = DataWorkload::sampled_every(["m".to_string()], DurationMs(1_000));
    let per_computation =
        generate_per_computation_candidates(&DagOfLogicalMemoGroups::from_roots(&roots)).unwrap();
    let pass1 = generate_per_computation_logical_memo_candidates(
        DagOfLogicalMemoGroups::from_roots(&roots),
        per_computation,
        &entries,
        &data,
        None,
    )
    .unwrap();
    let mut round = Pass2RoundState::new(pass1);
    let logical = generate_logical_asap_memo_candidates(
        &mut round,
        &entries,
        PaneWidthDomain::Complete,
        None,
    )
    .unwrap();
    let physical =
        asap_physical_asap_optimization::catalog::generate_physical_asap_memo_candidates(
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
// has an instance owner with 2 instances (one per reaching query), some
// owner reads it at its boundary, and every region holds only stateless
// candidates, links inside it and boundary reads of shareable candidates.
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
            ) && inputs.queries_reaching[candidate.0] == vec![0, 1]
        })
        .collect();
    assert!(!shared_chains.is_empty());
    let all_regions: Vec<&OwnerRegion> = inputs
        .query_owners
        .iter()
        .chain(inputs.instance_owners.iter().map(|owners| &owners.region))
        .collect();
    for &chain in &shared_chains {
        let owners = inputs
            .instance_owners
            .iter()
            .find(|owners| owners.class == chain)
            .expect("an instance owner per shared chain");
        assert_eq!(owners.instance_count, 2);
        assert!(all_regions.iter().any(|region| {
            region
                .boundary_reads
                .iter()
                .any(|&(_, _, read)| read == chain)
                || region.output_reads.iter().any(|&(_, read)| read == chain)
        }));
    }
    for region in &all_regions {
        assert!(region
            .classes
            .iter()
            .all(|&candidate| !links.is_shareable(candidate)));
        assert!(region
            .links
            .iter()
            .all(|(_, _, producer)| region.classes.contains(producer)));
        assert!(region
            .boundary_reads
            .iter()
            .all(|&(_, _, read)| links.is_shareable(read)));
    }
    for weights in &inputs.accuracy_weights {
        assert!(!weights.is_empty());
        assert!(weights
            .values()
            .all(|&weight| weight == AccuracyWeight::One));
    }
}

// ---------------------------------------------------------------------------
// Accuracy weights
// ---------------------------------------------------------------------------

/// `w(q, k)` of the one query of `physical` whose approximate candidates
/// are `approximate_candidates`, by candidate.
fn weights_of(
    physical: &PhysicalASAPMemoCandidates,
    approximate_candidates: &[usize],
) -> BTreeMap<usize, AccuracyWeight> {
    let class_data: Vec<ClassData> = (0..physical.candidates.len())
        .map(|candidate| ClassData {
            approximate: approximate_candidates.contains(&candidate),
            accuracy_budget_parts: 1,
            usage: Err("not costed".into()),
            usage_depends_on: Vec::new(),
        })
        .collect();
    let alive = vec![true; physical.candidates.len()];
    let links = producer_classes_left(&CandidateLinks::new(physical), &alive);
    let outputs = output_classes(physical, &alive);
    let weights = accuracy_weights(physical, &class_data, &links, &outputs);
    weights[0]
        .iter()
        .map(|(candidate, weight)| (candidate.0, *weight))
        .collect()
}

/// `a ∘ b` over two leaves, the query's root: candidates 0 and 1 are the
/// leaves, 2 the operator.
fn two_leaves(kind: ArithmeticOpKind) -> PhysicalASAPMemoCandidates {
    let (a, b) = (values_node(1.0), values_node(2.0));
    let top = binary(kind, &a, &b);
    catalog(
        vec![(a, 0), (b, 1), (top, 2)],
        vec![
            candidate(0, &[], false),
            candidate(1, &[], false),
            candidate(2, &[&[0], &[1]], true),
        ],
        vec![root(2)],
    )
}

// A ratio's numerator weighs 1 and its denominator 2; a sum's operands 1
// each.
#[test]
fn ratio_and_sum_weights() {
    use AccuracyWeight::*;
    let ratio = weights_of(&two_leaves(ArithmeticOpKind::Div), &[0, 1]);
    assert_eq!(ratio, BTreeMap::from([(0, One), (1, Two)]));
    let sum = weights_of(&two_leaves(ArithmeticOpKind::Add), &[0, 1]);
    assert_eq!(sum, BTreeMap::from([(0, One), (1, One)]));
}

// An exact divisor scales the numerator: it keeps weight 1.
#[test]
fn an_exact_divisor_keeps_the_numerator_ordinary() {
    let ratio = weights_of(&two_leaves(ArithmeticOpKind::Div), &[0]);
    assert_eq!(ratio, BTreeMap::from([(0, AccuracyWeight::One)]));
}

// `(a / b) / c`: the inner quotient's operands are unusable (a nested
// quotient), the outer denominator weighs 2.
#[test]
fn a_nested_quotient_is_unusable() {
    let (a, b, c) = (values_node(1.0), values_node(2.0), values_node(3.0));
    let inner = binary(ArithmeticOpKind::Div, &a, &b);
    let outer = binary(ArithmeticOpKind::Div, &inner, &c);
    let physical = catalog(
        vec![(a, 0), (b, 1), (c, 2), (inner, 3), (outer, 4)],
        vec![
            candidate(0, &[], false),
            candidate(1, &[], false),
            candidate(2, &[], false),
            candidate(3, &[&[0], &[1]], false),
            candidate(4, &[&[3], &[2]], true),
        ],
        vec![root(4)],
    );
    let nested = AccuracyWeight::Unusable(UnusableReason::NestedQuotient);
    assert_eq!(
        weights_of(&physical, &[0, 1, 2]),
        BTreeMap::from([(0, nested), (1, nested), (2, AccuracyWeight::Two)])
    );
}

// A scalar root `a / b` over two operator references: numerator 1,
// denominator 2.
#[test]
fn a_scalar_ratio_weighs_its_denominator_two() {
    let (a, b) = (values_node(1.0), values_node(2.0));
    let reference = |group| Box::new(ScalarExpr::PromqlScalarFromVector(MemoGroupId(group)));
    let physical = catalog(
        vec![(a, 0), (b, 1)],
        vec![candidate(0, &[], true), candidate(1, &[], true)],
        vec![QueryRoot::Scalar(ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Div,
            left: reference(0),
            right: reference(1),
            semantics: ExprSemantics::Promql,
        })],
    );
    assert_eq!(
        weights_of(&physical, &[0, 1]),
        BTreeMap::from([(0, AccuracyWeight::One), (1, AccuracyWeight::Two)])
    );
}
