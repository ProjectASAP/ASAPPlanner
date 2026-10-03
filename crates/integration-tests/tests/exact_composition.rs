//! Issue #171 — composing exact operators with summary plans across
//! explicit update/evaluation boundaries, end to end through
//! `search_workload_with` → `CandidateLogicalASAPDAGs::global_selection` →
//! `GlobalSelection::assemble_selected_dag` → `dag_export`.
//!
//! Covers the issue's integration matrix: both nesting directions, grouped
//! fine-to-coarse and identity folds, one inner summary shared by several
//! queries, phase-aware summary construction, a runtime without
//! the capability, a cost model without statistics, and pre/post-ASAP
//! schemas plus shared `Rc` identity — along with pins for every
//! already-supported exact-accumulator nesting.

use std::rc::Rc;

use asap_aware_mapping::cost_model::{
    CostProvenance, CostUnit, ExactCompositionCostInputs, ExactCompositionCostRequest,
    ValueOperationCapabilities,
};
use asap_aware_mapping::exact_composition::ExactOperation;
use asap_aware_mapping::replacement::{
    default_strategies_with, search_workload_with, ASAPStrategies, Replacement,
    ReplacementProvenance, ReplacementStrategy, TargetSubDAG,
};
use asap_aware_mapping::{
    CostModel, DefaultCostModel, EvaluationRate, ExplanationKind, OperationPlacement,
};
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::post_asap::{maintained, post_asap_dag, timed};
use asap_types::dag_export;
use asap_types::ir::operator_properties::{Reduction, Source};
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::ir::timing::data_state;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, TimeRangeKind};
use asap_types::post_asap::{
    ExactKind, ExecutionDataState, ExecutionTiming, FieldDataType, SketchAlgorithm, SummaryUpdate,
};
use asap_types::pre_asap::agg_intent::{default_quantile, AggIntent};
use asap_types::pre_asap::schema::{DataType, Field, Schema};

use asap_types::types::AccuracyTarget;

// ── fixtures ────────────────────────────────────────────────────────────

fn node(op: NonASAPOp) -> Rc<OperatorNode> {
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(op))
        .expect("fixture node derives its schema")
}

fn metric_scan(labels: &[&str]) -> Rc<OperatorNode> {
    let mut columns = vec![
        Field::plain("ts", DataType::Timestamp, false),
        Field::plain("value", DataType::Float64, false),
    ];
    columns.extend(
        labels
            .iter()
            .map(|n| Field::plain(*n, DataType::Utf8, true)),
    );
    node(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "latency".into(),
        },
        predicates: vec![],
        schema: Schema::with_time_index(columns, 0, vec![]),
    })
}

fn agg(by: Vec<usize>, intent: AggIntent, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    node(NonASAPOp::Aggregate {
        reduction: Reduction::by(by),
        measures: vec![intent],
        output_names: vec![],
        filters: vec![],
        having: None,
        child,
    })
}

fn per_entity(intent: AggIntent, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    node(NonASAPOp::Aggregate {
        reduction: Reduction::PerEntity,
        measures: vec![intent],
        output_names: vec![],
        filters: vec![],
        having: None,
        child,
    })
}

/// `quantile by (zone, host) (latency)` — the fine-grained inner summary.
fn fine_quantile() -> Rc<OperatorNode> {
    agg(
        vec![2, 3],
        default_quantile(0.99),
        metric_scan(&["zone", "host"]),
    )
}

/// A deployment cost model that supplies every statistic the issue's
/// formulas need, so a composition can actually win — and advertises both
/// mixed-execution shapes.
struct StatsModel;

