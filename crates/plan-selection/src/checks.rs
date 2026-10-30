//! Validity checks on one candidate: accuracy targets and latency. What
//! the executor or deployment cannot run (per-entity aggregates without a
//! kernel, query-time scans of raw data not retained) Stage 2 prunes.

use asap_cost::{family_name, PlanCost, QueryLatency};
use asap_types::ir::properties::{
    AccuracyGuarantee, AccuracyMetric, AccuracyScope, AccuracyTarget,
};
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, WeightDomain};
use asap_types::ir::{ASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::physical::PhysicalWorkloadPlan;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::accuracy_binding::{bound_root_guarantee, root_operators};
use crate::report::{AcceptedAccuracy, RejectionCause};
use crate::SelectionModels;
use asap_types::workload::bounds::{confirmed_weights, series_read, values_per_result};

/// The first query whose latency per evaluation is over its
/// `max_latency_ms` bound or, with no bound broken, over its repeat
/// interval (a query evaluated every `I` ms whose evaluation takes longer
/// than `I` falls further behind with every evaluation), as a rejection
/// cause and reason.
pub(crate) fn latency_violation(
    cost: &PlanCost,
    entries: &[QueryWorkloadEntry],
) -> Option<(RejectionCause, String)> {
    let per_query = || cost.usage.latency_per_query.iter().zip(entries).enumerate();
    let latency_text = |query: usize, latency: &QueryLatency| {
        format!(
            "q{}: latency {:.1} ms per evaluation (compaction {:.1} ms + query {:.1} ms)",
            query + 1,
            latency.total_ms(),
            latency.compaction_ms,
            latency.query_ms
        )
    };
    let over_bound =
        per_query().find_map(
            |(query, (latency, entry))| match entry.requirements.max_latency_ms {
                Some(bound) if latency.total_ms() > bound => Some((
                    RejectionCause::LatencyBoundExceeded,
                    format!(
                        "{} is over its {bound} ms bound",
                        latency_text(query, latency)
                    ),
                )),
                _ => None,
            },
        );
    over_bound.or_else(|| {
        per_query().find_map(|(query, (latency, entry))| {
            match entry.recurrence.fixed_interval_ms() {
                Some(interval_ms) if latency.total_ms() > interval_ms as f64 => Some((
                    RejectionCause::LatencyOverRepeatInterval,
                    format!(
                        "{} is over its {interval_ms} ms repeat interval, so its evaluations \
                         cannot keep up",
                        latency_text(query, latency)
                    ),
                )),
                _ => None,
            }
        })
    })
}

/// Each entry's accuracy target, as the per-query targets Stage 3 checks.
pub(crate) fn accuracy_targets(entries: &[QueryWorkloadEntry]) -> Vec<AccuracyTarget> {
    entries
        .iter()
        .map(|entry| entry.requirements.accuracy.clone())
        .collect()
}

/// What Stage 3 accepted for each query ([`AcceptedAccuracy`]), or the
/// first query whose output is not shown to meet its target, as a reason. An output meets its target when its theoretical root guarantee
/// (composed in Stage 1 with the built-in model) satisfies it, or when the
/// deployment's empirical evidence shows that this candidate's output does
/// ([`AccuracyEvidenceProvider::output_guarantee`]). A
/// certified estimate inside the plan is not enough on its own: a difference
/// or ratio of certified estimates can be unbounded. The root guarantee's
/// symbolic statistics are bound first ([`bound_root_guarantee`]); one still
/// unknown leaves the target unmet. Count-Min is admitted only over update
/// weights proven non-negative, which its bound and the executor assume: an open
/// [`WeightDomain::RawDataSamplesOfMetric`] requirement is met when `data` declares
/// the metric a counter or non-negative ([`confirmed_weights`]), and unmet
/// otherwise.
///
/// A target that sets no metric accepts a guarantee in any metric that meets
/// its ε, δ and scope ([`AccuracyModel::satisfies`]); one that sets a metric
/// accepts only guarantees in it (or convertible to it).
///
/// The Count-Min weight check runs first and rejects as
/// [`RejectionCause::CountMinWeightsNotProvenNonNegative`]: the executor
/// cannot run such a plan. Then the deployment's accuracy model is asked
/// ([`AccuracyModel::accepts_output_regardless_of_guarantee`], through
/// `SelectionModels::accuracy`): an output it accepts is accepted
/// whatever its guarantee says, and the guarantee check's rejection, if any,
/// is only recorded ([`AcceptedAccuracy::guarantee_check_rejection`]).
/// Otherwise, an output with no guarantee from either
/// source (Stage 1 had no rule, and the evidence has none) is rejected as
/// [`RejectionCause::NoAccuracyGuarantee`], and any other miss as
/// [`RejectionCause::AccuracyTargetMissed`].
pub(crate) fn accuracy_check(
    candidate: &PhysicalWorkloadPlan,
    targets: &[AccuracyTarget],
    data: &DataWorkload,
    models: &SelectionModels<'_>,
) -> Result<Vec<AcceptedAccuracy>, CheckFailure> {
    let mut accepted = Vec::with_capacity(targets.len());
    for (query, (root, target)) in candidate.roots.iter().zip(targets).enumerate() {
        let query = query + 1;
        let mut sketches: Vec<String> = Vec::new();
        for node in root_operators(root)
            .iter()
            .flat_map(|node| OperatorNode::reachable(node))
        {
            let Operator::ASAP(
                ASAPOp::SummaryAgg { family, input, .. } | ASAPOp::PaneChain { family, input, .. },
            ) = &node.operator
            else {
                continue;
            };
            if let FieldDataType::Sketch(kind, _) = family {
                let name = format!("{:?}", kind.algorithm());
                if !sketches.contains(&name) {
                    sketches.push(name);
                }
            }
            if matches!(family, FieldDataType::Sketch(kind, _)
                    if matches!(kind.algorithm(), SketchAlgorithm::Cms | SketchAlgorithm::CmsWithHeap))
                && !matches!(
                    confirmed_weights(&input.weight_domain, data),
                    WeightDomain::NonNegative { .. }
                )
            {
                let unconfirmed = match &input.weight_domain {
                    WeightDomain::RawDataSamplesOfMetric { metric } => format!(
                        " (samples of `{metric}`, which is not declared a counter or \
                         non-negative)"
                    ),
                    WeightDomain::CounterDerivativeOfMetric { metric } => format!(
                        " (rates or increases of `{metric}`, which is not declared a counter)"
                    ),
                    _ => String::new(),
                };
                // Count-Min (plain, CmsWithHeap or Hydra Count-Min) over
                // weights not proven non-negative does not run: the executor
                // refuses CmsWithHeap and HydraCms when it builds the plan,
                // and plain Count-Min adds a negative weight, which voids its
                // never-underestimate bound. So
                // this rejects the plan whatever the deployment's accuracy
                // model accepts.
                return Err(CheckFailure {
                    cause: RejectionCause::CountMinWeightsNotProvenNonNegative,
                    reason: format!(
                        "q{query}: {} needs non-negative update weights, and these are not \
                         proven non-negative{unconfirmed}",
                        family_name(family)
                    ),
                    size_independent: true,
                });
            }
        }
        // The guarantee check: the theoretical guarantee, then the
        // deployment's evidence.
        let checked = guarantee_check(query, root, target, data, models);
        // The deployment's model may accept the output directly, whatever
        // its guarantee says; the check's result is then only recorded.
        if models
            .accuracy
            .accepts_output_regardless_of_guarantee(root, target)
        {
            accepted.push(AcceptedAccuracy {
                target_metric: target.accuracy_metric,
                accepted_metric: None,
                accepted_regardless_of_guarantee: true,
                guarantee_check_rejection: checked.err().map(|failure| failure.cause),
                sketches,
            });
            continue;
        }
        accepted.push(AcceptedAccuracy {
            target_metric: target.accuracy_metric,
            accepted_metric: checked?,
            accepted_regardless_of_guarantee: false,
            guarantee_check_rejection: None,
            sketches,
        });
    }
    Ok(accepted)
}

/// A check a candidate failed: why, with its numbers, and whether every
/// size of the same plan's summaries fails it too.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CheckFailure {
    pub(crate) cause: RejectionCause,
    pub(crate) reason: String,
    /// `true` when no other size of the plan's summaries can pass the check
    /// (e.g. no rule composes a guarantee, or a bound stays unknown) and the
    /// deployment's accuracy model and evidence cannot judge another size
    /// otherwise; `false` when that is not proven.
    pub(crate) size_independent: bool,
}

