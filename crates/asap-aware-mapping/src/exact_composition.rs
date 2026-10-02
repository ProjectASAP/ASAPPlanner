//! [`ExactCompositionStrategy`] composes an exact function with a summary
//! plan across an explicit maintenance/read-time boundary (issue #171).
//!
//! `construct_summary_agg` already nests accumulator realizations, such as
//! KLL over exact `Sum` state or a quantile over `Rate` state. This strategy
//! covers the more general cases where an exact function must consume a
//! summary readout, or where a maintained summary consumes the values of an
//! exact function that has no accumulator realization.
//!
//! Both cases use an ordinary `NonASAPOp::Aggregate` node over the child
//! plan. The node carries no timing: it runs when its consumer runs, so the
//! same operator serves both placements and adding a function does not
//! require adding a new physical node type. [`OperationPlacement`] is the
//! search-time placement choice.
//!
//! ## Reference, don't select
//!
//! A composed candidate needs a child plan to compose *with* — the inner
//! quantile's own summary readout, say. This strategy deliberately does
//! **not** pick that child itself (the way `construct_summary_agg`'s
//! `realize_child` takes the head of the child's own ranking): a
//! [`Replacement::ExactComposition`] carries only the child *target*
//! (`ExactComposition::child_target`, the same `Rc<OperatorNode>` whose
//! `TargetSubDAGCandidates` in `PlanSpace` already holds every candidate for it). It is
//! [`PlanSpace::global_selection`](crate::replacement::PlanSpace::global_selection)
//! that commits the compatible parent/child pair — so the child's own
//! cost-model ranking, workload-wide effective consumer count, and shared
//! `Rc` identity (one inner summary serving two outer folds) all stay
//! correct, and a child that is also shared by an unrelated consumer is
//! maintained exactly once. `GlobalSelection::assemble_selected_dag` then links the
//! committed pair into one validated post-ASAP DAG.
//!
//! ## Proposal conditions
//!
//! A candidate is proposed only when all of these hold:
//!
//! - the target is a single-measure, `HAVING`-free exact aggregate;
//! - read-time operation: the child is a bindable aggregate that has at least one
//!   readout-producing summary implementation (a sketch/sample/wavelet/
//!   model — the shapes a maintained accumulator can't sit above), and the
//!   target's grouping keys resolve in the child's output schema;
//!   transform: the target is a per-entity exact function with no
//!   accumulator form (its only implementation is `PassThrough`);
//! - the exact operator consumes only `Plain` values in its data_state — checked
//!   again, structurally, when the pair is composed;
//! - the plugged-in [`CostModel`] has not disproven the matching
//!   [`ValueOperationCapabilities`](crate::cost_model::ValueOperationCapabilities).
//!   Unknown support keeps the candidate visible; only explicit positive
//!   support evidence permits global selection.
//!
//! `avg` gets a read-time operation candidate *and* keeps
//! [`crate::rewrite::AvgToSumOverCountStrategy`]'s rewrite in the same
//! group; the cost model picks between them, nothing here hard-codes one.
//!
//! ## What this strategy never does
//!
//! - Propose an `ExactRead` for a position beneath a maintained
//!   summary — data_state validation at composition rejects it as a typed
//!   `RealizationError` regardless.
//! - Decide whether a composition is *worth it*: that is
//!   `global_selection`'s job, using the issue's cost-units-per-second
//!   formulas (see `crate::cost_model::read_operation_plan_cost_rate` and
//!   siblings). Missing statistics keep the conservative kept subtree.

use std::rc::Rc;

use asap_types::ir::timing::{planned_data_state, validate_default};
use asap_types::ir::{NonASAPOp, Operator, OperatorNode, Predicate};
use asap_types::post_asap::execution_data_state::lift_plain;
use asap_types::post_asap::{
    AccuracyError, ExactOperationSchemaError, ExecutionDataState, ExecutionDataStateError,
    ResultGuarantee, Schema,
};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::vocabulary::{aggregate_output_schema, Reduction};
use asap_types::types::AccuracyTarget;

use crate::cost_model::CostModel;
use crate::replacement::{
    bindable_intent, describe_intent, realizations_for_intent, Realization, RealizationError,
    Replacement, ReplacementProvenance, ReplacementStrategy, ReplacementSubDAG, TargetSubDAG,
};
use crate::{AccuracyModel, DefaultAccuracyModel, PropagationStats};

#[cfg(test)]
use asap_types::post_asap::ExecutionTiming;