/// Search, selection, and materialization must retain the caller's proven rule.
#[test]
fn custom_accuracy_rule_survives_root_target_and_materialization() {
    use asap_aware_mapping::{AccuracyModel, DefaultAccuracyModel, PropagationStats};
    use asap_types::post_asap::{
        AccuracyError, CompositionOperator, ResultGuarantee, SketchStatistic,
    };
    struct Model;
    impl AccuracyModel for Model {
        fn exact_operation_rule(&self, _: &ExactOperation) -> Option<CompositionOperator> {
            Some(CompositionOperator::ExactExtremum)
        }
        fn local_guarantee(
            &self,
            family: &FieldDataType,
            query: &SketchStatistic,
        ) -> Option<ResultGuarantee> {
            DefaultAccuracyModel.local_guarantee(family, query)
        }
        fn propagate(
            &self,
            _: &CompositionOperator,
            _: &[ResultGuarantee],
            _: Option<&ResultGuarantee>,
            _: &PropagationStats,
        ) -> Result<ResultGuarantee, AccuracyError> {
            // Test-only oracle: the marker detects accidental use of the default model.
            Ok(ResultGuarantee::exact("custom rule oracle"))
        }
        fn satisfies(&self, g: &ResultGuarantee, t: &AccuracyTarget) -> bool {
            DefaultAccuracyModel.satisfies(g, t)
        }
    }
    let root = agg(vec![0], AggIntent::Max { col: None }, fine_quantile());
    let space = asap_aware_mapping::replacement::search_workload_with_targets(
        vec![("q", root, Some(AccuracyTarget::Exact))],
        &default_strategies_with(&StatsModel),
        &Model,
    );
    let selection = space.global_selection(&StatsModel);
    assert!(selection
        .for_target(&space.roots[0].1)
        .unwrap()
        .composition
        .is_some());
    let node = selection
        .assemble_selected_dag(&space.roots[0].1)
        .unwrap()
        .unwrap();
    let guarantee = node.guarantee.as_ref().unwrap();
    assert!(guarantee.is_exact());
    assert!(format!("{:?}", guarantee.provenance).contains("custom rule oracle"));
}

/// An exact operator must not turn an unknown approximate-input bound into exactness.
#[test]
fn root_target_rejects_unproven_composition() {
    let root = agg(vec![0], AggIntent::Max { col: None }, fine_quantile());
    let space = asap_aware_mapping::replacement::search_workload_with_targets(
        vec![("q", root, Some(AccuracyTarget::Exact))],
        &default_strategies_with(&StatsModel),
        &asap_aware_mapping::DefaultAccuracyModel,
    );
    let selection = space.global_selection(&StatsModel);
    assert!(selection
        .for_target(&space.roots[0].1)
        .unwrap()
        .composition
        .is_none());
}

impl CostModel for StatsModel {
    fn allow_uncosted_legacy_selection(&self) -> bool {
        true
    }

    fn value_operation_support_evidence(
        &self,
        _operation: &ExactOperation,
        _placement: OperationPlacement,
    ) -> Option<bool> {
        Some(true)
    }
    fn rank_candidates(
        &self,
        _intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm> {
        candidates.to_vec()
    }
    fn exact_composition_cost_inputs(
        &self,
        _request: &ExactCompositionCostRequest<'_>,
    ) -> ExactCompositionCostInputs {
        ExactCompositionCostInputs {
            exact_cost_per_row: Some(0.1),
            expected_input_rows: Some(50.0),
            expected_output_rows: Some(10.0),
            summary_maintenance_cost_per_update: Some(0.01),
            summary_read_cost: Some(1.0),
            update_rate: Some(100.0),
            evaluation_rate: Some(EvaluationRate(1.0)),
            raw_recompute_cost: Some(100.0),
            unit: CostUnit::CostUnitsPerSecond,
            provenance: CostProvenance {
                model: "StatsModel".into(),
                version: "test-1".into(),
            },
        }
    }
}

/// Same statistics, but the runtime advertises no mixed-execution shape.
struct NoCapabilityModel;

impl CostModel for NoCapabilityModel {
    fn allow_uncosted_legacy_selection(&self) -> bool {
        true
    }