/// Whether query `query`'s output `root` meets `target` by its theoretical
/// root guarantee or, failing that, the deployment's evidence
/// ([`accuracy_check`]): the metric of the guarantee that met it (`None`
/// for an exact output), or why not, as
/// [`RejectionCause::NoAccuracyGuarantee`] or
/// [`RejectionCause::AccuracyTargetMissed`].
pub(crate) fn guarantee_check(
    query: usize,
    root: &QueryRoot,
    target: &AccuracyTarget,
    data: &DataWorkload,
    models: &SelectionModels<'_>,
) -> Result<Option<AccuracyMetric>, CheckFailure> {
    // A guarantee per value is stated over the values the target's scope
    // covers: the whole result's `G`, or at most `⌊1 / share⌋` groups.
    let series = series_read(root, data);
    let values = values_per_result(root, series, data);
    let over_scope = |g: AccuracyGuarantee| g.over_scope(target.accuracy_scope, values);
    let guarantee = bound_root_guarantee(root, series, data, models).map(over_scope);
    let meets = |g: &AccuracyGuarantee| models.accuracy.satisfies(g, target);
    // The deployment's evidence is read only when the theoretical
    // guarantee does not meet the target.
    let evidence = || models.evidence.output_guarantee(root).map(over_scope);
    let (met, evidence) = match guarantee.clone().filter(meets) {
        Some(met) => (Some(met), None),
        None => {
            let evidence = evidence();
            (evidence.clone().filter(meets), evidence)
        }
    };
    if let Some(met) = met {
        return Ok((!met.is_exact()).then_some(met.spec.accuracy_metric));
    }
    // Whether the deployment's model or evidence may judge another size of
    // the plan's summaries otherwise.
    let model_or_evidence_may_judge = models.accuracy.may_accept_regardless_of_guarantee()
        || models.evidence.may_provide_evidence();
    if guarantee.is_none() && evidence.is_none() {
        return Err(CheckFailure {
            cause: RejectionCause::NoAccuracyGuarantee,
            reason: format!(
                "q{query}: the output has no guarantee, and the deployment has no evidence \
                 for it, so it cannot meet target {target:?}"
            ),
            // No rule composes one, whatever the sizes.
            size_independent: !model_or_evidence_may_judge,
        });
    }
    // A bound or failure probability that stays unknown once bound is
    // unknown at every size; a known one may shrink with larger summaries.
    let unknown = |g: &AccuracyGuarantee| {
        g.spec.error.evaluate().is_none()
            || (target.failure_probability.is_some()
                && g.spec.failure_probability.evaluate().is_none())
    };
    let size_independent = evidence.is_none()
        && guarantee.as_ref().is_some_and(unknown)
        && !model_or_evidence_may_judge;
    Err(CheckFailure {
        cause: RejectionCause::AccuracyTargetMissed,
        size_independent,
        reason: match guarantee {
            Some(g)
                if values.is_none()
                    && target.accuracy_scope == AccuracyScope::WholeResult
                    && target.failure_probability.is_some()
                    && g.spec.failure_probability.evaluate().is_none() =>
            {
                format!(
                    "q{query}: the output's guarantee holds for each value, and the number \
                     of values one evaluation returns is not bound (no declared series \
                     count for a source a PromQL vector reads, and no declared \
                     `distinct_values_per_label` for every group key nor a \
                     `distinct_values_per_label_set` covering them, or a key that is not \
                     an unchanged source column), so it does not meet \
                     whole-result target {target:?}, and \
                     the deployment has no evidence that it meets it (a target on the groups \
                     holding a share of the records, `groups_with_record_share_at_least`, \
                     covers a bound number of values)"
                )
            }
            Some(g) => format!(
                "q{query}: the output guarantees a {:?} bound {:?}, failure probability \
                 {:?}, which misses target {target:?}, and the deployment has no evidence \
                 that it meets it",
                g.spec.accuracy_metric,
                g.spec.error.evaluate(),
                g.spec.failure_probability.evaluate(),
            ),
            None => format!(
                "q{query}: the output has no guarantee, and the deployment has no \
                 evidence that it meets target {target:?}"
            ),
        },
    })
}
