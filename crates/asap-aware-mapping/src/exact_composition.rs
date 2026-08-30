//! [`ExactCompositionStrategy`] — composing an exact operator with a
//! summary plan across an explicit update/readout boundary (issue #171).
//!
//! ## The gap this closes
//!
//! `construct_summary_agg` already nests: a `SummaryAgg` recursively
//! realizes its child, so a KLL over an exact `Sum` accumulator, or a
//! `quantile(rate(...))` over a `Rate` accumulator, come out as one bound
//! DAG. What it cannot represent is an exact operator that is **not**
//! maintained summary state sitting next to a summary:
//!
//! - `max by (zone) (quantile_over_time(0.99, latency[5m]))` — the outer
//!   `max` is an exact fold over the inner summary's *readout*. A `MinMax`
//!   accumulator over that readout is data_state-illegal (a maintained summary
//!   can't consume query-time values — see
//!   `asap_types::post_asap::execution_data_state`),
//!   and `avg` has no accumulator at all, so today either shape collapses
//!   into one opaque `KeepPreAsap` that swallows the realizable inner
//!   quantile.
//! - `quantile(0.99, deriv(m[5m]))` — the inner `deriv` is an exact,
//!   per-sample transform with no accumulator form; the outer summary can
//!   only consume it as an opaque raw `KeepPreAsap` blob today, with no
//!   explicit "this row transform runs on the update path" node.
//!
//! [`SummaryExpr::ReadoutPostProcess`] and [`SummaryExpr::UpdateTransform`]
//! are the two data_state-explicit representations; this strategy is what
//! proposes them.
//!
//! ## Reference, don't select
//!
//! A composed candidate needs a child plan to compose *with* — the inner
//! quantile's own summary readout, say. This strategy deliberately does
//! **not** pick that child itself (the way `construct_summary_agg`'s
//! `realize_child` takes the head of the child's own ranking): a
//! [`Replacement::ExactComposition`] carries only the child *target*
//! (`ExactComposition::child_target`, the same `Rc<QueryExpr>` whose
//! `MemoGroup` in `PlanSpace` already holds every candidate for it). It is
//! [`PlanSpace::global_selection`](crate::replacement::PlanSpace::global_selection)
//! that commits the compatible parent/child pair — so the child's own
//! cost-model ranking, workload-wide effective consumer count, and shared
//! `Rc` identity (one inner summary serving two outer folds) all stay
//! correct, and a child that is also shared by an unrelated consumer is
//! maintained exactly once. `GlobalSelection::materialize` then links the
//! committed pair into one validated post-ASAP DAG.
//!
//! ## Proposal conditions
//!
//! A candidate is proposed only when all of these hold:
//!
//! - the target is a single-measure, `HAVING`-free exact aggregate;
//! - post-process: the child is a bindable aggregate that has at least one
//!   readout-producing summary implementation (a sketch/sample/wavelet/
//!   model — the shapes a maintained accumulator can't sit above), and the
//!   target's grouping keys resolve in the child's output schema;
//!   transform: the target is a per-entity exact transform with no
//!   accumulator form (its only implementation is `PassThrough`);
//! - the exact operator consumes only `Plain` values in its data_state — checked
//!   again, structurally, when the pair is composed;
//! - the plugged-in [`CostModel`] advertises the matching
//!   [`MixedExecutionCapabilities`](crate::cost_model::MixedExecutionCapabilities).
//!
//! `avg` gets a post-process candidate *and* keeps
//! [`crate::rewrite::AvgToSumOverCountStrategy`]'s rewrite in the same
//! group; the cost model picks between them, nothing here hard-codes one.
//!
//! ## What this strategy never does
//!
//! - Propose an `ExactPostProcess` for a position beneath a maintained
//!   summary — data_state validation at composition rejects it as a typed
//!   `ImplementError` regardless.
//! - Decide whether a composition is *worth it*: that is
//!   `global_selection`'s job, using the issue's cost-units-per-second
//!   formulas (see `crate::cost_model::postprocess_plan_cost_rate` and
//!   siblings). Missing statistics keep the conservative `KeepPreAsap`.

use std::rc::Rc;