    fn rank_candidates(
        &self,
        _intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm> {
        candidates.to_vec()
    }
    fn value_operation_capabilities(&self) -> ValueOperationCapabilities {
        ValueOperationCapabilities::NONE
    }
    fn exact_composition_cost_inputs(
        &self,
        request: &ExactCompositionCostRequest<'_>,
    ) -> ExactCompositionCostInputs {
        StatsModel.exact_composition_cost_inputs(request)
    }
}

/// Complete cost evidence does not imply runtime support evidence.
struct UnknownCapabilityModel;

impl CostModel for UnknownCapabilityModel {
    fn rank_candidates(
        &self,
        _intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm> {
        candidates.to_vec()
    }
    fn exact_composition_cost_inputs(
        &self,
        request: &ExactCompositionCostRequest<'_>,
    ) -> ExactCompositionCostInputs {
        StatsModel.exact_composition_cost_inputs(request)
    }
}

#[test]
fn unknown_runtime_capability_keeps_candidate_but_prevents_selection() {
    let root = agg(vec![0], AggIntent::Max { col: None }, fine_quantile());
    let space = plan(vec![("q", root)], &UnknownCapabilityModel);
    let group = space.candidates_for_target(&space.roots[0].1).unwrap();
    assert!(group.candidates.iter().any(|candidate| {
        matches!(candidate.replacement, Replacement::ExactComposition(_))
            && candidate
                .runtime_support_evidence(&UnknownCapabilityModel)
                .is_none()
            && UnknownCapabilityModel
                .candidate_cost(
                    candidate,
                    &asap_aware_mapping::TargetSubDAG::new(&space.roots[0].1),
                )
                .is_none()
    }));
    let selection = space.global_selection(&UnknownCapabilityModel);
    assert!(selection
        .for_target(&space.roots[0].1)
        .unwrap()
        .composition
        .is_none());
    assert!(selection
        .assemble_selected_dag(&space.roots[0].1)
        .unwrap()
        .is_some());
}

fn plan(
    roots: Vec<(&'static str, Rc<OperatorNode>)>,
    cost_model: &dyn CostModel,
) -> asap_aware_mapping::CandidateLogicalASAPDAGs<&'static str> {
    search_workload_with(roots, &default_strategies_with(cost_model))
}

fn is_plain(node: &OperatorNode) -> bool {
    node.schema
        .fields
        .iter()
        .all(|f| matches!(f.dtype, FieldDataType::Plain(_)))
}

fn names(node: &OperatorNode) -> Vec<&str> {
    node.schema.fields.iter().map(|f| f.name.as_str()).collect()
}

/// The composed query-time shape: an exact `Aggregate` directly over a
/// summary evaluation, at query time.
fn is_query_time_fold(node: &OperatorNode) -> bool {
    matches!(
        node.non_asap(),
        Some(NonASAPOp::Aggregate { child, .. })
            if matches!(child.operator, Operator::ASAP(ASAPOp::SummaryEstimate { .. }))
    )
}

// ── step 1: pin every already-supported exact-accumulator nesting ───────

#[test]
fn every_exact_accumulator_is_finalized_before_an_outer_sketch() {
    use std::time::Duration;
    let cases: Vec<(Rc<OperatorNode>, ExactKind)> = vec![
        (
            agg(
                vec![2],
                AggIntent::Sum { col: None },
                metric_scan(&["zone"]),
            ),
            ExactKind::Sum,
        ),
        (
            agg(
                vec![2],
                AggIntent::Count {
                    accuracy: AccuracyTarget::Exact,
                },
                metric_scan(&["zone"]),
            ),
            ExactKind::Count,
        ),
        (
            agg(
                vec![2],
                AggIntent::Min { col: None },
                metric_scan(&["zone"]),
            ),
            ExactKind::Min,
        ),
        (
            agg(
                vec![2],
                AggIntent::Max { col: None },
                metric_scan(&["zone"]),
            ),
            ExactKind::Max,
        ),
        (
            per_entity(
                AggIntent::Rate,
                node(NonASAPOp::TimeRange {
                    range: Duration::from_secs(300),
                    kind: TimeRangeKind::Range,
                    child: metric_scan(&["zone"]),
                }),
            ),
            ExactKind::Rate,
        ),
        (
            per_entity(
                AggIntent::Increase,
                node(NonASAPOp::TimeRange {
                    range: Duration::from_secs(300),
                    kind: TimeRangeKind::Range,
                    child: metric_scan(&["zone"]),
                }),
            ),
            ExactKind::Increase,
        ),
    ];
    for (inner, kind) in cases {
        let outer = agg(vec![], default_quantile(0.9), inner);
        let target = TargetSubDAG::new(&outer);
        let candidates = ASAPStrategies::default_cost_model().replacements(&target);
        let Replacement::SubDAG(root) = &candidates[0].replacement else {
            unreachable!()
        };
        // Timing is not stored on the plan: time it with the outer summary
        // maintained (which also validates every edge) and inspect the copy.
        let root = maintained(root);
        let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &root.operator else {
            panic!("expected KLL evaluation, got {:?}", root.operator);
        };
        let Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) = &summary_input.operator else {
            panic!("expected outer SummaryAgg");
        };
        let Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: finalized }) = &child.operator
        else {
            panic!("{kind:?}: missing maintenance finalization");
        };
        assert_eq!(
            child.timing,
            Some(ExecutionTiming::IngestionTime),
            "{kind:?}: finalization runs at maintenance time"
        );
        assert!(
            matches!(
                &finalized.operator,
                Operator::ASAP(ASAPOp::SummaryAgg { family: FieldDataType::ExactAggregate(k, _), .. }) if *k == kind
            ),
            "{kind:?}: expected the exact accumulator under its finalization, got {:?}",
            finalized.operator
        );
    }
}

