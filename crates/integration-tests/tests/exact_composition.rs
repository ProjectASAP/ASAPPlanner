//! Issue #171 — composing exact operators with summary plans across
//! explicit update/readout boundaries, end to end through
//! `search_workload_with` → `PlanSpace::global_selection` →
//! `GlobalSelection::materialize` → `dag_export`.
//!
//! Covers the issue's integration matrix: both nesting directions, grouped
//! fine-to-coarse and identity folds, one inner summary shared by several
//! queries, illegal readout-under-maintenance rejection, a runtime without
//! the capability, a cost model without statistics, and pre/post-ASAP
//! schemas plus shared `Rc` identity — along with pins for every
//! already-supported exact-accumulator nesting.

use std::rc::Rc;

use asap_aware_mapping::cost_model::{
    CostProvenance, CostUnit, EvaluationRate, ExactCompositionCostInputs,
    ExactCompositionCostRequest, MixedExecutionCapabilities,
};
use asap_aware_mapping::replacement::{
    default_strategies_with, search_workload_with, ImplementError, Replacement,
    ReplacementProvenance, ReplacementStrategy, SketchAlgorithmStrategy, TargetSubDAG,
};
use asap_aware_mapping::{CompositionPhase, CostModel, DefaultCostModel, ExplanationKind};
use asap_frontend_promql::lower_promql;
use asap_types::dag_export;
use asap_types::post_asap::{
    validate_execution_phases, ExactKind, ExecutionAvailability, PhaseError, SketchAlgorithm,
    SummaryExpr, SummaryFamilyType, SummaryNode,
};
use asap_types::pre_asap::agg_intent::{default_quantile, AggIntent};
use asap_types::pre_asap::query_expr::{QueryExpr, Reduction, Source};
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::types::AccuracyTarget;

// ── fixtures ────────────────────────────────────────────────────────────

fn metric_scan(labels: &[&str]) -> QueryExpr {
    let mut columns = vec![
        Column::new("ts", DataType::Timestamp, false),
        Column::new("value", DataType::Float64, false),
    ];
    columns.extend(labels.iter().map(|n| Column::new(*n, DataType::Utf8, true)));
    QueryExpr::Scan {
        source: Source::TimeSeries {
            metric: "latency".into(),
        },
        predicates: vec![],
        schema: Schema::with_time_index(columns, 0, vec![]),
    }
}

fn agg(by: Vec<usize>, intent: AggIntent, child: Rc<QueryExpr>) -> Rc<QueryExpr> {
    Rc::new(QueryExpr::Aggregate {
        reduction: Reduction::by(by),
        measures: vec![intent],
        output_names: vec![],
        having: None,
        child,
    })
}

fn per_entity(intent: AggIntent, child: Rc<QueryExpr>) -> Rc<QueryExpr> {
    Rc::new(QueryExpr::Aggregate {
        reduction: Reduction::PerEntity,
        measures: vec![intent],
        output_names: vec![],
        having: None,
        child,
    })
}

/// `quantile by (zone, host) (latency)` — the fine-grained inner summary.
fn fine_quantile() -> Rc<QueryExpr> {
    agg(
        vec![2, 3],
        default_quantile(0.99),
        Rc::new(metric_scan(&["zone", "host"])),
    )
}

/// A deployment cost model that supplies every statistic the issue's
/// formulas need, so a composition can actually win — and advertises both
/// mixed-execution shapes.
struct StatsModel;