/// Which side of the maintenance/read boundary an [`ExactComposition`]'s
/// exact function executes on.
/// The exact function an [`ExactComposition`] applies: the parameters of
/// the `NonASAPOp::Aggregate` node the composition builds over its child.
#[derive(Debug, Clone, PartialEq)]
pub enum ExactOperation {
    Aggregate {
        reduction: Reduction,
        measures: Vec<AggIntent>,
        output_names: Vec<String>,
        having: Option<Predicate>,
    },
}

impl ExactOperation {
    /// Output schema of this operation over a child whose edge carries
    /// `input` — the same canonical derivation the pre-ASAP `Aggregate`
    /// node uses. `Err` when the child carries non-plain state the operator
    /// cannot read.
    pub fn output_schema(&self, input: &Schema) -> Result<Schema, ExactOperationSchemaError> {
        if !input.is_all_plain() {
            return Err(ExactOperationSchemaError::NonPlainInput);
        }
        let plain = lift_plain(input);
        let ExactOperation::Aggregate {
            reduction,
            measures,
            output_names,
            ..
        } = self;
        let out = aggregate_output_schema(&plain, reduction, measures, output_names)?;
        Ok(lift_plain(&out))
    }

    fn into_op(self, child: Rc<OperatorNode>) -> NonASAPOp {
        let ExactOperation::Aggregate {
            reduction,
            measures,
            output_names,
            having,
        } = self;
        NonASAPOp::Aggregate {
            reduction,
            measures,
            output_names,
            having,
            child,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationPlacement {
    /// After the child's summary readout.
    Read,
    /// On the maintenance path, feeding
    /// maintained state above.
    Maintenance,
}

impl OperationPlacement {
    /// The availability the composed operator consumes and produces.
    pub fn data_state(self) -> ExecutionDataState {
        match self {
            Self::Read => ExecutionDataState::QUERY_ROWS,
            Self::Maintenance => ExecutionDataState::INGESTION_ROWS,
        }
    }

    pub fn provenance(self) -> ReplacementProvenance {
        match self {
            Self::Read => ReplacementProvenance::ValueOperationAtQueryTime,
            Self::Maintenance => ReplacementProvenance::ValueOperationAtIngestionTime,
        }
    }
}

/// The payload of a [`Replacement::ExactComposition`] candidate: an exact
/// operator, the placement it runs at, and a *reference* to the child target
/// it composes over — never an already-selected child plan (see the module
/// docs' "Reference, don't select").
#[derive(Debug, Clone)]
pub struct ExactComposition {
    pub placement: OperationPlacement,
    pub op: ExactOperation,
    /// The pre-ASAP child the operator consumes; its `TargetSubDAGCandidates` holds the
    /// candidates `global_selection` may commit this composition with.
    pub child_target: Rc<OperatorNode>,
    /// The composed node's output schema — the target's own pre-ASAP
    /// output schema, lifted with every column `Plain` (an exact operator
    /// only ever produces plain values).
    pub schema: Schema,
}

impl ExactComposition {
    /// The data state `child` produces when this operation (its consumer)
    /// runs at the placement's timing.
    fn child_data_state(&self, child: &Rc<OperatorNode>) -> ExecutionDataState {
        planned_data_state(child, self.placement.data_state().timing)
    }

    /// Can `child` legally be this composition's input? Phase legality
    /// (the child's produced data_state — a kept pre-ASAP subtree takes the
    /// phase this edge assigns) plus the plain-operand rule, checked
    /// through the same schema derivation [`Self::compose`] uses.
    pub fn accepts_child(&self, child: &Rc<OperatorNode>) -> bool {
        self.child_data_state(child) == self.placement.data_state()
            && self.op.output_schema(&child.schema).is_ok()
    }

    /// Build the composed, data_state-validated node over `child`. Every edge of
    /// the result (including everything beneath `child`) is checked by
    /// `asap_types::ir::timing::validate_default`; an illegal
    /// placement is a typed [`RealizationError::ExecutionDataState`], never deferred to a
    /// runtime.
    pub fn compose(&self, child: Rc<OperatorNode>) -> Result<Rc<OperatorNode>, RealizationError> {
        self.compose_with_accuracy(child, &DefaultAccuracyModel)
    }

    /// Compose using the caller's accuracy algebra. Exact operators do not
    /// erase an approximate child's error: supported folds propagate it;
    /// unsupported folds fail closed with a typed accuracy error.
    pub fn compose_with_accuracy(
        &self,
        child: Rc<OperatorNode>,
        accuracy_model: &dyn AccuracyModel,
    ) -> Result<Rc<OperatorNode>, RealizationError> {
        let produced = self.child_data_state(&child);
        if produced != self.placement.data_state() {
            let edge = match self.placement {
                OperationPlacement::Maintenance => "exact operation child (maintenance time)",
                OperationPlacement::Read => "exact operation child (read time)",
            };
            return Err(RealizationError::ExecutionDataState(
                ExecutionDataStateError::IllegalChildDataState {
                    edge,
                    child: produced,
                },
            ));
        }
        let schema = self.op.output_schema(&child.schema)?;
        let guarantee = match &child.guarantee {
            None => None,
            Some(input) if input.is_exact() => Some(ResultGuarantee::exact(format!(
                "exact function {:?} over exact input",
                self.op
            ))),
            Some(input) => match accuracy_model.exact_operation_rule(&self.op) {
                Some(operator) => match accuracy_model.propagate(
                    &operator,
                    std::slice::from_ref(input),
                    None,
                    &PropagationStats::default(),
                ) {
                    Ok(guarantee) => Some(guarantee),
                    // The plan remains executable without a declared accuracy
                    // target, but an unknown guarantee cannot satisfy a later
                    // target check. Never replace this with an exact/default
                    // bound.
                    Err(AccuracyError::UnsupportedComposition { .. }) => None,
                    Err(error) => return Err(RealizationError::Accuracy(error)),
                },
                // No definition-registered rule: preserve "unknown". This is
                // the fail-closed value used by accuracy-target filtering.
                None => None,
            },
        };
        let node = Rc::new(
            OperatorNode::with_schema(Operator::NonASAP(self.op.clone().into_op(child)), schema)
                .with_guarantee(guarantee),
        );
        validate_default(&node, self.placement.data_state().timing)?;
        Ok(node)
    }

    /// Structural identity for `TargetSubDAGCandidates` dedup: same placement, same
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
fn is_query_time_reducer(intent: &AggIntent) -> bool {
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
fn needs_readout(implementation: &Realization) -> bool {
    matches!(
        implementation,
        Realization::Sketch(_)
            | Realization::Sample { .. }
            | Realization::Wavelet { .. }
            | Realization::StatModel { .. }
    )
}

/// The `(op, child)` of a read-time operation-shaped target, or `None`.
fn query_time_shape(
    root: &OperatorNode,
    cost_model: &dyn CostModel,
) -> Option<(ExactOperation, Rc<OperatorNode>, AggIntent)> {
    let Some(NonASAPOp::Aggregate {
        reduction,
        measures,
        output_names,
        having: None,
        child,
    }) = root.non_asap()
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
    if !is_query_time_reducer(intent) {
        return None;
    }
    let child_intent = bindable_intent(child)?;
    if !realizations_for_intent(child_intent, cost_model)
        .iter()
        .any(needs_readout)
    {
        return None;
    }
    // Grouping keys must resolve in the child's output schema — the same
    // derivation the composed node's own schema will use.
    Some((
        ExactOperation::Aggregate {
            reduction: reduction.clone(),
            measures: measures.clone(),
            output_names: output_names.clone(),
            having: None,
        },
        Rc::clone(child),
        intent.clone(),
    ))
}

/// The `(op, child)` of a function-shaped target — a per-entity exact
/// transform with no accumulator form — or `None`.
fn ingestion_time_shape(
    root: &OperatorNode,
    cost_model: &dyn CostModel,
) -> Option<(ExactOperation, Rc<OperatorNode>, AggIntent)> {
    let Some(NonASAPOp::Aggregate {
        reduction: Reduction::PerEntity,
        measures,
        output_names,
        having: None,
        child,
    }) = root.non_asap()
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
    // as `SummaryAgg(ExactAggregate)`; only a pass-through function needs
    // an explicit update-path node.
    if realizations_for_intent(intent, cost_model)
        .iter()
        .any(|i| *i != Realization::PassThrough)
    {
        return None;
    }
    Some((
        ExactOperation::Aggregate {
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
        let schema = lift_plain(&target.root.schema);
        let mut out = Vec::new();

        if let Some((op, child, intent)) = query_time_shape(target.root, self.cost_model) {
            if self
                .cost_model
                .value_operation_support_evidence(&op, OperationPlacement::Read)
                != Some(false)
            {
                let child_desc =
                    describe_intent(bindable_intent(&child).expect("checked by query_time_shape"));
                out.push(ReplacementSubDAG {
                    strategy: "ExactCompositionStrategy",
                    replacement: Replacement::ExactComposition(ExactComposition {
                        placement: OperationPlacement::Read,
                        op,
                        child_target: child,
                        schema: schema.clone(),
                    }),
                    provenance: ReplacementProvenance::ValueOperationAtQueryTime,
                    rationale: format!(
                        "{} is an exact fold whose input is the readout of {} — a maintained \
                         accumulator cannot consume query-time values, so instead of keeping \
                         the whole tree pre-ASAP this applies the fold as an \
                         ExactRead over whichever summary readout global_selection \
                         commits for the child target (asap_aware_mapping::exact_composition)",
                        describe_intent(&intent),
                        child_desc
                    ),
                });
            }
        }

        if let Some((op, child, intent)) = ingestion_time_shape(target.root, self.cost_model) {
            if self
                .cost_model
                .value_operation_support_evidence(&op, OperationPlacement::Maintenance)
                != Some(false)
            {
                out.push(ReplacementSubDAG {
                    strategy: "ExactCompositionStrategy",
                    replacement: Replacement::ExactComposition(ExactComposition {
                        placement: OperationPlacement::Maintenance,
                        op,
                        child_target: child,
                        schema,
                    }),
                    provenance: ReplacementProvenance::ValueOperationAtIngestionTime,
                    rationale: format!(
                        "{} is an exact per-entity function with no accumulator form; as an \
                         explicit ExactMaintenance on the update path its output can feed a \
                         maintained summary above it instead of being handed over as an opaque \
                         raw kept subtree (asap_aware_mapping::exact_composition)",
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
    use crate::cost_model::{DefaultCostModel, ValueOperationCapabilities};
    use crate::replacement::keep_pre_asap;
    use crate::test_support::{agg, agg_per_entity as per_entity, metric_scan, timed};
    use asap_types::ir::ASAPOp;
    use asap_types::post_asap::{ExecutionDataStateError, FieldDataType, SketchAlgorithm};
    use asap_types::pre_asap::agg_intent::default_quantile;

    /// `max by (zone) (quantile by (zone, host) (m))`.
    fn max_over_quantile() -> Rc<OperatorNode> {
        let inner = agg(
            vec![2, 3],
            default_quantile(0.99),
            metric_scan(&["zone", "host"]),
        );
        agg(vec![0], AggIntent::Max { col: None }, inner)
    }

    #[test]
    fn proposes_query_time_operation_for_max_over_quantile() {
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
        assert_eq!(comp.placement, OperationPlacement::Read);
        assert_eq!(
            candidates[0].provenance,
            ReplacementProvenance::ValueOperationAtQueryTime
        );
        let Some(NonASAPOp::Aggregate { child, .. }) = root.non_asap() else {
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
    fn proposes_query_time_operation_for_avg_over_quantile_alongside_the_rewrite() {
        let inner = agg(vec![2], default_quantile(0.99), metric_scan(&["zone"]));
        let root = agg(vec![0], AggIntent::Avg { col: None }, inner);
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
    fn proposes_ingestion_time_operation_for_a_per_entity_pass_through_over_raw_input() {
        let root = per_entity(AggIntent::Deriv, metric_scan(&["zone"]));
        let target = TargetSubDAG::new(&root);
        let candidates = ExactCompositionStrategy::default_cost_model().replacements(&target);
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].provenance,
            ReplacementProvenance::ValueOperationAtIngestionTime
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
        let root = agg(vec![0], AggIntent::Sum { col: None }, inner);
        assert!(!ExactCompositionStrategy::default_cost_model().matches(&TargetSubDAG::new(&root)));
        // rate is an exact accumulator — directly nestable, no separate value operation.
        let rate = per_entity(AggIntent::Rate, metric_scan(&[]));
        assert!(!ExactCompositionStrategy::default_cost_model().matches(&TargetSubDAG::new(&rate)));
        // A sketch-capable outer intent is not an exact fold.
        let inner = agg(vec![2], default_quantile(0.5), metric_scan(&["zone"]));
        let root = agg(vec![0], default_quantile(0.99), inner);
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
        fn value_operation_capabilities(&self) -> ValueOperationCapabilities {
            ValueOperationCapabilities::NONE
        }
    }

    #[test]
    fn a_runtime_without_the_capability_gets_no_candidate() {
        let root = max_over_quantile();
        let target = TargetSubDAG::new(&root);
        let strategy = ExactCompositionStrategy::new(&NoMixedExecution);
        assert!(!strategy.matches(&target));
        assert!(strategy.replacements(&target).is_empty());
        let deriv = per_entity(AggIntent::Deriv, metric_scan(&[]));
        assert!(!strategy.matches(&TargetSubDAG::new(&deriv)));
    }

    #[test]
    fn compose_rejects_a_maintained_state_child_for_a_query_time_operation() {
        let root = max_over_quantile();
        let target = TargetSubDAG::new(&root);
        let candidates = ExactCompositionStrategy::default_cost_model().replacements(&target);
        let Replacement::ExactComposition(comp) = &candidates[0].replacement else {
            unreachable!()
        };
        // A bare SummaryAgg (state, no readout) is not a legal read-time operation
        // input — the operator would be consuming sketch state.
        let state_child =
            crate::replacement::realize_child(&comp.child_target, &DefaultCostModel).unwrap();
        let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &state_child.operator
        else {
            panic!("expected the child to realize to a readout");
        };
        assert!(!comp.accepts_child(summary_input));
        assert!(matches!(
            comp.compose(Rc::clone(summary_input)),
            Err(RealizationError::ExecutionDataState(
                ExecutionDataStateError::IllegalChildDataState { .. }
            ))
        ));
        // The readout itself is accepted and composes to a plain schema.
        assert!(comp.accepts_child(&state_child));
        let composed = comp.compose(state_child).unwrap();
        assert!(
            composed.guarantee.is_none(),
            "rank error has no registered conversion through max"
        );
        assert!(matches!(
            composed.operator,
            Operator::NonASAP(NonASAPOp::Aggregate { .. })
        ));
        // Timing is no longer stored by composition: under the default
        // lifecycle assignment the composed read-time operation runs at
        // query time.
        assert_eq!(timed(&composed).timing, Some(ExecutionTiming::QueryTime));
        assert!(composed
            .schema
            .fields
            .iter()
            .all(|f| matches!(f.dtype, FieldDataType::Plain(_))));
    }

    #[test]
    fn compose_rejects_a_readout_child_for_a_ingestion_time_operation() {
        let inner = agg(vec![2], default_quantile(0.99), metric_scan(&["zone"]));
        let root = per_entity(AggIntent::Deriv, inner);
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
            Err(RealizationError::ExecutionDataState(
                ExecutionDataStateError::IllegalChildDataState { .. }
            ))
        ));
        // Raw update input is fine.
        let raw = keep_pre_asap(&comp.child_target).unwrap();
        assert!(comp.accepts_child(&raw));
        // Timing is no longer stored by composition: the composition's
        // placement is maintenance time, the composed exact operation is a
        // plain Aggregate over the raw rows, and it is legal (and planned to
        // run) at ingestion time.
        assert_eq!(comp.placement, OperationPlacement::Maintenance);
        let composed = comp.compose(raw).unwrap();
        assert!(matches!(
            composed.operator,
            Operator::NonASAP(NonASAPOp::Aggregate { .. })
        ));
        validate_default(&composed, ExecutionTiming::IngestionTime).unwrap();
        assert_eq!(
            planned_data_state(&composed, ExecutionTiming::IngestionTime).timing,
            ExecutionTiming::IngestionTime
        );
    }

    fn max_op(by: Vec<usize>) -> ExactOperation {
        ExactOperation::Aggregate {
            reduction: Reduction::by(by),
            measures: vec![AggIntent::Max { col: None }],
            output_names: vec![],
            having: None,
        }
    }

    #[test]
    fn exact_operator_schema_matches_pre_asap_aggregate_derivation() {
        let child = lift_plain(&metric_scan(&["zone"]).schema);
        let out = max_op(vec![2]).output_schema(&child).unwrap();
        let names: Vec<_> = out.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["zone", "max"]);
        assert!(out.is_all_plain());
    }

    #[test]
    fn exact_operator_rejects_non_plain_input() {
        let state = Schema::lifted(
            vec![asap_types::pre_asap::Field::new(
                "state",
                FieldDataType::ExactAggregate(
                    asap_types::post_asap::ExactKind::Sum,
                    asap_types::post_asap::ExactParams::Sum,
                ),
                false,
            )],
            None,
        );
        assert!(matches!(
            max_op(vec![]).output_schema(&state),
            Err(ExactOperationSchemaError::NonPlainInput)
        ));
    }
}