// ── direction 1: outer exact fold over an inner summary evaluation ────────

/// `max`/`avg` over a quantile does not collapse into one opaque kept
/// sub-DAG: the outer group holds an `ValueOperationAtQueryTime`
/// candidate referencing the inner target, the inner group keeps its own
/// sketch candidates, and with statistics the pair is committed and
/// materializes as `ValueOperationAtQueryTime → SummaryEstimate → SummaryAgg`.
#[test]
fn max_and_avg_over_quantile_compose_at_query_time_with_statistics() {
    for intent in [AggIntent::Max { col: None }, AggIntent::Avg { col: None }] {
        let root = agg(vec![0], intent.clone(), fine_quantile());
        let space = plan(vec![("q", Rc::clone(&root))], &StatsModel);
        let root = Rc::clone(&space.roots[0].1);
        let Some(NonASAPOp::Aggregate { child: inner, .. }) = root.non_asap() else {
            unreachable!()
        };

        let outer_group = space.candidates_for_target(&root).unwrap();
        assert!(
            outer_group
                .candidates
                .iter()
                .any(|c| c.provenance == ReplacementProvenance::ValueOperationAtQueryTime),
            "{intent:?}: outer group must hold an ValueOperationAtQueryTime candidate"
        );
        let inner_group = space.candidates_for_target(inner).unwrap();
        assert!(
            inner_group
                .candidates
                .iter()
                .any(|c| matches!(&c.replacement, Replacement::SubDAG(n)
                    if matches!(n.operator, Operator::ASAP(ASAPOp::SummaryEstimate { .. })))),
            "{intent:?}: the inner quantile keeps its own evaluation candidates"
        );

        let selection = space.global_selection(&StatsModel);
        let selected = selection.for_target(&root).unwrap();
        let chosen = selected.chosen.expect("a decision");
        assert_eq!(
            chosen.provenance,
            ReplacementProvenance::ValueOperationAtQueryTime
        );
        let decision = selected
            .composition
            .as_ref()
            .expect("composition provenance");
        assert!(Rc::ptr_eq(decision.child_target, inner));
        assert!(decision.cost_rate < decision.baseline_rate);
        assert_eq!(decision.inputs.unit, CostUnit::CostUnitsPerSecond);
        assert_eq!(decision.inputs.provenance.model, "StatsModel");
        // The child was committed to a compatible candidate *from its own
        // group* — the same candidate its own selection reports.
        let child_candidate = decision.child_candidate.expect("read-time operation child");
        let inner_selected = selection.for_target(inner).unwrap();
        assert!(std::ptr::eq(
            inner_selected.chosen.unwrap(),
            child_candidate
        ));

        let composed = selection.assemble_selected_dag(&root).unwrap().unwrap();
        let Some(NonASAPOp::Aggregate { child, .. }) = composed.non_asap() else {
            panic!(
                "{intent:?}: expected ValueOperationAtQueryTime root, got {:?}",
                composed.operator
            );
        };
        assert!(matches!(
            child.operator,
            Operator::ASAP(ASAPOp::SummaryEstimate { .. })
        ));
        assert!(
            child.guarantee.is_some(),
            "child has its KLL rank guarantee"
        );
        assert!(
            composed.guarantee.is_none(),
            "rank error has no definition-backed conversion through max/average"
        );
        assert!(is_plain(&composed));
        assert_eq!(
            names(&composed),
            root.schema
                .fields
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            "the composed plan's schema is the pre-ASAP target's own"
        );
        assert_eq!(
            timed(&composed).timing,
            Some(ExecutionTiming::QueryTime),
            "{intent:?}: the exact fold runs at query time"
        );
    }
}