use asap_types::post_asap::execution_data_state::validate_execution_data_states_at;
use asap_types::post_asap::{
    exact_operator_output_schema, produced_data_state, CompositionOperator, ExactOperator,
    ExecutionDataState, ExecutionDataStateError, SummaryExpr, SummaryNode, SummarySchema,
    ValueOperator,
};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::query_expr::{QueryExpr, Reduction};
use asap_types::types::AccuracyTarget;

use crate::cost_model::CostModel;
use crate::replacement::{
    bindable_intent, describe_intent, implementations_for_with, ImplementError, Implementation,
    Replacement, ReplacementProvenance, ReplacementStrategy, ReplacementSubDAG, TargetSubDAG,
};
use crate::{AccuracyModel, DefaultAccuracyModel, PropagationStats};

/// Which side of the update/readout boundary an [`ExactComposition`]'s
/// exact operator executes on — selects the `SummaryExpr` variant
/// [`ExactComposition::compose`] builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompositionPlacement {
    /// [`SummaryExpr::ReadoutPostProcess`]: after the child's readout.
    PostProcess,
    /// [`SummaryExpr::UpdateTransform`]: on the update path, feeding
    /// maintained state above.
    Transform,
}

impl CompositionPlacement {
    /// The availability the composed operator consumes and produces.
    pub fn data_state(self) -> ExecutionDataState {
        match self {
            Self::PostProcess => ExecutionDataState::READ_ROWS,
            Self::Transform => ExecutionDataState::MAINTENANCE_ROWS,
        }
    }

    pub fn provenance(self) -> ReplacementProvenance {
        match self {
            Self::PostProcess => ReplacementProvenance::ExactPostProcess,
            Self::Transform => ReplacementProvenance::ExactTransform,
        }
    }
}

/// The payload of a [`Replacement::ExactComposition`] candidate: an exact
/// operator, the placement it runs at, and a *reference* to the child target
/// it composes over — never an already-selected child plan (see the module
/// docs' "Reference, don't select").
#[derive(Debug, Clone)]
pub struct ExactComposition {
    pub placement: CompositionPlacement,
    pub op: ExactOperator,
    /// The pre-ASAP child the operator consumes; its `MemoGroup` holds the
    /// candidates `global_selection` may commit this composition with.
    pub child_target: Rc<QueryExpr>,
    /// The composed node's output schema — the target's own pre-ASAP
    /// output schema, lifted with every column `Plain` (an exact operator
    /// only ever produces plain values).
    pub schema: SummarySchema,
}

impl ExactComposition {
    /// Can `child` legally be this composition's input? Phase legality
    /// (the child's produced data_state — a `KeepPreAsap` leaf takes the
    /// phase this edge assigns) plus the plain-operand rule, checked
    /// through the same schema derivation [`Self::compose`] uses.
    pub fn accepts_child(&self, child: &SummaryNode) -> bool {
        let phase_ok = match produced_data_state(&child.expr) {
            None => true,
            Some(avail) => avail == self.placement.data_state(),
        };
        phase_ok && exact_operator_output_schema(&self.op, &child.schema).is_ok()
    }

    /// Build the composed, data_state-validated node over `child`. Every edge of
    /// the result (including everything beneath `child`) is checked by
    /// `asap_types::post_asap::validate_execution_data_states`; an illegal
    /// placement is a typed [`ImplementError::ExecutionDataState`], never deferred to a
    /// runtime.
    pub fn compose(&self, child: Rc<SummaryNode>) -> Result<Rc<SummaryNode>, ImplementError> {
        self.compose_with_accuracy(child, &DefaultAccuracyModel)
    }