impl CostModel for StatsModel {
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
            evaluation_rate: EvaluationRate::from_intervals(&[std::time::Duration::from_secs(1)]),
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
    fn rank_candidates(
        &self,
        _intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm> {
        candidates.to_vec()
    }
    fn mixed_execution_capabilities(&self) -> MixedExecutionCapabilities {
        MixedExecutionCapabilities::NONE
    }
    fn exact_composition_cost_inputs(
        &self,
        request: &ExactCompositionCostRequest<'_>,
    ) -> ExactCompositionCostInputs {
        StatsModel.exact_composition_cost_inputs(request)
    }
}

fn plan(
    roots: Vec<(&'static str, Rc<QueryExpr>)>,
    cost_model: &dyn CostModel,
) -> asap_aware_mapping::PlanSpace<&'static str> {
    search_workload_with(roots, &default_strategies_with(cost_model))
}

fn is_plain(node: &SummaryNode) -> bool {
    node.schema
        .fields
        .iter()
        .all(|f| matches!(f.dtype, SummaryFamilyType::Plain(_)))
}

fn names(node: &SummaryNode) -> Vec<&str> {
    node.schema.fields.iter().map(|f| f.name.as_str()).collect()
}

// ── step 1: pin every already-supported exact-accumulator nesting ───────

#[test]
fn every_exact_accumulator_nests_directly_under_an_outer_sketch() {
    use std::time::Duration;
    let cases: Vec<(Rc<QueryExpr>, ExactKind)> = vec![
        (
            agg(
                vec![2],
                AggIntent::Sum { col: None },
                Rc::new(metric_scan(&["zone"])),
            ),
            ExactKind::Sum,
        ),
        (
            agg(
                vec![2],
                AggIntent::Count {
                    accuracy: AccuracyTarget::Exact,
                },
                Rc::new(metric_scan(&["zone"])),
            ),
            ExactKind::Count,
        ),
        (
            agg(
                vec![2],
                AggIntent::Min { col: None },
                Rc::new(metric_scan(&["zone"])),
            ),
            ExactKind::MinMax,
        ),
        (
            agg(
                vec![2],
                AggIntent::Max { col: None },
                Rc::new(metric_scan(&["zone"])),
            ),
            ExactKind::MinMax,
        ),
        (
            per_entity(
                AggIntent::Rate,
                Rc::new(QueryExpr::TimeRange {
                    range: Duration::from_secs(300),
                    child: Rc::new(metric_scan(&["zone"])),
                }),
            ),
            ExactKind::Rate,
        ),
        (
            per_entity(
                AggIntent::Increase,
                Rc::new(QueryExpr::TimeRange {
                    range: Duration::from_secs(300),
                    child: Rc::new(metric_scan(&["zone"])),
                }),
            ),
            ExactKind::Increase,
        ),
    ];
    for (inner, kind) in cases {
        let outer = agg(vec![], default_quantile(0.9), inner);
        let target = TargetSubDAG::new(&outer);
        let candidates = SketchAlgorithmStrategy::default_cost_model().replacements(&target);
        let Replacement::Summary(root) = &candidates[0].replacement else {
            unreachable!()
        };
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &root.expr else {
            panic!("expected KLL readout, got {:?}", root.expr);
        };
        let SummaryExpr::SummaryAgg { child, .. } = &summary_input.expr else {
            panic!("expected outer SummaryAgg");
        };
        assert!(
            matches!(
                &child.expr,
                SummaryExpr::SummaryAgg { family: SummaryFamilyType::ExactAggregate(k, _), .. } if *k == kind
            ),
            "{kind:?}: expected the exact accumulator directly under the outer sketch, got {:?}",
            child.expr
        );
        validate_execution_phases(root).expect("accumulator state composes under maintenance");
    }
}

// ── direction 1: outer exact fold over an inner summary readout ────────

/// Before this PR both `max`/`avg` over a quantile collapsed into one
/// opaque `KeepPreAsap`. Now: the outer group holds an `ExactPostProcess`
/// candidate referencing the inner target, the inner group keeps its own
/// sketch candidates, and with statistics the pair is committed and
/// materializes as `ExactPostProcess → SummaryEstimate → SummaryAgg`.
#[test]
fn max_and_avg_over_quantile_compose_as_post_process_with_statistics() {
    for intent in [AggIntent::Max { col: None }, AggIntent::Avg { col: None }] {
        let root = agg(vec![0], intent.clone(), fine_quantile());
        let space = plan(vec![("q", Rc::clone(&root))], &StatsModel);
        let root = Rc::clone(&space.roots[0].1);
        let QueryExpr::Aggregate { child: inner, .. } = root.as_ref() else {
            unreachable!()
        };

        let outer_group = space.group_for(&root).unwrap();
        assert!(
            outer_group
                .candidates
                .iter()
                .any(|c| c.provenance == ReplacementProvenance::ExactPostProcess),
            "{intent:?}: outer group must hold an ExactPostProcess candidate"
        );
        let inner_group = space.group_for(inner).unwrap();
        assert!(
            inner_group
                .candidates
                .iter()
                .any(|c| matches!(&c.replacement, Replacement::Summary(n)
                    if matches!(n.expr, SummaryExpr::SummaryEstimate { .. }))),
            "{intent:?}: the inner quantile keeps its own readout candidates"
        );

        let selection = space.global_selection(&StatsModel);
        let selected = selection.for_target(&root).unwrap();
        let chosen = selected.chosen.expect("a decision");
        assert_eq!(chosen.provenance, ReplacementProvenance::ExactPostProcess);
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
        let child_candidate = decision.child_candidate.expect("post-process child");
        let inner_selected = selection.for_target(inner).unwrap();
        assert!(std::ptr::eq(
            inner_selected.chosen.unwrap(),
            child_candidate
        ));

        let composed = selection.materialize(&root).unwrap().unwrap();
        let SummaryExpr::ExactPostProcess { child, .. } = &composed.expr else {
            panic!(
                "{intent:?}: expected ExactPostProcess root, got {:?}",
                composed.expr
            );
        };
        assert!(matches!(child.expr, SummaryExpr::SummaryEstimate { .. }));
        let child_guarantee = child.guarantee.as_ref().expect("child guarantee");
        let composed_guarantee = composed
            .guarantee
            .as_ref()
            .expect("exact post-process must propagate the child's guarantee");
        assert_eq!(composed_guarantee.metric, child_guarantee.metric);
        assert_eq!(
            composed_guarantee.bound.evaluate(),
            child_guarantee.bound.evaluate(),
            "an exact max/average fold retains the modeled error magnitude"
        );
        assert!(is_plain(&composed));
        assert_eq!(
            names(&composed),
            root.output_schema()
                .unwrap()
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            "the composed plan's schema is the pre-ASAP target's own"
        );
        validate_execution_phases(&composed).unwrap();
    }
}

/// `avg` keeps competing with `AvgToSumOverCountStrategy`: both candidates
/// live in the same group; nothing hard-codes the winner.
#[test]
fn avg_over_quantile_keeps_the_sum_over_count_rewrite_as_a_competitor() {
    // `by (zone)` over `by (zone)`: the averaged column resolves to the
    // non-null quantile output, which is what the rewrite requires.
    let inner = agg(
        vec![2],
        default_quantile(0.99),
        Rc::new(metric_scan(&["zone"])),
    );
    let root = agg(vec![0], AggIntent::Avg { col: None }, inner);
    let space = plan(vec![("q", root)], &StatsModel);
    let group = space.group_for(&space.roots[0].1).unwrap();
    let provenances: Vec<_> = group.candidates.iter().map(|c| c.provenance).collect();
    assert!(provenances.contains(&ReplacementProvenance::LogicalRewrite));
    assert!(provenances.contains(&ReplacementProvenance::ExactPostProcess));
}

/// Grouped fine-to-coarse fold (`by (zone)` over `by (zone, host)`) and the
/// identity fold (`by (zone)` over `by (zone)`) both compose; the operator
/// is the same, only the fold's row multiplicity differs.
#[test]
fn identity_and_genuine_multi_row_folds_both_compose() {
    let identity_inner = agg(
        vec![2],
        default_quantile(0.99),
        Rc::new(metric_scan(&["zone"])),
    );
    for (label, inner) in [
        ("identity", identity_inner),
        ("fine-to-coarse", fine_quantile()),
    ] {
        let root = agg(vec![0], AggIntent::Max { col: None }, inner);
        let space = plan(vec![("q", root)], &StatsModel);
        let root = &space.roots[0].1;
        let composed = space
            .global_selection(&StatsModel)
            .materialize(root)
            .unwrap()
            .unwrap();
        assert!(
            matches!(composed.expr, SummaryExpr::ExactPostProcess { .. }),
            "{label}: {:?}",
            composed.expr
        );
        assert_eq!(names(&composed), vec!["zone", "max"], "{label}");
    }
}

/// One inner quantile consumed by two outer folds in two queries: CSE
/// collapses the inner target onto one `Rc`, both compositions commit to
/// the *same* child candidate, and both materializations share one
/// `Rc<SummaryNode>` for it — the summary is maintained once.
#[test]
fn a_shared_inner_summary_is_materialized_once_for_several_outer_folds() {
    let max = agg(vec![0], AggIntent::Max { col: None }, fine_quantile());
    let min = agg(vec![0], AggIntent::Min { col: None }, fine_quantile());
    let space = plan(vec![("max", max), ("min", min)], &StatsModel);
    let selection = space.global_selection(&StatsModel);

    let roots: Vec<Rc<QueryExpr>> = space.roots.iter().map(|(_, r)| Rc::clone(r)).collect();
    let inner_of = |r: &Rc<QueryExpr>| match r.as_ref() {
        QueryExpr::Aggregate { child, .. } => Rc::clone(child),
        _ => unreachable!(),
    };
    assert!(
        Rc::ptr_eq(&inner_of(&roots[0]), &inner_of(&roots[1])),
        "CSE must intern the shared inner quantile"
    );
    let inner = inner_of(&roots[0]);
    assert_eq!(space.group_for(&inner).unwrap().consumer_count, 2);

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
        .map(|r| selection.materialize(r).unwrap().unwrap())
        .collect();
    let child_of = |n: &Rc<SummaryNode>| match &n.expr {
        SummaryExpr::ExactPostProcess { child, .. } => Rc::clone(child),
        other => panic!("expected ExactPostProcess, got {other:?}"),
    };
    assert!(
        Rc::ptr_eq(&child_of(&composed[0]), &child_of(&composed[1])),
        "both folds compose over the same Rc<SummaryNode>"
    );
}

// ── direction 2: outer summary over an inner exact update-path transform ─

/// `quantile(0.99, deriv(latency[5m]))`: `deriv` has no accumulator form.
/// The transform target gets an `ExactTransform` candidate; with a
/// maintained summary above it and statistics, it is committed, and the
/// outer summary's materialization is re-linked over it.
#[test]
fn outer_summary_over_an_exact_transform_composes_on_the_update_path() {
    use std::time::Duration;
    let deriv = per_entity(
        AggIntent::Deriv,
        Rc::new(QueryExpr::TimeRange {
            range: Duration::from_secs(300),
            child: Rc::new(metric_scan(&["zone"])),
        }),
    );
    let root = agg(vec![], default_quantile(0.99), deriv);
    let space = plan(vec![("q", root)], &StatsModel);
    let root = Rc::clone(&space.roots[0].1);
    let QueryExpr::Aggregate { child: deriv, .. } = root.as_ref() else {
        unreachable!()
    };
    assert!(space
        .group_for(deriv)
        .unwrap()
        .candidates
        .iter()
        .any(|c| c.provenance == ReplacementProvenance::ExactTransform));

    let selection = space.global_selection(&StatsModel);
    let deriv_sel = selection.for_target(deriv).unwrap();
    assert_eq!(
        deriv_sel.chosen.unwrap().provenance,
        ReplacementProvenance::ExactTransform
    );
    let decision = deriv_sel.composition.as_ref().unwrap();
    assert!(decision.child_candidate.is_none(), "transform input is raw");
    assert!(decision.cost_rate < decision.baseline_rate);

    let composed = selection.materialize(&root).unwrap().unwrap();
    let SummaryExpr::SummaryEstimate { summary_input, .. } = &composed.expr else {
        panic!("expected readout root, got {:?}", composed.expr);
    };
    let SummaryExpr::SummaryAgg { child, .. } = &summary_input.expr else {
        panic!("expected SummaryAgg");
    };
    let SummaryExpr::ExactTransform { child: raw, .. } = &child.expr else {
        panic!(
            "expected ExactTransform under the maintained summary, got {:?}",
            child.expr
        );
    };
    assert!(matches!(raw.expr, SummaryExpr::KeepPreAsap(_)));
    let assignment = validate_execution_phases(&composed).unwrap();
    assert_eq!(
        assignment.stage_of(child),
        Some(ExecutionAvailability::UpdateValue)
    );
    assert_eq!(
        assignment.stage_of(raw),
        Some(ExecutionAvailability::UpdateValue)
    );
}

// ── rejection, capability, statistics ───────────────────────────────────

/// A maintained summary above a query-time readout is a typed plan-time
/// error, both for the construction path and for a hand-built plan.
#[test]
fn readout_under_maintenance_is_rejected_at_construction() {
    let root = agg(vec![0], AggIntent::Max { col: None }, fine_quantile());
    let candidates =
        SketchAlgorithmStrategy::default_cost_model().replacements(&TargetSubDAG::new(&root));
    // The MinMax accumulator over the quantile readout is not constructible;
    // the strategy reports the conservative fallback once instead.
    assert_eq!(candidates.len(), 1);
    let Replacement::Summary(node) = &candidates[0].replacement else {
        unreachable!()
    };
    assert!(matches!(node.expr, SummaryExpr::KeepPreAsap(_)));

    // ExactPostProcess can never be placed under a SummaryAgg: compose a
    // post-process, then try to maintain a summary over it.
    let space = plan(vec![("q", Rc::clone(&root))], &StatsModel);
    let post = space
        .global_selection(&StatsModel)
        .materialize(&space.roots[0].1)
        .unwrap()
        .unwrap();
    let illegal = Rc::new(SummaryNode {
        expr: SummaryExpr::SummaryAgg {
            child: post,
            family: SummaryFamilyType::ExactAggregate(
                ExactKind::MinMax,
                asap_types::post_asap::ExactParams::MinMax,
            ),
            col: asap_types::pre_asap::ColumnRef::SampleValue,
            reduction: Reduction::by(vec![]),
            grouping: Default::default(),
        },
        schema: asap_types::post_asap::SummarySchema {
            fields: vec![],
            time_index: None,
        },
        guarantee: None,
    });
    assert!(matches!(
        validate_execution_phases(&illegal),
        Err(PhaseError::ReadoutUnderMaintenance { .. })
    ));
    let err: ImplementError = validate_execution_phases(&illegal).unwrap_err().into();
    assert!(matches!(err, ImplementError::Phase(_)));
}

#[test]
fn a_runtime_without_mixed_execution_gets_no_composition_candidates() {
    let root = agg(vec![0], AggIntent::Max { col: None }, fine_quantile());
    let space = plan(vec![("q", root)], &NoCapabilityModel);
    let root = Rc::clone(&space.roots[0].1);
    let group = space.group_for(&root).unwrap();
    assert!(group
        .candidates
        .iter()
        .all(|c| !matches!(c.replacement, Replacement::ExactComposition(_))));
    let selection = space.global_selection(&NoCapabilityModel);
    assert!(selection.for_target(&root).unwrap().composition.is_none());
    let node = selection.materialize(&root).unwrap().unwrap();
    assert!(matches!(node.expr, SummaryExpr::KeepPreAsap(_)));
    // The inner quantile is still independently selectable.
    let QueryExpr::Aggregate { child, .. } = root.as_ref() else {
        unreachable!()
    };
    assert!(selection.for_target(child).unwrap().chosen.is_some());
}

/// Without statistics (the built-in model) the composition is *proposed*
/// — visible in `PlanSpace` and explanations — but never *selected*: the
/// site keeps the conservative `KeepPreAsap`, and the inner summary stays
/// independently selectable.
#[test]
fn missing_cost_statistics_preserve_the_conservative_keep_pre_asap() {
    let root = agg(vec![0], AggIntent::Max { col: None }, fine_quantile());
    let space = plan(vec![("q", root)], &DefaultCostModel);
    let root = Rc::clone(&space.roots[0].1);
    assert!(space
        .group_for(&root)
        .unwrap()
        .candidates
        .iter()
        .any(|c| c.provenance == ReplacementProvenance::ExactPostProcess));
    let selection = space.global_selection(&DefaultCostModel);
    let selected = selection.for_target(&root).unwrap();
    assert!(selected.composition.is_none());
    assert!(!matches!(
        selected.chosen.map(|c| &c.replacement),
        Some(Replacement::ExactComposition(_))
    ));
    let node = selection.materialize(&root).unwrap().unwrap();
    assert!(matches!(node.expr, SummaryExpr::KeepPreAsap(_)));

    let explanations = asap_aware_mapping::explain_replacements(vec![("q", (*root).clone())]);
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
        .materialize(root)
        .unwrap()
        .unwrap();
    let graph = dag_export::export_summary(&composed);
    let node = &graph.nodes[graph.root as usize];
    assert_eq!(node.kind, "ExactPostProcess");
    assert_eq!(node.detail["stage"], "readout_value");
    assert_eq!(node.detail["op"], "Aggregate");
    let stages: Vec<(&str, String)> = graph
        .nodes
        .iter()
        .map(|n| (n.kind, n.detail["stage"].as_str().unwrap().to_string()))
        .collect();
    assert!(stages.contains(&("SummaryEstimate", "readout_value".into())));
    assert!(stages.contains(&("SummaryAgg", "summary_state".into())));
    assert!(stages.contains(&("KeepPreAsap", "update_value".into())));

    // Pre-ASAP export of the same target still describes the same columns.
    let pre = dag_export::export(root);
    let pre_root = &pre.nodes[pre.root as usize];
    let pre_cols: Vec<String> = pre_root.schema.as_ref().unwrap()["columns"]
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
    let space = plan(vec![("q", Rc::new(expr))], &StatsModel);
    let root = &space.roots[0].1;
    let selection = space.global_selection(&StatsModel);
    let selected = selection.for_target(root).unwrap();
    assert_eq!(
        selected.chosen.map(|c| c.provenance),
        Some(ReplacementProvenance::ExactPostProcess),
        "{:?}",
        space
            .group_for(root)
            .unwrap()
            .candidates
            .iter()
            .map(|c| (c.strategy, c.provenance))
            .collect::<Vec<_>>()
    );
    let composed = selection.materialize(root).unwrap().unwrap();
    assert!(matches!(
        composed.expr,
        SummaryExpr::ExactPostProcess { .. }
    ));
    assert_eq!(
        selected.composition.as_ref().map(|d| d.inputs.unit),
        Some(CostUnit::CostUnitsPerSecond)
    );
    let _ = CompositionPhase::PostProcess;
}