/// `avg` keeps competing with `AvgToSumOverCountStrategy`: both candidates
/// live in the same group; nothing hard-codes the winner.
#[test]
fn avg_over_quantile_keeps_the_sum_over_count_rewrite_as_a_competitor() {
    // `by (zone)` over `by (zone)`: the averaged column resolves to the
    // non-null quantile output, which is what the rewrite requires.
    let inner = agg(vec![2], default_quantile(0.99), metric_scan(&["zone"]));
    let root = agg(vec![0], AggIntent::Avg { col: None }, inner);
    let space = plan(vec![("q", root)], &StatsModel);
    let group = space.candidates_for_target(&space.roots[0].1).unwrap();
    let provenances: Vec<_> = group.candidates.iter().map(|c| c.provenance).collect();
    assert!(provenances.contains(&ReplacementProvenance::LogicalRewrite));
    assert!(provenances.contains(&ReplacementProvenance::ValueOperationAtQueryTime));
}

/// Grouped fine-to-coarse fold (`by (zone)` over `by (zone, host)`) and the
/// identity fold (`by (zone)` over `by (zone)`) both compose; the operator
/// is the same, only the fold's row multiplicity differs.
#[test]
fn identity_and_genuine_multi_row_folds_both_compose() {
    let identity_inner = agg(vec![2], default_quantile(0.99), metric_scan(&["zone"]));
    for (label, inner) in [
        ("identity", identity_inner),
        ("fine-to-coarse", fine_quantile()),
    ] {
        let root = agg(vec![0], AggIntent::Max { col: None }, inner);
        let space = plan(vec![("q", root)], &StatsModel);
        let root = &space.roots[0].1;
        let composed = space
            .global_selection(&StatsModel)
            .assemble_selected_dag(root)
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                composed.non_asap(),
                Some(NonASAPOp::Aggregate { child, .. })
                    if matches!(child.operator, Operator::ASAP(ASAPOp::SummaryEstimate { .. }))
            ),
            "{label}: {:?}",
            composed.operator
        );
        assert_eq!(
            timed(&composed).timing,
            Some(ExecutionTiming::QueryTime),
            "{label}"
        );
        assert_eq!(names(&composed), vec!["zone", "max"], "{label}");
    }
}