    /// Compose using the caller's accuracy algebra. Exact operators do not
    /// erase an approximate child's error: supported folds propagate it;
    /// unsupported folds fail closed with a typed accuracy error.
    pub fn compose_with_accuracy(
        &self,
        child: Rc<SummaryNode>,
        accuracy_model: &dyn AccuracyModel,
    ) -> Result<Rc<SummaryNode>, ImplementError> {
        if let Some(produced) = produced_data_state(&child.expr) {
            if produced != self.placement.data_state() {
                let edge = match self.placement {
                    CompositionPlacement::Transform => "UpdateTransform.child",
                    CompositionPlacement::PostProcess => "ReadoutPostProcess.child",
                };
                return Err(ImplementError::ExecutionDataState(
                    ExecutionDataStateError::IllegalChildPhase {
                        edge,
                        child: produced,
                    },
                ));
            }
        }
        let schema = exact_operator_output_schema(&self.op, &child.schema)?;
        let guarantee = match &child.guarantee {
            None => None,
            Some(input) => {
                let operator = match &self.op {
                    ExactOperator::Aggregate { measures, .. } => match measures.as_slice() {
                        [AggIntent::Sum { .. }] => CompositionOperator::ExactSum,
                        [AggIntent::Min { .. } | AggIntent::Max { .. } | AggIntent::Avg { .. }] => {
                            CompositionOperator::ExactExtremum
                        }
                        // This placeholder is only used by the model's exact-input
                        // fast path. Approximate inputs correctly fail closed.
                        [_] => CompositionOperator::Lipschitz { constant: 1.0 },
                        _ => CompositionOperator::Lipschitz { constant: 1.0 },
                    },
                    _ => CompositionOperator::Lipschitz { constant: 1.0 },
                };
                Some(accuracy_model.propagate(
                    &operator,
                    std::slice::from_ref(input),
                    None,
                    &PropagationStats::default(),
                )?)
            }
        };
        let expr = match self.placement {
            CompositionPlacement::PostProcess => SummaryExpr::ReadoutPostProcess {
                child,
                op: ValueOperator::Exact(self.op.clone()),
            },
            CompositionPlacement::Transform => SummaryExpr::UpdateTransform {
                child,
                op: ValueOperator::Exact(self.op.clone()),
            },
        };
        let node = Rc::new(SummaryNode {
            expr,
            schema,
            guarantee,
        });
        validate_execution_data_states_at(&node, self.placement.data_state())?;
        Ok(node)
    }

    /// Structural identity for `MemoGroup` dedup: same placement, same
    /// operator, same child `Rc`.
    pub(crate) fn same_as(&self, other: &Self) -> bool {
        self.placement == other.placement
            && self.op == other.op
            && Rc::ptr_eq(&self.child_target, &other.child_target)
    }
}

/// Which exact reducers may run as a query-time fold over readout rows.
/// `Count` only at `Exact` accuracy (an approximate count is a sketch
/// target, not an exact fold).
fn is_post_process_reducer(intent: &AggIntent) -> bool {
    matches!(
        intent,
        AggIntent::Sum { .. }
            | AggIntent::Min { .. }
            | AggIntent::Max { .. }
            | AggIntent::Avg { .. }
            | AggIntent::StdDev { .. }
            | AggIntent::Variance { .. }
            | AggIntent::Count {
                accuracy: AccuracyTarget::Exact
            }
    )
}

/// Does `implementation` need a `SummaryEstimate` readout to yield a value
/// — i.e. is it a shape a maintained accumulator can't legally sit above?
fn needs_readout(implementation: &Implementation) -> bool {
    matches!(
        implementation,
        Implementation::Sketch(_)
            | Implementation::Sample { .. }
            | Implementation::Wavelet { .. }
            | Implementation::StatModel { .. }
    )
}

/// The `(op, child)` of a post-process-shaped target, or `None`.
fn post_process_shape(
    root: &QueryExpr,
    cost_model: &dyn CostModel,
) -> Option<(ExactOperator, Rc<QueryExpr>, AggIntent)> {
    let QueryExpr::Aggregate {
        reduction,
        measures,
        output_names,
        having: None,
        child,
    } = root
    else {
        return None;
    };
    let Reduction::Reduce(by) = reduction else {
        return None;
    };
    if by.is_without() {
        return None;
    }
    let [intent] = measures.as_slice() else {
        return None;
    };
    if !is_post_process_reducer(intent) {
        return None;
    }
    let child_intent = bindable_intent(child)?;
    if !implementations_for_with(child_intent, cost_model)
        .iter()
        .any(needs_readout)
    {
        return None;
    }
    // Grouping keys must resolve in the child's output schema — the same
    // derivation the composed node's own schema will use.
    root.output_schema().ok()?;
    Some((
        ExactOperator::Aggregate {
            reduction: reduction.clone(),
            measures: measures.clone(),
            output_names: output_names.clone(),
            having: None,
        },
        Rc::clone(child),
        intent.clone(),
    ))
}

