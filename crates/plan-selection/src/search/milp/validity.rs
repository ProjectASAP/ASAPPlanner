//! Validity before cost: the memo MILP's choices that are invalid in every
//! plan containing them, left out before the model is built, so the MILP
//! never proposes a plan Stage 3 rejects for one of them.
//!
//! It runs after Stage 2 and the coefficient derivation (each choice is a
//! candidate in one Stage 2 materialization, with its coefficients), and
//! before the model; Stages 1 and 2 stay cost-free. Each rule is checked on
//! one choice (or one candidate, for all of its choices) and is exact: it
//! removes a choice only when every plan containing it fails a Stage 3
//! check, as each rule's doc comment argues. Rules, in the order checked:
//!
//! 1. [`PruneReason::RawDataNotRetained`] ([`runs_raw_scan_at_query_time`]);
//! 2. [`PruneReason::NotExecutable`] ([`not_executable`]);
//! 3. [`PruneReason::NoAccuracyGuarantee`] ([`has_no_guarantee_at_any_threshold`]);
//! 4. [`PruneReason::LatencyBoundExceeded`] ([`over_every_latency_bound`]).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use asap_logical_asap_optimization::per_computation_candidates::recompose_guarantees;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::physical::Materialization;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::model::{Choice, Via};
use crate::search::memo::Memo;
use crate::PlanningModels;

/// Why a choice is invalid in every plan containing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PruneReason {
    /// Its output has no accuracy guarantee at any size threshold of its
    /// coupling sites.
    NoAccuracyGuarantee,
    /// The executor cannot run one of its own nodes.
    NotExecutable,
    /// Its query-time work alone is over the latency bound of every query
    /// that may reach it.
    LatencyBoundExceeded,
    /// It scans raw data at query time in a deployment that keeps none.
    RawDataNotRetained,
}

impl PruneReason {
    /// Its name in [`crate::MemoStats::pruned_by_reason`].
    pub(crate) fn name(self) -> &'static str {
        match self {
            PruneReason::NoAccuracyGuarantee => "no_accuracy_guarantee",
            PruneReason::NotExecutable => "not_executable",
            PruneReason::LatencyBoundExceeded => "latency_bound_exceeded",
            PruneReason::RawDataNotRetained => "raw_data_not_retained",
        }
    }
}