/// One inner quantile consumed by two outer folds in two queries: CSE
/// collapses the inner target onto one `Rc`, both compositions commit to
/// the *same* child candidate, and both materializations share one
/// `Rc<OperatorNode>` for it — the summary is maintained once.
#[test]
fn a_shared_inner_summary_is_materialized_once_for_several_outer_folds() {
    let max = agg(vec![0], AggIntent::Max { col: None }, fine_quantile());
    let min = agg(vec![0], AggIntent::Min { col: None }, fine_quantile());
    let space = plan(vec![("max", max), ("min", min)], &StatsModel);
    let selection = space.global_selection(&StatsModel);

    let roots: Vec<Rc<OperatorNode>> = space.roots.iter().map(|(_, r)| Rc::clone(r)).collect();
    let inner_of = |r: &Rc<OperatorNode>| match r.non_asap() {
        Some(NonASAPOp::Aggregate { child, .. }) => Rc::clone(child),
        _ => unreachable!(),
    };
    assert!(
        Rc::ptr_eq(&inner_of(&roots[0]), &inner_of(&roots[1])),
        "CSE must intern the shared inner quantile"
    );
    let inner = inner_of(&roots[0]);
    assert_eq!(
        space.candidates_for_target(&inner).unwrap().consumer_count,
        2
    );

    let decisions: Vec<_> = roots
        .iter()
        .map(|r| {
            selection
                .for_target(r)
                .unwrap()
                .composition
                .as_ref()
                .expect("both roots compose")
        })
        .collect();
    assert!(std::ptr::eq(
        decisions[0].child_candidate.unwrap(),
        decisions[1].child_candidate.unwrap()
    ));
    // Shared state counted once: the second parent sees zero marginal
    // maintenance, so its rate is strictly lower than the first's.
    assert!(decisions[1].cost_rate < decisions[0].cost_rate);

    let composed: Vec<_> = roots
        .iter()
        .map(|r| selection.assemble_selected_dag(r).unwrap().unwrap())
        .collect();
    let child_of = |n: &Rc<OperatorNode>| match n.non_asap() {
        Some(NonASAPOp::Aggregate { child, .. })
            if matches!(
                child.operator,
                Operator::ASAP(ASAPOp::SummaryEstimate { .. })
            ) =>
        {
            Rc::clone(child)
        }
        _ => panic!("expected ValueOperationAtQueryTime, got {:?}", n.operator),
    };
    assert!(
        Rc::ptr_eq(&child_of(&composed[0]), &child_of(&composed[1])),
        "both folds compose over the same Rc<OperatorNode>"
    );
}

// ── direction 2: outer summary over an inner exact maintenance-time operation ─