/// The `(op, child)` of a transform-shaped target — a per-entity exact
/// transform with no accumulator form — or `None`.
fn transform_shape(
    root: &QueryExpr,
    cost_model: &dyn CostModel,
) -> Option<(ExactOperator, Rc<QueryExpr>, AggIntent)> {
    let QueryExpr::Aggregate {
        reduction: Reduction::PerEntity,
        measures,
        output_names,
        having: None,
        child,
    } = root
    else {
        return None;
    };
    let [intent] = measures.as_slice() else {
        return None;
    };
    if !intent.is_per_series() {
        return None;
    }
    // Exact accumulators (`Rate`/`Increase`) are already directly nestable
    // as `SummaryAgg(ExactAggregate)`; only a pass-through transform needs
    // an explicit update-path node.
    if implementations_for_with(intent, cost_model)
        .iter()
        .any(|i| *i != Implementation::PassThrough)
    {
        return None;
    }
    root.output_schema().ok()?;
    Some((
        ExactOperator::Aggregate {
            reduction: Reduction::PerEntity,
            measures: measures.clone(),
            output_names: output_names.clone(),
            having: None,
        },
        Rc::clone(child),
        intent.clone(),
    ))
}

/// Proposes [`Replacement::ExactComposition`] candidates — see the module
/// docs. Holds a [`CostModel`] only to ask it which mixed-execution shapes
/// the runtime advertises and which implementations the child has; it
/// never uses it to *rank* anything.
pub struct ExactCompositionStrategy<'a> {
    cost_model: &'a dyn CostModel,
}

static DEFAULT_COST_MODEL: crate::cost_model::DefaultCostModel =
    crate::cost_model::DefaultCostModel;

impl ExactCompositionStrategy<'static> {
    /// A strategy consulting the built-in [`DefaultCostModel`](crate::cost_model::DefaultCostModel).
    pub fn default_cost_model() -> Self {
        Self {
            cost_model: &DEFAULT_COST_MODEL,
        }
    }
}

impl<'a> ExactCompositionStrategy<'a> {
    pub fn new(cost_model: &'a dyn CostModel) -> Self {
        Self { cost_model }
    }

    fn candidates(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        let capabilities = self.cost_model.mixed_execution_capabilities();
        let Ok(schema) = target.root.output_schema() else {
            return Vec::new();
        };
        let schema = asap_types::post_asap::execution_data_state::lift_plain(&schema);
        let mut out = Vec::new();

        if capabilities.supports(CompositionPlacement::PostProcess) {
            if let Some((op, child, intent)) = post_process_shape(target.root, self.cost_model) {
                let child_desc = describe_intent(
                    bindable_intent(&child).expect("checked by post_process_shape"),
                );
                out.push(ReplacementSubDAG {
                    strategy: "ExactCompositionStrategy",
                    replacement: Replacement::ExactComposition(ExactComposition {
                        placement: CompositionPlacement::PostProcess,
                        op,
                        child_target: child,
                        schema: schema.clone(),
                    }),
                    provenance: ReplacementProvenance::ExactPostProcess,
                    rationale: format!(
                        "{} is an exact fold whose input is the readout of {} — a maintained \
                         accumulator cannot consume query-time values, so instead of collapsing \
                         the whole tree into KeepPreAsap this applies the fold as an \
                         ExactPostProcess over whichever summary readout global_selection \
                         commits for the child target (asap_aware_mapping::exact_composition)",
                        describe_intent(&intent),
                        child_desc
                    ),
                });
            }
        }

        if capabilities.supports(CompositionPlacement::Transform) {
            if let Some((op, child, intent)) = transform_shape(target.root, self.cost_model) {
                out.push(ReplacementSubDAG {
                    strategy: "ExactCompositionStrategy",
                    replacement: Replacement::ExactComposition(ExactComposition {
                        placement: CompositionPlacement::Transform,
                        op,
                        child_target: child,
                        schema,
                    }),
                    provenance: ReplacementProvenance::ExactTransform,
                    rationale: format!(
                        "{} is an exact per-entity transform with no accumulator form; as an \
                         explicit ExactTransform on the update path its output can feed a \
                         maintained summary above it instead of being handed over as an opaque \
                         raw KeepPreAsap blob (asap_aware_mapping::exact_composition)",
                        describe_intent(&intent)
                    ),
                });
            }
        }
        out
    }
}