/// Per choice of `choices` (the memo's choices with their raw scans, as
/// the model reads them: `raw_scans[c]` is choice `c`'s query-time raw
/// scans by part), why it is invalid in every plan, or `None`.
pub(crate) fn invalid_choices(
    memo: &Memo,
    choices: &[Choice],
    raw_scans: &[Vec<Via>],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Vec<Option<PruneReason>> {
    let may_run_at_ingestion = may_run_at_ingestion(memo, choices);
    let mut not_executable_candidates: HashMap<(usize, usize), bool> = HashMap::new();
    let mut without_guarantee_candidates: HashMap<(usize, usize), bool> = HashMap::new();
    let guarantee_decides = guarantee_decides(models);
    let latency_bounds = latency_bounds_ms(memo, entries);
    choices
        .iter()
        .enumerate()
        .map(|(c, choice)| {
            let g = choice.group.0 as usize;
            let key = (g, choice.candidate);
            if !models.deployment_capabilities.raw_data_retained
                && runs_raw_scan_at_query_time(choice, &raw_scans[c], may_run_at_ingestion[g])
            {
                return Some(PruneReason::RawDataNotRetained);
            }
            if *not_executable_candidates
                .entry(key)
                .or_insert_with(|| not_executable(memo, g, choice.candidate, data).is_some())
            {
                return Some(PruneReason::NotExecutable);
            }
            if guarantee_decides
                && *without_guarantee_candidates
                    .entry(key)
                    .or_insert_with(|| has_no_guarantee_at_any_threshold(memo, g, choice.candidate))
            {
                return Some(PruneReason::NoAccuracyGuarantee);
            }
            if over_every_latency_bound(choice, latency_bounds[g], may_run_at_ingestion[g]) {
                return Some(PruneReason::LatencyBoundExceeded);
            }
            None
        })
        .collect()
}

/// Per group, whether some plan may run it at ingestion time: some choice
/// reads it through an ingestion-time slot, or reads it while it may run at
/// ingestion time itself. A group for which this is `false` runs at query
/// time in every plan containing it: every query reaching it reaches it at
/// query time (the model's `at_query_time`).
fn may_run_at_ingestion(memo: &Memo, choices: &[Choice]) -> Vec<bool> {
    let mut at_ingestion = vec![false; memo.groups.len()];
    // Groups are children first: parents' choices are visited before their
    // children's, so a parent's flag is final when its choices are.
    let mut order: Vec<usize> = (0..choices.len()).collect();
    order.sort_by_key(|&c| std::cmp::Reverse(choices[c].group.0));
    for c in order {
        let choice = &choices[c];
        let parent = at_ingestion[choice.group.0 as usize];
        let vias = &choice.coefficients.child_via;
        if vias.len() != choice.children.len() {
            // Not the model's shape: assume any child may be.
            for child in &choice.children {
                at_ingestion[child.0 as usize] = true;
            }
            continue;
        }
        for (child, via) in choice.children.iter().zip(vias) {
            let through_ingestion_slot = matches!(via, Via::Slot(i)
                if matches!(choice.coefficients.slots[*i].materialization, Materialization::MatAtIngestion(_)));
            if parent || through_ingestion_slot {
                at_ingestion[child.0 as usize] = true;
            }
        }
    }
    at_ingestion
}

/// Rule (d), raw data not retained: whether `choice`, whose query-time raw
/// scans are by part `raw_scans`, scans raw data at query time in every
/// plan containing it. A private slot that is not materialized at
/// ingestion runs at query time whenever the choice is chosen; its direct
/// part runs at query time for every consumer but an ingestion-time slot
/// of a parent, so whenever its group cannot run at ingestion
/// (`group_may_run_at_ingestion` false). Without raw data retained, Stage 3
/// rejects every such plan (`RawDataNotRetained`), and the model already
/// forbids the choice; leaving it out only shrinks the model.
pub(crate) fn runs_raw_scan_at_query_time(
    choice: &Choice,
    raw_scans: &[Via],
    group_may_run_at_ingestion: bool,
) -> bool {
    raw_scans.iter().any(|via| match via {
        Via::Direct => !group_may_run_at_ingestion,
        Via::Slot(i) => !matches!(
            choice.coefficients.slots[*i].materialization,
            Materialization::MatAtIngestion(_)
        ),
    })
}

/// Rule (b), not executable: why the executor cannot run one of the
/// candidate's own nodes (those down to its child groups' nodes), or
/// `None`. These are Stage 3's size-independent executability checks: an
/// exact per-series aggregate with no kernel
/// (`PerEntityAggregateNotExecutable`) and a Count-Min over update weights
/// not proven non-negative (`CountMinWeightsNotProvenNonNegative`). Each
/// depends only on the node itself (its measures, filters, family and
/// declared weight domain) and the data workload, not on the other
/// groups' choices, the sizes the coupling sites set or the
/// materialization, and the node is in every plan containing the
/// candidate, so Stage 3 rejects every such plan.
///
/// The deployment's compile check
/// ([`crate::PlanningModels::executor_compile_check`]) is not asked here:
/// it compiles a whole plan, and its verdict on a candidate planned alone
/// does not bind plans whose other choices differ.
pub(crate) fn not_executable(
    memo: &Memo,
    group: usize,
    candidate: usize,
    data: &DataWorkload,
) -> Option<String> {
    let candidate = &memo.candidates[group][candidate];
    let boundary: HashSet<*const OperatorNode> = candidate
        .children
        .iter()
        .map(|child| Rc::as_ptr(&memo.groups[child.0 as usize].node))
        .collect();
    private_nodes(&candidate.node, &boundary)
        .into_iter()
        .find_map(|node| match &node.operator {
            Operator::NonASAP(aggregate @ NonASAPOp::Aggregate { .. }) => {
                crate::per_entity_aggregate_violation_of(aggregate)
            }
            Operator::ASAP(
                ASAPOp::SummaryAgg { family, input, .. } | ASAPOp::PaneChain { family, input, .. },
            ) if crate::count_min_over_unproven_weights(family, &input.weight_domain, data) => {
                Some(format!(
                    "{} needs non-negative update weights, and these are not proven non-negative",
                    crate::family_name(family)
                ))
            }
            _ => None,
        })
}

/// `node` and its descendants down to, not including, the nodes in
/// `boundary` (a candidate's child groups' nodes).
fn private_nodes(
    node: &Rc<OperatorNode>,
    boundary: &HashSet<*const OperatorNode>,
) -> Vec<Rc<OperatorNode>> {
    let mut seen: HashSet<*const OperatorNode> = HashSet::new();
    let mut stack = vec![Rc::clone(node)];
    let mut out = Vec::new();
    while let Some(node) = stack.pop() {
        let pointer = Rc::as_ptr(&node);
        if boundary.contains(&pointer) || !seen.insert(pointer) {
            continue;
        }
        stack.extend(node.children().into_iter().cloned());
        out.push(node);
    }
    out
}

/// Whether a query output's guarantee alone decides whether Stage 3
/// accepts it: guarantees are enforced, the deployment's evidence supplies
/// none, and its accuracy model accepts no output whatever its guarantee
/// says.
fn guarantee_decides(models: PlanningModels<'_>) -> bool {
    !models.plan_selection.ignore_accuracy_guarantees
        && !models.evidence.may_provide_evidence()
        && !models
            .effective_accuracy()
            .may_accept_regardless_of_guarantee()
}

/// Whether `node`, its guarantees recomposed over its own tree, has no
/// accuracy guarantee: an approximate input that no rule composes.
fn has_no_guarantee(node: &Rc<OperatorNode>) -> bool {
    recompose_guarantees(&[QueryRoot::Operator(Rc::clone(node))]).is_ok_and(
        |roots| matches!(&roots[0], QueryRoot::Operator(root) if root.guarantee.is_none()),
    )
}

/// Rule (a), no accuracy guarantee: whether the candidate's node, over its
/// child groups' own nodes (each exact), has no accuracy guarantee at every
/// size threshold of its coupling sites, when the guarantee alone decides
/// acceptance ([`guarantee_decides`]).
///
/// Whether a node has a guarantee depends on the rules that compose its
/// inputs' errors, not on summary sizes: no size Stage 3 or a shared build
/// sets gives it one. Over approximate children instead of exact ones, a
/// rule composes no more, and no node above restores a guarantee to an
/// input without one. So every query a plan containing the candidate leads
/// it to has an output without a guarantee, and Stage 3 rejects the plan
/// (`NoAccuracyGuarantee`). A guarantee whose failure probability misses a
/// target is not ruled out here: Stage 3 resizes summaries for their
/// failure probability (`sized_for_failure_probability`), so a size the
/// memo does not list may meet it. Nothing is ruled out when the candidate
/// has no version at its thresholds (it then has no choice either).
pub(crate) fn has_no_guarantee_at_any_threshold(
    memo: &Memo,
    group: usize,
    candidate: usize,
) -> bool {
    let versions =
        super::coefficients::at_site_thresholds(memo, &memo.candidates[group][candidate]);
    !versions.is_empty()
        && versions
            .iter()
            .all(|(version, _)| has_no_guarantee(&version.node))
}

/// Per group, the largest latency bound in ms among the queries that may
/// reach it (each the tighter of its own bound and its repeat interval, as
/// the model's [`super::model::ModelQuery::max_latency_ms`]); `None` when one
/// of them has neither.
fn latency_bounds_ms(memo: &Memo, entries: &[QueryWorkloadEntry]) -> Vec<Option<f64>> {
    let bound = |entry: &QueryWorkloadEntry| {
        let interval = entry.recurrence.fixed_interval_ms().map(|ms| ms as f64);
        match (entry.requirements.max_latency_ms, interval) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    };
    memo.groups
        .iter()
        .map(|group| {
            let bounds: Option<Vec<f64>> =
                group.queries.iter().map(|&q| bound(&entries[q])).collect();
            bounds?.into_iter().max_by(f64::total_cmp)
        })
        .collect()
}

/// Rule (c), latency bound exceeded: whether `choice`'s query-time work per
/// computation (its direct part and its private slots not materialized at
/// ingestion, Stage 3's per-node work at the windows it reads itself) is
/// above `largest_bound_ms`, the largest latency bound of the queries that
/// may reach its group, when the group cannot run at ingestion time.
///
/// Such a group runs at query time in every plan containing it, so some
/// query reaching it adds that work to its latency (Stage 3 counts each
/// query-time node a query reaches once; sharing a scan only widens it, and
/// a shared node built once still adds its work to each reader). That
/// query's latency is then above its own bound, the tighter of its latency
/// bound and repeat interval, and Stage 3 rejects the plan
/// (`LatencyBoundExceeded` or `LatencyOverRepeatInterval`).
pub(crate) fn over_every_latency_bound(
    choice: &Choice,
    largest_bound_ms: Option<f64>,
    group_may_run_at_ingestion: bool,
) -> bool {
    let Some(bound) = largest_bound_ms else {
        return false;
    };
    if group_may_run_at_ingestion {
        return false;
    }
    let k = &choice.coefficients;
    let work_ms = k.direct.work_ms
        + k.slots
            .iter()
            .filter(|s| !matches!(s.materialization, Materialization::MatAtIngestion(_)))
            .map(|s| s.work_ms)
            .sum::<f64>();
    work_ms > bound * (1.0 + 1e-9)
}

/// `pruned`'s counts by reason name, for [`crate::MemoStats::pruned_by_reason`].
pub(crate) fn counts(pruned: &[Option<PruneReason>]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for reason in pruned.iter().flatten() {
        *counts.entry(reason.name().to_string()).or_default() += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_logical_asap_optimization::deployment_capabilities::DeploymentCapabilities;
    use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::schema_support::with_promql_series_identity;
    use asap_types::workload::{
        DataArrival, MetricType, Predictability, QueryRecurrence, RepeatedDemand,
        RepetitionInterval, SourceFacts,
    };

    /// `queries`' memo (each every minute at `target`, over the gauge `m`
    /// and the counter-like `c_total`, each 10,000 series at
    /// `rows_per_second`), its choices as the MILP search keeps them
    /// (copies and widened scans left out) and why each is invalid.
    fn plan_memo(
        queries: &[&str],
        target: AccuracyTarget,
        rows_per_second: f64,
        deployment: &DeploymentCapabilities,
    ) -> (Memo, Vec<Choice>, Vec<Option<PruneReason>>) {
        let roots: Vec<(usize, QueryRoot)> = queries
            .iter()
            .map(|q| {
                let root = lower_promql(q, target.clone());
                QueryRoot::Operator(with_promql_series_identity(&root).unwrap())
            })
            .enumerate()
            .collect();
        let per_computation = generate_per_computation_candidates(roots).unwrap();
        let entries: Vec<QueryWorkloadEntry> = queries
            .iter()
            .map(|_| {
                let mut entry = entry_with(
                    target.clone(),
                    QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
                        60_000,
                    ))),
                );
                entry.predictability = Predictability::Predictable { known_at: None };
                entry
            })
            .collect();
        let source = |metric_type| SourceFacts {
            series_count: Some(10_000),
            metric_type: Some(metric_type),
            ..SourceFacts::default()
        };
        let data = DataWorkload {
            arrival: DataArrival::Ingesting { rows_per_second },
            sources: [
                ("m".to_string(), source(MetricType::Gauge)),
                ("c_total".to_string(), source(MetricType::Gauge)),
            ]
            .into_iter()
            .collect(),
        };
        let models = PlanningModels::builtin().with_deployment_capabilities(deployment);
        let memo = crate::search::memo::brute_force::memo(&per_computation, &entries);
        let scale =
            crate::cost::per_node::frozen_calibration(&memo, &entries, &data, models).unwrap();
        let mut cache = crate::cost::per_node::PriceCache::new(scale);
        let mut choices = Vec::new();
        let mut raw_scans = Vec::new();
        for g in 0..memo.groups.len() {
            let (group_choices, lazy) = crate::search::milp::coefficients::choices(
                &memo,
                crate::search::memo::GroupId(g as u32),
                &entries,
                &data,
                models,
                &mut cache,
            )
            .unwrap();
            for (choice, lazy) in group_choices.into_iter().zip(lazy) {
                if choice.copy_of.is_some()
                    || memo.candidates[g][choice.candidate].origin
                        == crate::search::memo::CandidateOrigin::WidenedScans
                {
                    continue;
                }
                raw_scans.push(lazy.raw_scans.iter().map(|(_, _, via)| *via).collect());
                choices.push(choice);
            }
        }
        let pruned = invalid_choices(&memo, &choices, &raw_scans, &entries, &data, models);
        (memo, choices, pruned)
    }

    fn raw() -> DeploymentCapabilities {
        DeploymentCapabilities::default()
    }

    fn no_raw() -> DeploymentCapabilities {
        DeploymentCapabilities {
            raw_data_retained: false,
            ..DeploymentCapabilities::default()
        }
    }

    fn target() -> AccuracyTarget {
        AccuracyTarget::max_error(0.05).failure_probability(0.01)
    }

    fn pruned_for(pruned: &[Option<PruneReason>], reason: PruneReason) -> Vec<usize> {
        (0..pruned.len())
            .filter(|&c| pruned[c] == Some(reason))
            .collect()
    }

    /// Rule (d): without raw data, a 5-minute sum's choices that scan raw
    /// data at query time are left out, and the panes built at ingestion
    /// time stay for its root group; with raw data, none is.
    #[test]
    fn query_time_raw_scans_are_pruned_without_raw_data() {
        let query = ["sum_over_time(m[5m])"];
        let (memo, choices, pruned) = plan_memo(&query, target(), 10_000.0, &no_raw());
        let out = pruned_for(&pruned, PruneReason::RawDataNotRetained);
        assert!(!out.is_empty(), "{pruned:?}");
        let root = memo
            .groups
            .iter()
            .position(|g| !g.roots_of.is_empty())
            .unwrap();
        assert!(
            (0..choices.len()).any(|c| choices[c].group.0 as usize == root && pruned[c].is_none()),
            "the root keeps a choice"
        );
        let (_, _, with_raw) = plan_memo(&query, target(), 10_000.0, &raw());
        assert!(pruned_for(&with_raw, PruneReason::RawDataNotRetained).is_empty());
    }

    /// Rule (b): the exact per-series standard deviation has no executor
    /// kernel; that candidate's choices are left out, and its summary
    /// candidates stay.
    #[test]
    fn a_per_series_aggregate_without_a_kernel_is_not_executable() {
        let (memo, choices, pruned) =
            plan_memo(&["stddev_over_time(m[5m])"], target(), 10_000.0, &raw());
        let out = pruned_for(&pruned, PruneReason::NotExecutable);
        assert!(!out.is_empty(), "{pruned:?}");
        for c in out {
            let reason = not_executable(
                &memo,
                choices[c].group.0 as usize,
                choices[c].candidate,
                &DataWorkload::default(),
            );
            assert!(reason.unwrap().contains("no exact per-series aggregate"));
        }
        assert!(pruned.iter().any(Option::is_none));
    }

    /// Rule (b): a Count-Min heap over update weights not proven
    /// non-negative (rates of an undeclared metric) does not run.
    #[test]
    fn a_count_min_over_unproven_weights_is_not_executable() {
        let (_, _, pruned) = plan_memo(
            &["topk(5, sum by (job) (rate(c_total[5m])))"],
            target(),
            10_000.0,
            &raw(),
        );
        assert!(!pruned_for(&pruned, PruneReason::NotExecutable).is_empty());
    }

    /// Rule (a): a top-k over rates: some candidates' outputs have no
    /// accuracy guarantee at any size threshold, and every one left out has
    /// none. With guarantees ignored, the guarantee does not decide and
    /// nothing is left out for it.
    #[test]
    fn a_candidate_without_a_guarantee_is_pruned() {
        let query = ["topk(5, sum by (job) (rate(c_total[5m])))"];
        let (memo, choices, pruned) = plan_memo(&query, target(), 10_000.0, &raw());
        let out = pruned_for(&pruned, PruneReason::NoAccuracyGuarantee);
        assert!(!out.is_empty(), "{pruned:?}");
        for c in out {
            let candidate = &memo.candidates[choices[c].group.0 as usize][choices[c].candidate];
            assert!(has_no_guarantee(&candidate.node));
        }
        assert!(guarantee_decides(PlanningModels::builtin()));
        let ignored = crate::PlanSelectionSettings {
            ignore_accuracy_guarantees: true,
            ..crate::PlanSelectionSettings::default()
        };
        assert!(!guarantee_decides(
            PlanningModels::builtin().with_plan_selection(&ignored)
        ));
    }

    /// Rule (c): at a billion rows per second, a 5-minute sum read from raw
    /// data at query time takes longer than its one-minute repeat interval:
    /// those choices are left out, and every one left out has query-time
    /// work above it; the panes built at ingestion time stay. At 10,000
    /// rows per second, none is.
    #[test]
    fn query_time_work_over_every_bound_is_pruned() {
        let query = ["sum_over_time(m[5m])"];
        let (_, choices, pruned) = plan_memo(&query, target(), 1e9, &raw());
        let out = pruned_for(&pruned, PruneReason::LatencyBoundExceeded);
        assert!(!out.is_empty(), "{pruned:?}");
        for c in out {
            let k = &choices[c].coefficients;
            let work = k.direct.work_ms
                + k.slots
                    .iter()
                    .filter(|s| !matches!(s.materialization, Materialization::MatAtIngestion(_)))
                    .map(|s| s.work_ms)
                    .sum::<f64>();
            assert!(work > 60_000.0, "{work}");
        }
        assert!(pruned.iter().any(Option::is_none));
        let (_, _, slow) = plan_memo(&query, target(), 10_000.0, &raw());
        assert!(pruned_for(&slow, PruneReason::LatencyBoundExceeded).is_empty());
    }

    /// The counts by reason name, as the selection report shows them.
    #[test]
    fn counts_are_by_reason_name() {
        let counted = counts(&[
            None,
            Some(PruneReason::NotExecutable),
            Some(PruneReason::NotExecutable),
            Some(PruneReason::RawDataNotRetained),
        ]);
        assert_eq!(
            counted,
            [
                ("not_executable".to_string(), 2),
                ("raw_data_not_retained".to_string(), 1)
            ]
            .into()
        );
    }
}