/// `quantile(0.99, deriv(latency[5m]))`: `deriv` has no accumulator form.
/// The function target gets an `ValueOperationAtIngestionTime` candidate; with a
/// maintained summary above it and statistics, it is committed, and the
/// outer summary's materialization is re-linked over it.
#[test]
fn outer_summary_over_an_exact_function_composes_at_ingestion_time() {
    use std::time::Duration;
    let deriv = per_entity(
        AggIntent::Deriv,
        node(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: metric_scan(&["zone"]),
        }),
    );
    let root = agg(vec![], default_quantile(0.99), deriv);
    let space = plan(vec![("q", root)], &StatsModel);
    let root = Rc::clone(&space.roots[0].1);
    let Some(NonASAPOp::Aggregate { child: deriv, .. }) = root.non_asap() else {
        unreachable!()
    };
    assert!(space
        .candidates_for_target(deriv)
        .unwrap()
        .candidates
        .iter()
        .any(|c| c.provenance == ReplacementProvenance::ValueOperationAtIngestionTime));

    let selection = space.global_selection(&StatsModel);
    let deriv_sel = selection.for_target(deriv).unwrap();
    assert_eq!(
        deriv_sel.chosen.unwrap().provenance,
        ReplacementProvenance::ValueOperationAtIngestionTime
    );
    let decision = deriv_sel.composition.as_ref().unwrap();
    assert!(decision.child_candidate.is_none(), "function input is raw");
    assert!(decision.cost_rate < decision.baseline_rate);

    let composed = selection.assemble_selected_dag(&root).unwrap().unwrap();
    // Walk the timed copy, with the outer summary maintained at ingestion time.
    let composed = maintained(&composed);
    let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &composed.operator else {
        panic!("expected evaluation root, got {:?}", composed.operator);
    };
    let Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) = &summary_input.operator else {
        panic!("expected SummaryAgg");
    };
    let Some(NonASAPOp::Aggregate { child: raw, .. }) = child.non_asap() else {
        panic!(
            "expected ValueOperationAtIngestionTime under the maintained summary, got {:?}",
            child.operator
        );
    };
    // The raw input is kept as-is.
    assert!(matches!(raw.non_asap(), Some(NonASAPOp::TimeRange { .. })));
    assert!(!raw.contains_asap());
    assert_eq!(data_state(child), Some(ExecutionDataState::INGESTION_ROWS));
    assert_eq!(data_state(raw), Some(ExecutionDataState::INGESTION_ROWS));
}

// ── rejection, capability, statistics ───────────────────────────────────

/// Summary construction can consume query-time values without pretending
/// they are available to an ingestion-time consumer.
#[test]
fn summary_construction_follows_its_value_input_phase() {
    let root = agg(vec![0], AggIntent::Max { col: None }, fine_quantile());
    let space = plan(vec![("q", Rc::clone(&root))], &StatsModel);
    let post = space
        .global_selection(&StatsModel)
        .assemble_selected_dag(&space.roots[0].1)
        .unwrap()
        .unwrap();
    let illegal = std::rc::Rc::new(
        OperatorNode::with_schema(
            asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg {
                child: post,
                family: FieldDataType::ExactAggregate(
                    ExactKind::Max,
                    asap_types::post_asap::ExactParams::Max,
                ),
                input: SummaryUpdate::column(asap_types::pre_asap::ColumnRef::SampleValue),
                reduction: Reduction::by(vec![]),
                grouping: Default::default(),
                filter: None,
            }),
            Schema::lifted(vec![], None),
        )
        .with_guarantee(None),
    );
    // Even under an ingestion-time consumer the state is built at query
    // time, because a evaluation sits below it.
    let state = asap_types::ir::planned_data_state(&illegal, ExecutionTiming::IngestionTime);
    assert_eq!(state.timing, ExecutionTiming::QueryTime);
    asap_types::ir::validate_maintained(&illegal, state.timing).unwrap();
}

#[test]
fn a_runtime_without_mixed_execution_gets_no_composition_candidates() {
    let root = agg(vec![0], AggIntent::Max { col: None }, fine_quantile());
    let space = plan(vec![("q", root)], &NoCapabilityModel);
    let root = Rc::clone(&space.roots[0].1);
    let group = space.candidates_for_target(&root).unwrap();
    assert!(group
        .candidates
        .iter()
        .all(|c| !matches!(c.replacement, Replacement::ExactComposition(_))));
    let selection = space.global_selection(&NoCapabilityModel);
    assert!(selection.for_target(&root).unwrap().composition.is_none());
    let node = selection.assemble_selected_dag(&root).unwrap().unwrap();
    assert!(!is_query_time_fold(&node));
    // The inner quantile is still independently selectable.
    let Some(NonASAPOp::Aggregate { child, .. }) = root.non_asap() else {
        unreachable!()
    };
    assert!(selection.for_target(child).unwrap().chosen.is_some());
}