impl ReplacementStrategy for ExactCompositionStrategy<'_> {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        !self.candidates(target).is_empty()
    }

    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        self.candidates(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost_model::{DefaultCostModel, MixedExecutionCapabilities};
    use crate::replacement::keep_pre_asap;
    use asap_types::post_asap::{ExecutionDataStateError, SketchAlgorithm, SummaryFamilyType};
    use asap_types::pre_asap::agg_intent::default_quantile;
    use asap_types::pre_asap::query_expr::Source;
    use asap_types::pre_asap::schema::{Column, DataType, Schema};

    fn metric_scan(labels: &[&str]) -> QueryExpr {
        let mut columns = vec![
            Column::new("ts", DataType::Timestamp, false),
            Column::new("value", DataType::Float64, false),
        ];
        columns.extend(labels.iter().map(|n| Column::new(*n, DataType::Utf8, true)));
        QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(columns, 0, vec![]),
        }
    }

    fn agg(by: Vec<usize>, intent: AggIntent, child: QueryExpr) -> QueryExpr {
        QueryExpr::Aggregate {
            reduction: Reduction::by(by),
            measures: vec![intent],
            output_names: vec![],
            having: None,
            child: Rc::new(child),
        }
    }

    fn per_entity(intent: AggIntent, child: QueryExpr) -> QueryExpr {
        QueryExpr::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![intent],
            output_names: vec![],
            having: None,
            child: Rc::new(child),
        }
    }

    /// `max by (zone) (quantile by (zone, host) (m))`.
    fn max_over_quantile() -> Rc<QueryExpr> {
        let inner = agg(
            vec![2, 3],
            default_quantile(0.99),
            metric_scan(&["zone", "host"]),
        );
        Rc::new(agg(vec![0], AggIntent::Max { col: None }, inner))
    }

    #[test]
    fn proposes_post_process_for_max_over_quantile() {
        let root = max_over_quantile();
        let target = TargetSubDAG::new(&root);
        let strategy = ExactCompositionStrategy::default_cost_model();
        assert!(strategy.matches(&target));
        let candidates = strategy.replacements(&target);
        assert_eq!(candidates.len(), 1);
        let Replacement::ExactComposition(comp) = &candidates[0].replacement else {
            panic!(
                "expected a composition, got {:?}",
                candidates[0].replacement
            );
        };
        assert_eq!(comp.placement, CompositionPlacement::PostProcess);
        assert_eq!(
            candidates[0].provenance,
            ReplacementProvenance::ExactPostProcess
        );
        let QueryExpr::Aggregate { child, .. } = root.as_ref() else {
            unreachable!()
        };
        assert!(
            Rc::ptr_eq(&comp.child_target, child),
            "the candidate references the child target's own Rc — nothing selected"
        );
        let names: Vec<_> = comp.schema.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["zone", "max"]);
    }

    #[test]
    fn proposes_post_process_for_avg_over_quantile_alongside_the_rewrite() {
        let inner = agg(vec![2], default_quantile(0.99), metric_scan(&["zone"]));
        let root = Rc::new(agg(vec![0], AggIntent::Avg { col: None }, inner));
        let target = TargetSubDAG::new(&root);
        assert_eq!(
            ExactCompositionStrategy::default_cost_model()
                .replacements(&target)
                .len(),
            1
        );
        // `avg` competes with AvgToSumOverCountStrategy in the same group.
        assert!(crate::rewrite::AvgToSumOverCountStrategy.matches(&target));
    }

    #[test]
    fn proposes_transform_for_a_per_entity_pass_through_over_raw_input() {
        let root = Rc::new(per_entity(AggIntent::Deriv, metric_scan(&["zone"])));
        let target = TargetSubDAG::new(&root);
        let candidates = ExactCompositionStrategy::default_cost_model().replacements(&target);
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].provenance,
            ReplacementProvenance::ExactTransform
        );
    }

    #[test]
    fn does_not_propose_for_shapes_already_covered_by_accumulators() {
        // sum by (zone) over an exact Sum child: the child has no readout,
        // so SummaryAgg(Sum) over SummaryAgg(Sum) is already legal.
        let inner = agg(
            vec![2, 3],
            AggIntent::Sum { col: None },
            metric_scan(&["zone", "host"]),
        );
        let root = Rc::new(agg(vec![0], AggIntent::Sum { col: None }, inner));
        assert!(!ExactCompositionStrategy::default_cost_model().matches(&TargetSubDAG::new(&root)));
        // rate is an exact accumulator — directly nestable, no transform.
        let rate = Rc::new(per_entity(AggIntent::Rate, metric_scan(&[])));
        assert!(!ExactCompositionStrategy::default_cost_model().matches(&TargetSubDAG::new(&rate)));
        // A sketch-capable outer intent is not an exact fold.
        let inner = agg(vec![2], default_quantile(0.5), metric_scan(&["zone"]));
        let root = Rc::new(agg(vec![0], default_quantile(0.99), inner));
        assert!(!ExactCompositionStrategy::default_cost_model().matches(&TargetSubDAG::new(&root)));
    }

    struct NoMixedExecution;
    impl CostModel for NoMixedExecution {
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
    }

    #[test]
    fn a_runtime_without_the_capability_gets_no_candidate() {
        let root = max_over_quantile();
        let target = TargetSubDAG::new(&root);
        let strategy = ExactCompositionStrategy::new(&NoMixedExecution);
        assert!(!strategy.matches(&target));
        assert!(strategy.replacements(&target).is_empty());
        let deriv = Rc::new(per_entity(AggIntent::Deriv, metric_scan(&[])));
        assert!(!strategy.matches(&TargetSubDAG::new(&deriv)));
    }

    #[test]
    fn compose_rejects_a_maintained_state_child_for_a_post_process() {
        let root = max_over_quantile();
        let target = TargetSubDAG::new(&root);
        let candidates = ExactCompositionStrategy::default_cost_model().replacements(&target);
        let Replacement::ExactComposition(comp) = &candidates[0].replacement else {
            unreachable!()
        };
        // A bare SummaryAgg (state, no readout) is not a legal post-process
        // input — the operator would be consuming sketch state.
        let state_child =
            crate::replacement::realize_child(&comp.child_target, &DefaultCostModel).unwrap();
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &state_child.expr else {
            panic!("expected the child to realize to a readout");
        };
        assert!(!comp.accepts_child(summary_input));
        assert!(matches!(
            comp.compose(Rc::clone(summary_input)),
            Err(ImplementError::ExecutionDataState(
                ExecutionDataStateError::IllegalChildPhase { .. }
            ))
        ));
        // The readout itself is accepted and composes to a plain schema.
        assert!(comp.accepts_child(&state_child));
        let composed = comp.compose(state_child).unwrap();
        assert!(matches!(
            composed.expr,
            SummaryExpr::ReadoutPostProcess { .. }
        ));
        assert!(composed
            .schema
            .fields
            .iter()
            .all(|f| matches!(f.dtype, SummaryFamilyType::Plain(_))));
    }

    #[test]
    fn compose_rejects_a_readout_child_for_a_transform() {
        let inner = agg(vec![2], default_quantile(0.99), metric_scan(&["zone"]));
        let root = Rc::new(per_entity(AggIntent::Deriv, inner));
        let candidates =
            ExactCompositionStrategy::default_cost_model().replacements(&TargetSubDAG::new(&root));
        let Replacement::ExactComposition(comp) = &candidates[0].replacement else {
            unreachable!()
        };
        let readout =
            crate::replacement::realize_child(&comp.child_target, &DefaultCostModel).unwrap();
        assert!(!comp.accepts_child(&readout));
        assert!(matches!(
            comp.compose(readout),
            Err(ImplementError::ExecutionDataState(
                ExecutionDataStateError::IllegalChildPhase { .. }
            ))
        ));
        // Raw update input is fine.
        let raw = keep_pre_asap(&comp.child_target).unwrap();
        assert!(comp.accepts_child(&raw));
        assert!(matches!(
            comp.compose(raw).unwrap().expr,
            SummaryExpr::UpdateTransform { .. }
        ));
    }
}