/// Without statistics (the built-in model) the composition is *proposed*
/// — visible in `CandidateLogicalASAPDAGs` and explanations — but never *selected*: the
/// site keeps a non-composed alternative, and the inner summary stays
/// independently selectable.
#[test]
fn missing_cost_statistics_preserve_the_conservative_retain_exact() {
    let root = agg(vec![0], AggIntent::Max { col: None }, fine_quantile());
    let space = plan(vec![("q", root)], &DefaultCostModel);
    let root = Rc::clone(&space.roots[0].1);
    assert!(space
        .candidates_for_target(&root)
        .unwrap()
        .candidates
        .iter()
        .any(|c| c.provenance == ReplacementProvenance::ValueOperationAtQueryTime));
    let selection = space.global_selection(&DefaultCostModel);
    let selected = selection.for_target(&root).unwrap();
    assert!(selected.composition.is_none());
    assert!(!matches!(
        selected.chosen.map(|c| &c.replacement),
        Some(Replacement::ExactComposition(_))
    ));
    let node = selection.assemble_selected_dag(&root).unwrap().unwrap();
    assert!(!is_query_time_fold(&node));

    let explanations = asap_aware_mapping::explain_replacements(vec![("q", Rc::clone(&root))]);
    assert!(explanations
        .iter()
        .any(|e| e.kind == ExplanationKind::ExactComposition));
}

// ── DAG export: explicit stage, schema, provenance ───────────────────────

#[test]
fn dag_export_carries_explicit_stage_and_plain_schema_for_a_composed_plan() {
    let root = agg(vec![0], AggIntent::Max { col: None }, fine_quantile());
    let space = plan(vec![("q", root)], &StatsModel);
    let root = &space.roots[0].1;
    let composed = space
        .global_selection(&StatsModel)
        .assemble_selected_dag(root)
        .unwrap()
        .unwrap();
    let dag = dag_export::export_summary(&composed);
    let node = &dag.nodes[dag.root as usize];
    assert_eq!(node.kind, "aggregate");
    assert!(node.detail["measures"].is_array());
    // Timing is explicit in the wire-6 DAG: the root is a relational
    // aggregate placed at query time.
    let wire = post_asap_dag(&composed);
    let wire_root = wire.nodes.iter().find(|n| n.id == wire.roots[0]).unwrap();
    assert!(matches!(
        wire_root.payload,
        PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Aggregate { .. })
    ));
    assert_eq!(wire_root.output_state.timing, ExecutionTiming::QueryTime);

    // Pre-ASAP export of the same target still describes the same columns.
    let pre = dag_export::export(root);
    let pre_root = &pre.nodes[pre.root as usize];
    let pre_cols: Vec<String> = pre_root.schema.as_ref().unwrap()["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(pre_cols, names(&composed));
}

/// The PromQL front end produces the exact issue shape and it composes.
#[test]
fn promql_max_by_zone_over_quantile_over_time_composes() {
    let expr = lower_promql(
        "max by (zone) (quantile_over_time(0.99, latency[5m]))",
        AccuracyTarget::Epsilon(0.01),
    )
    .unwrap();
    let space = plan(vec![("q", expr)], &StatsModel);
    let root = &space.roots[0].1;
    let selection = space.global_selection(&StatsModel);
    let selected = selection.for_target(root).unwrap();
    assert_eq!(
        selected.chosen.map(|c| c.provenance),
        Some(ReplacementProvenance::ValueOperationAtQueryTime),
        "{:?}",
        space
            .candidates_for_target(root)
            .unwrap()
            .candidates
            .iter()
            .map(|c| (c.strategy, c.provenance))
            .collect::<Vec<_>>()
    );
    let composed = selection.assemble_selected_dag(root).unwrap().unwrap();
    assert!(is_query_time_fold(&composed), "{:?}", composed.operator);
    assert_eq!(timed(&composed).timing, Some(ExecutionTiming::QueryTime));
    assert_eq!(
        selected.composition.as_ref().map(|d| d.inputs.unit),
        Some(CostUnit::CostUnitsPerSecond)
    );
    let _ = OperationPlacement::Read;
}
