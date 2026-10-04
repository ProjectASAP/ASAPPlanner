//! Legacy whole-workload selection over the Stage 1 search space
//! ([`CandidateLogicalASAPDAGs`]): per-target cost ranking, recurrence
//! profiles and global selection. These are free functions over Stage 1
//! types, so that Stage 1 does not depend on the cost model. Assembling the
//! selected DAG is Stage 1's [`GlobalSelection`]; [`CostedGlobalSelection`]
//! adds the cost comparison behind each chosen exact composition.
//!
//! The stage pipeline does not call this module. It is deleted under #580.

use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use asap_types::ir::schema::{FieldDataType, GroupingStrategy, SketchAlgorithm};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};

use crate::cost::cost_model::{
    raw_recompute_cost_rate, CostModel, CseCandidate, ExactCompositionCostInputs,
    ExactCompositionCostRequest, ShareDecision,
};
use crate::cost::recurrence::{
    CostRate, Horizon, RecurrenceError, RecurrenceProfile, RootRecurrence, UpdateRate,
};
use asap_logical_optimizer::pass1::exact_composition::OperationPlacement;
use asap_logical_optimizer::pass1::replacement::{
    bindable_intent, cse_candidate_pair, direct_child_counts, is_logical_rewrite, realize_child,
    CandidateLogicalASAPDAGs, GlobalSelection, PreparedComposition, Replacement,
    ReplacementProvenance, ReplacementSubDAG, TargetSubDAG, TargetSubDAGCandidates,
    TargetSubDAGSelection,
};

/// Physical feasibility evidence for `candidate`. A pure logical
/// rewrite needs no new operator. Unknown support is checked during
/// physical/deployment compilation; explicit rejection prevents selection.
pub fn runtime_support_evidence(
    candidate: &ReplacementSubDAG,
    cost_model: &dyn CostModel,
) -> Option<bool> {
    match &candidate.replacement {
        Replacement::ExactComposition(composition) => {
            cost_model.value_operation_support_evidence(&composition.op, composition.placement)
        }
        // Any summary decision, including one rooted in a relational
        // operator above its evaluations, asks the deployment for support.
        Replacement::SubDAG(node) if !is_logical_rewrite(node) => {
            cost_model.summary_support_evidence(node)
        }
        Replacement::SubDAG(_) => Some(true),
    }
}

/// The `sorted_by(cost_model)` step: every group, each with its own
/// candidates ranked best-first under `cost_model` where this module
/// knows how (see the module docs' "Cost-based final selection"
/// section) — groups themselves stay in discovery order, since targets
/// are independent decision points, not alternatives competing with
/// each other.
///
/// Ranking itself is decided entirely by [`rank_group`] before
/// [`RankedTargetSubDAGCandidates::costs`] is ever computed — pairing each candidate with
/// [`CostModel::grouping_state_cost`] for grouping alternatives, or
/// [`CostModel::estimate_cost`] otherwise, is an additive annotation
/// for a caller that wants to *display* a cost (e.g. a
/// DAG-visualization view), not a second ranking signal, so plugging in
/// a `CostModel` whose `estimate_cost` disagrees with its own
/// `rank_candidates`/`cse_share_decision` (a deployment bug, not
/// something this method tries to protect against) would show a
/// `RankedTargetSubDAGCandidates` whose `costs` aren't monotonically non-decreasing —
/// `cost_sorted`'s own ordering guarantee is unaffected either way.
pub fn cost_sorted<'a, Id>(
    space: &'a CandidateLogicalASAPDAGs<Id>,
    cost_model: &dyn CostModel,
) -> Vec<RankedTargetSubDAGCandidates<'a>> {
    space
        .order()
        .iter()
        .map(|ptr| {
            let group = &space.groups()[ptr];
            let target = TargetSubDAG::with_consumer_count(&group.target, group.consumer_count);
            let mut candidates = rank_group(group, cost_model);
            // Availability is candidate-specific and cannot be expressed
            // by `rank_candidates`' exhaustive permutation contract.
            // Keep unavailable alternatives for explanation, but place
            // them after every selectable candidate.
            candidates
                .sort_by_key(|candidate| cost_model.candidate_cost(candidate, &target).is_none());
            let costs = candidates
                .iter()
                .map(|c| {
                    cost_model
                        .grouping_state_cost(c, &target)
                        .map_or_else(|| cost_model.estimate_cost(c, &target), |cost| cost.0)
                })
                .collect();
            RankedTargetSubDAGCandidates {
                target: &group.target,
                consumer_count: group.consumer_count,
                candidates,
                costs,
            }
        })
        .collect()
}

/// Recurrence-aware counterpart to [`cost_sorted`]. CSE
/// share/recompute pairs are ordered with the target's recurrence
/// profile; all other candidate shapes retain their existing ranking.
pub fn cost_sorted_with_recurrence<'a, Id>(
    space: &'a CandidateLogicalASAPDAGs<Id>,
    cost_model: &dyn CostModel,
    profiles: &RecurrenceProfileMap,
    horizon: Option<Horizon>,
) -> Result<Vec<RankedTargetSubDAGCandidates<'a>>, RecurrenceError> {
    space
        .order()
        .iter()
        .map(|ptr| {
            let group = &space.groups()[ptr];
            let mut candidates = rank_group(group, cost_model);
            if cse_candidate_pair(group).is_some() {
                if let Some(decision) = decide_group_with_recurrence(
                    group,
                    group.consumer_count,
                    profiles.for_target(&group.target),
                    horizon,
                    cost_model,
                )? {
                    candidates.sort_by_key(|candidate| match candidate.provenance {
                        ReplacementProvenance::CseShare if decision == ShareDecision::Share => 0,
                        ReplacementProvenance::CseRecompute
                            if decision == ShareDecision::RecomputeIndependently =>
                        {
                            0
                        }
                        ReplacementProvenance::CseShare | ReplacementProvenance::CseRecompute => 2,
                        _ => 1,
                    });
                }
            }
            let target = TargetSubDAG::with_consumer_count(&group.target, group.consumer_count);
            let costs = candidates
                .iter()
                .map(|candidate| {
                    cost_model
                        .grouping_state_cost(candidate, &target)
                        .map_or_else(
                            || cost_model.estimate_cost(candidate, &target),
                            |cost| cost.0,
                        )
                })
                .collect();
            Ok(RankedTargetSubDAGCandidates {
                target: &group.target,
                consumer_count: group.consumer_count,
                candidates,
                costs,
            })
        })
        .collect()
}

// ── Recurrence-aware cost context (issue #287) ──────────────────────────

/// One [`RecurrenceProfile`] per discovered [`TargetSubDAGCandidates`] target, built by
/// [`recurrence_profiles`] — the "carry `RepeatingEntry.demand`
/// and relevant `DataWorkload` into ASAP-aware search/cost context"
/// half of issue #287. Looked up by `Rc` pointer identity, the same
/// currency [`CandidateLogicalASAPDAGs::candidates_for_target`]/[`GlobalSelection::for_target`] already
/// use.
/// Holds an owned `Rc<OperatorNode>` clone alongside each profile (not just
/// its raw pointer) so this map keeps every node it describes alive for as
/// long as the map itself lives — a `RecurrenceProfileMap` is safe to outlive
/// the `CandidateLogicalASAPDAGs` it was built from. Without this, a raw `*const OperatorNode` key
/// could, after the originating `CandidateLogicalASAPDAGs` (the only other owner of those
/// `Rc`s) is dropped, collide with an unrelated, later allocation that
/// happens to reuse the same freed address — silently returning a stale
/// profile for the wrong node (issue #287 review, bug 4).
#[derive(Debug, Clone)]
pub struct RecurrenceProfileMap {
    profiles: HashMap<*const OperatorNode, (Rc<OperatorNode>, RecurrenceProfile)>,
}

impl RecurrenceProfileMap {
    /// The [`RecurrenceProfile`] for `target`, or
    /// [`RecurrenceProfile::EMPTY`] when `target` wasn't a discovered site
    /// in the [`CandidateLogicalASAPDAGs`] this map was built from (or carried no
    /// recurring/one-shot/update-rate metadata at all) — always a valid,
    /// "no metadata" answer, never a panic.
    pub fn for_target(&self, target: &Rc<OperatorNode>) -> RecurrenceProfile {
        self.profiles
            .get(&Rc::as_ptr(target))
            .map(|(_, profile)| *profile)
            .unwrap_or(RecurrenceProfile::EMPTY)
    }
}

/// Build one [`RecurrenceProfile`] per discovered site, by walking every
/// root's whole reachable sub-DAG (the same relational-skeleton
/// traversal `discover_targets` itself used to discover those sites)
/// and folding each root's own recurrence tag
/// (a normalized repeating rate or a one-time invocation count) into every
/// site reachable from it.
///
/// `root_recurrence` is positional: `root_recurrence[i]` describes
/// `space.roots[i]` — the same order [`search_workload`]/
/// [`search_workload_with`] were originally called with (post-CSE
/// dedup preserves both root count and order — see
/// `asap_types::ir::cse::share_common_sub_dags`'s own
/// `.map(...).collect()` body). This keeps `Id` fully opaque (no `Eq`/
/// `Hash`/`Clone` bound needed on it at all — issue #287's "keep
/// caller/query identifiers opaque" requirement) at the cost of the
/// caller keeping the two slices in step; `root_recurrence.len()` must
/// equal `space.roots.len()`.
///
/// A shared sub-DAG reachable from more than one root aggregates every
/// reaching root's contribution — repeating roots' rates are summed and
/// one-shot roots
/// increment [`RecurrenceProfile::one_shot_consumers`] — so a summary
/// consumed by queries with different intervals gets one profile
/// reflecting all of them, per issue #287's "support a shared sub-DAG
/// consumed by queries with different intervals".
///
/// `update_rate` is applied uniformly to every discovered site *that
/// this walk actually reached from some root* (see the "unreachable
/// sites" note below): today's
/// [`asap_types::workload::DataWorkload`] is a single
/// workload-level value (applies to every query in a `QueryWorkload`),
/// not per-target, so there is no finer-grained source to attach
/// instead. `None` when no `DataWorkload` evidence was available —
/// preserves "missing metadata" behavior for the update-rate term alone
/// even when repeating/one-shot consumer information is present.
///
/// A parent that structurally references the same child more than once
/// (e.g. `BinaryOp{lhs: X, rhs: X}`) credits that child with one
/// contribution per reference, not one contribution per distinct node —
/// matching how [`TargetSubDAGCandidates::consumer_count`] counts that occurrence.
/// Multiplicity is propagated through the full descendant path: if the
/// repeated parent is independently evaluated twice, its child is also
/// evaluated twice. This supplies recurrence-aware selection with the
/// effective structural execution rate rather than mere reachability.
///
/// **Unreachable sites**: [`CandidateLogicalASAPDAGs`] can contain a site no root's own
/// structural DAG actually reaches — e.g. one only ever produced by a
/// [`Replacement::Rewrite`] candidate a [`ReplacementStrategy`] invented
/// (this walk only follows [`TargetSubDAGCandidates::target`]'s own structural
/// children, the same scope `discover_targets` uses for the original
/// roots, never a candidate's rewritten value). Such a site gets
/// [`RecurrenceProfile::EMPTY`] — in particular, `update_rate` is
/// **not** stamped onto it — so it falls back to the ordinary
/// structural decision instead of being charged an ingest-driven
/// maintenance cost against a real evaluation/one-shot signal of
/// exactly zero, which previously made `RecomputeIndependently` win
/// there unconditionally, regardless of the site's actual
/// `consumer_count` (issue #287 review, bug 2).
///
/// Returns [`RecurrenceError::InvalidEvaluationRate`] if any repeating
/// rate is non-finite or negative,
/// [`RecurrenceError::InvalidUpdateRate`] if `update_rate` is non-finite
/// or negative, or [`RecurrenceError::RootCountMismatch`] if
/// `root_recurrence.len() != space.roots.len()`.
pub fn recurrence_profiles<Id>(
    space: &CandidateLogicalASAPDAGs<Id>,
    root_recurrence: &[RootRecurrence],
    update_rate: Option<UpdateRate>,
) -> Result<RecurrenceProfileMap, crate::cost::recurrence::RecurrenceError> {
    if root_recurrence.len() != space.roots.len() {
        return Err(
            crate::cost::recurrence::RecurrenceError::RootCountMismatch {
                expected: space.roots.len(),
                got: root_recurrence.len(),
            },
        );
    }
    if let Some(rate) = update_rate {
        crate::cost::recurrence::validate_update_rate(rate)?;
    }
    for recurrence in root_recurrence {
        if let RootRecurrence::Repeating(rate) = recurrence {
            if !rate.0.is_finite() || rate.0 < 0.0 {
                return Err(crate::cost::recurrence::RecurrenceError::InvalidEvaluationRate(*rate));
            }
        }
    }

    let mut rates: HashMap<*const OperatorNode, f64> = HashMap::new();
    let mut one_shot_counts: HashMap<*const OperatorNode, usize> = HashMap::new();
    // Sites actually reached by at least one root's own recurrence tag
    // during the walk below — see this method's own "Unreachable
    // sites" doc.
    let mut reached: HashSet<*const OperatorNode> = HashSet::new();

    for ((_, root), recurrence) in space.roots.iter().zip(root_recurrence) {
        let recurrence = *recurrence;
        let root_ptr = Rc::as_ptr(root);
        // Carry path multiplicity transitively. If a shared ancestor is
        // referenced twice, every descendant below an independently
        // recomputed occurrence is evaluated twice as well; stopping
        // expansion after the first pointer visit undercounts exactly
        // the effective-consumer rate recurrence-aware costing needs.
        let mut queue: VecDeque<(*const OperatorNode, usize)> = VecDeque::new();
        queue.push_back((root_ptr, 1));

        while let Some((ptr, path_count)) = queue.pop_front() {
            contribute(
                ptr,
                path_count,
                recurrence,
                &mut rates,
                &mut one_shot_counts,
                &mut reached,
            );
            // Every reachable node was itself discovered as its own
            // `TargetSubDAGCandidates` (`discover_targets` walks the identical
            // relational-skeleton scope) — its own `target` is the
            // canonical `Rc` to read children off.
            if let Some(group) = space.groups().get(&ptr) {
                for (child, edge_count) in direct_child_counts(&group.target) {
                    queue.push_back((
                        child,
                        path_count
                            .checked_mul(edge_count)
                            .expect("query DAG path multiplicity overflowed usize"),
                    ));
                }
            }
        }
    }

    let mut profiles = HashMap::with_capacity(space.order().len());
    for ptr in space.order() {
        let rate = rates.get(ptr).copied().unwrap_or(0.0);
        let evaluation_rate = (rate > 0.0).then_some(crate::cost::recurrence::EvaluationRate(rate));
        let one_shot_consumers = one_shot_counts.get(ptr).copied().unwrap_or(0);
        // Bug 2 fix (see "Unreachable sites" above): only a reached
        // site carries the caller-supplied `update_rate`.
        let site_update_rate = if reached.contains(ptr) {
            update_rate
        } else {
            None
        };
        let node = Rc::clone(&space.groups()[ptr].target);
        profiles.insert(
            *ptr,
            (
                node,
                RecurrenceProfile {
                    evaluation_rate,
                    one_shot_consumers,
                    update_rate: site_update_rate,
                },
            ),
        );
    }

    Ok(RecurrenceProfileMap { profiles })
}

/// Record `times` occurrences of `recurrence` against `ptr` — `times > 1`
/// when a single parent structurally references `ptr` more than once (see
/// [`recurrence_profiles`]'s own doc on edge multiplicity).
/// A no-op for `times == 0` (an `Rc` returned as a `direct_child_counts`
/// child always has `edge_count >= 1` in practice, but this keeps the
/// helper correct regardless).
fn contribute(
    ptr: *const OperatorNode,
    times: usize,
    recurrence: RootRecurrence,
    rates: &mut HashMap<*const OperatorNode, f64>,
    one_shot_counts: &mut HashMap<*const OperatorNode, usize>,
    reached: &mut HashSet<*const OperatorNode>,
) {
    if times == 0 {
        return;
    }
    reached.insert(ptr);
    match recurrence {
        RootRecurrence::Repeating(rate) => {
            *rates.entry(ptr).or_insert(0.0) += rate.0 * times as f64;
        }
        RootRecurrence::OneShotCount(count) => {
            *one_shot_counts.entry(ptr).or_insert(0) += count.saturating_mul(times);
        }
        RootRecurrence::Unknown => {}
    }
}

/// One [`TargetSubDAGCandidates`]'s candidates, ranked best-first by
/// [`cost_sorted`].
#[derive(Debug)]
pub struct RankedTargetSubDAGCandidates<'a> {
    pub target: &'a Rc<OperatorNode>,
    pub consumer_count: usize,
    pub candidates: Vec<&'a ReplacementSubDAG>,
    /// `costs[i]` is `candidates[i]`'s own grouping-state cost when available,
    /// and its [`CostModel::estimate_cost`] otherwise
    /// estimate — aligned index-for-index with `candidates`, one number per
    /// candidate, for a caller that wants an actual `f64` next to each
    /// candidate (e.g. "candidate A costs ≈ X, candidate B costs ≈ Y") and
    /// not just `candidates`' own relative order. `f64::NAN` throughout
    /// unless `cost_model` overrides `estimate_cost` — see that method's own
    /// doc.
    pub costs: Vec<f64>,
}

/// Rank `group`'s candidates best-first under `cost_model`, per the module
/// docs' "Cost-based final selection" section. Falls back to discovery
/// order whenever there's nothing to rank (0 or 1 candidates) or this
/// module doesn't have a defined `CostModel` comparison for the shape it
/// sees — it never invents one.
fn rank_group<'a>(
    group: &'a TargetSubDAGCandidates,
    cost_model: &dyn CostModel,
) -> Vec<&'a ReplacementSubDAG> {
    let mut ranked: Vec<&ReplacementSubDAG> = group.candidates.iter().collect();
    if ranked.len() <= 1 {
        return ranked;
    }

    // Shape 1: the exact `SharedSubDAGStrategy` share-vs-recompute pair —
    // rank via `CostModel::cse_share_decision`, the same comparison
    // the local CSE ranking path already uses.
    if cse_candidate_pair(group).is_some() {
        if let Some(prefer_target) = cse_preference(group, cost_model) {
            ranked.sort_by_key(|c| match c.provenance {
                ReplacementProvenance::CseShare if prefer_target => 0,
                ReplacementProvenance::CseRecompute if !prefer_target => 0,
                ReplacementProvenance::CseShare | ReplacementProvenance::CseRecompute => 2,
                _ => 1,
            });
        }
        return ranked;
    }

    // Shape 2: independent and Hydra grouping alternatives for the same
    // sketch algorithms. When deployment statistics provide a subpopulation
    // estimate, compare N independent states with the shared grid directly.
    let target = TargetSubDAG::with_consumer_count(&group.target, group.consumer_count);
    let has_hydra = ranked.iter().any(|candidate| {
        let Replacement::SubDAG(node) = &candidate.replacement else {
            return false;
        };
        summary_grouping(node).is_some_and(|grouping| {
            matches!(grouping, GroupingStrategy::SharedMultiSubpopulation { .. })
        })
    });
    let grouping_costs: Option<Vec<f64>> = if has_hydra {
        ranked
            .iter()
            .map(|candidate| {
                cost_model
                    .grouping_state_cost(candidate, &target)
                    .map(|cost| cost.0)
            })
            .collect()
    } else {
        None
    };
    if let Some(costs) = grouping_costs {
        let by_ptr: HashMap<*const ReplacementSubDAG, f64> = ranked
            .iter()
            .zip(costs)
            .map(|(candidate, cost)| (*candidate as *const ReplacementSubDAG, cost))
            .collect();
        ranked.sort_by(|a, b| {
            by_ptr[&(*a as *const ReplacementSubDAG)]
                .total_cmp(&by_ptr[&(*b as *const ReplacementSubDAG)])
        });
        return ranked;
    }

    // Shape 3: `ASAPStrategies`'s sketch-family candidates (every
    // candidate is a `Summary` that realizes a `SketchAlgorithm`) — rank via
    // `CostModel::rank_candidates`, the same hook `realizations_for_intent`
    // itself consults.
    if let Some(intent) = bindable_intent(&group.target) {
        let kinds: Option<Vec<SketchAlgorithm>> = ranked
            .iter()
            .map(|c| match &c.replacement {
                Replacement::SubDAG(node) => sketch_kind_of(node),
                Replacement::ExactComposition(_) => None,
            })
            .collect();
        if let Some(kinds) = kinds {
            let order =
                crate::cost::cost_model::validated_candidate_ranking(cost_model, intent, &kinds);
            ranked.sort_by_key(|c| {
                let kind = match &c.replacement {
                    Replacement::SubDAG(node) => sketch_kind_of(node),
                    Replacement::ExactComposition(_) => None,
                };
                kind.and_then(|k| order.iter().position(|o| *o == k))
                    .unwrap_or(usize::MAX)
            });
            return ranked;
        }
    }

    // A target may be handled by more than one strategy (for example, a
    // shared aggregate has both bound-summary and share/recompute rewrite
    // candidates). No shape-specific hook spans those different candidate
    // types, so compare the numeric estimates the CostModel exposes for that
    // purpose. `total_cmp` gives deterministic placement to a model's NaN
    // placeholders without dropping any candidate.
    ranked.sort_by(|a, b| {
        match (
            cost_model.candidate_cost(a, &target),
            cost_model.candidate_cost(b, &target),
        ) {
            (Some(a), Some(b)) => a.0.total_cmp(&b.0),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => cost_model
                .estimate_cost(a, &target)
                .total_cmp(&cost_model.estimate_cost(b, &target)),
        }
    });
    ranked
}

/// For a group whose candidates are all [`Replacement::Rewrite`] (the
/// [`SharedSubDAGStrategy`] shape): does [`CostModel::cse_share_decision`]
/// prefer the candidate that shares `group.target`'s own `Rc` (`true`), or
/// the one that recomputes independently (`false`)? `None` when there's no
/// real comparison to make — fewer than 2 consumers (mirrors
/// [`SharedSubDAGStrategy::matches`]'s own gate), or `group.target` can't
/// actually be bound at all (no candidate and no logical fallback — never
/// expected in practice for a target that's already part of a legitimate
/// workload DAG, but this degrades to "keep discovery order" rather than
/// panicking).
fn cse_preference(group: &TargetSubDAGCandidates, cost_model: &dyn CostModel) -> Option<bool> {
    if group.consumer_count < 2 {
        return None;
    }
    let bound = realize_one(&group.target)?;
    let candidate = CseCandidate {
        sub_dag: &group.target,
        bound_summary: &bound,
        consumer_count: group.consumer_count,
    };
    Some(match cost_model.cse_share_decision(&candidate) {
        ShareDecision::Share => true,
        ShareDecision::RecomputeIndependently => false,
    })
}

/// [`cse_preference`] only needs one representative bound [`OperatorNode`]
/// for `target` (to build a [`CseCandidate`] for
/// [`CostModel::cse_share_decision`]), not the full ranked candidate list
/// [`ASAPStrategies::replacements`] returns — so this just reuses
/// [`realize_child`], the same rank-and-take-first helper
/// `construct_summary_agg`'s own recursion and
/// [`crate::cost::cost_model::DefaultCostModel::estimate_cost`] already use,
/// wrapped to swallow the (here, uninteresting) error into `None`.
fn realize_one(target: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
    realize_child(target).ok()
}

/// The `SketchAlgorithm` a bound [`Replacement::SubDAG`] candidate ultimately
/// realizes, if any (`None` for an `ExactAggregate`/pass-through
/// sub-DAG — nothing to rank against another `SketchAlgorithm`).
///
/// Mirrors this module's own `#[cfg(test)]`-only `summary_family_algorithm`
/// helper (in the test module below), which does the identical
/// `SummaryEstimate`-unwrap-then-match for that module's own tests; that
/// copy is test-only, so this needs its own for real (non-test) ranking
/// code — the same "duplicate a small, self-contained traversal rather than
/// restructure a test helper" call this file's own top doc already makes
/// for `discover_targets`.
pub(crate) fn sketch_kind_of(node: &OperatorNode) -> Option<SketchAlgorithm> {
    match &node.operator {
        Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => {
            sketch_kind_of(summary_input)
        }
        Operator::ASAP(ASAPOp::SummaryAgg {
            family: FieldDataType::Sketch(kind, _),
            ..
        }) => Some(kind.algorithm().clone()),
        _ => None,
    }
}

/// The grouping strategy used by a bound summary candidate, unwrapping its
/// evaluation node when necessary.
fn summary_grouping(node: &OperatorNode) -> Option<&GroupingStrategy> {
    match &node.operator {
        Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => {
            summary_grouping(summary_input)
        }
        Operator::ASAP(ASAPOp::SummaryAgg { grouping, .. }) => Some(grouping),
        _ => None,
    }
}

// ── global_selection ─────────────────────────────────────────────────────

/// Why [`global_selection`] committed an exact composition at a
/// site: which child candidate it composes with, and the
/// cost-units-per-second comparison against the raw fallback that it won.
#[derive(Debug)]
pub struct CompositionDecision<'a> {
    /// The exact child/operation pair validated by the search accuracy model.
    pub plan: Rc<OperatorNode>,
    /// The child target the composed operator consumes.
    pub child_target: &'a Rc<OperatorNode>,
    /// For a read-time operation: the child's own candidate committed alongside
    /// (the summary evaluation the operator folds). `None` for an update-path
    /// transform, whose input is raw update data — its cost is charged to
    /// the maintained summary *above* it instead.
    pub child_candidate: Option<&'a ReplacementSubDAG>,
    /// The composed plan's recurring rate — `read_operation_plan_cost_rate`
    /// or `maintenance_operation_plan_cost_rate`.
    pub cost_rate: CostRate,
    /// `raw_recompute_cost_rate` — the kept-sub-DAG baseline it beat.
    pub baseline_rate: CostRate,
    /// The statistics (and their provenance) both rates were computed from.
    pub inputs: ExactCompositionCostInputs,
}

/// [`global_selection`]'s result: the [`GlobalSelection`] it committed to,
/// plus the cost comparison behind each exact composition it chose. The
/// selection is a Stage 1 type and carries no cost data, so the comparison
/// is kept here.
#[derive(Debug)]
pub struct CostedGlobalSelection<'a> {
    selection: GlobalSelection<'a>,
    compositions: HashMap<*const OperatorNode, CompositionDecision<'a>>,
}

impl<'a> CostedGlobalSelection<'a> {
    /// The decision behind `target`'s chosen exact composition, if it chose one.
    pub fn composition(&self, target: &Rc<OperatorNode>) -> Option<&CompositionDecision<'a>> {
        self.compositions.get(&Rc::as_ptr(target))
    }
}

impl<'a> std::ops::Deref for CostedGlobalSelection<'a> {
    type Target = GlobalSelection<'a>;

    fn deref(&self) -> &Self::Target {
        &self.selection
    }
}

/// The maintained `SummaryAgg` a bound summary candidate builds (under
/// its `SummaryEstimate` evaluation, if any) — the summary an `ValueOperationAtIngestionTime`
/// beneath it feeds, for `maintenance_operation_plan_cost_rate`.
fn maintained_summary(node: &Rc<OperatorNode>) -> Option<&Rc<OperatorNode>> {
    match &node.operator {
        Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => {
            maintained_summary(summary_input)
        }
        Operator::ASAP(ASAPOp::SummaryAgg { .. }) => Some(node),
        _ => None,
    }
}

fn is_composition_candidate(candidate: &ReplacementSubDAG) -> bool {
    matches!(candidate.replacement, Replacement::ExactComposition(_))
}

/// Everything [`global_selection`] threads between sites for
/// exact compositions (issue #171): child candidates already committed by
/// an earlier parent, and the maintained summary above each site.
#[derive(Default)]
struct CompositionContext {
    /// child target ptr → the child's candidate an ancestor's composition
    /// already committed to (a later parent must compose with the *same*
    /// one, and the child's own selection is forced to it).
    committed_child: HashMap<*const OperatorNode, *const ReplacementSubDAG>,
    /// site ptr → the maintained `SummaryAgg` directly above it, when its
    /// parent chose a bound summary — what an `ValueOperationAtIngestionTime` here feeds.
    maintaining_parent: HashMap<*const OperatorNode, Rc<OperatorNode>>,
}

/// One eligible composed alternative at a site, before the cheapest wins.
struct CompositionOption<'a> {
    candidate: &'a ReplacementSubDAG,
    decision: CompositionDecision<'a>,
}

/// Every [`Replacement::ExactComposition`] candidate of `group` whose
/// composed-plan rate is *known* and beats the raw-recompute baseline —
/// costed against each compatible child candidate already in `CandidateLogicalASAPDAGs`
/// (or the one an earlier parent committed). Unknown statistics yield no
/// option at all: the conservative kept-sub-DAG path stays.
fn composition_options<'a>(
    group: &'a TargetSubDAGCandidates,
    groups: &'a HashMap<*const OperatorNode, TargetSubDAGCandidates>,
    effective: usize,
    cost_model: &dyn CostModel,
    context: &CompositionContext,
    plans: &[PreparedComposition],
) -> Vec<CompositionOption<'a>> {
    let mut options = Vec::new();
    for candidate in &group.candidates {
        let Replacement::ExactComposition(composition) = &candidate.replacement else {
            continue;
        };
        if runtime_support_evidence(candidate, cost_model) != Some(true) {
            continue;
        }
        let child_ptr = Rc::as_ptr(&composition.child_target);
        let Some(child_group) = groups.get(&child_ptr) else {
            continue;
        };
        let already_committed = context.committed_child.get(&child_ptr).copied();
        let cost = |summary: &OperatorNode, shared: bool| {
            let request = ExactCompositionCostRequest {
                target: &group.target,
                composition,
                summary,
                effective_consumer_count: effective,
            };
            let mut inputs = cost_model.exact_composition_cost_inputs(&request);
            if shared {
                // Shared state is counted once: an earlier parent already
                // pays this child's maintenance, so the marginal cost here
                // is zero — a *known* zero, unlike an unknown input.
                if let Some(maintenance) = inputs.summary_maintenance_cost_per_update.as_mut() {
                    *maintenance = 0.0;
                }
            }
            let rate = inputs.composed_plan_cost_rate(composition.placement)?;
            let baseline = raw_recompute_cost_rate(&inputs)?;
            (rate < baseline).then_some((rate, baseline, inputs))
        };
        match composition.placement {
            OperationPlacement::Read => {
                let child_candidates: Vec<&'a ReplacementSubDAG> = match already_committed {
                    // SAFETY-free: the pointer was taken from `groups`'s own
                    // candidate storage, which outlives this borrow.
                    Some(ptr) => child_group
                        .candidates
                        .iter()
                        .filter(|c| std::ptr::eq(*c, ptr))
                        .collect(),
                    None => child_group.candidates.iter().collect(),
                };
                for child_candidate in child_candidates {
                    if !is_automatically_selectable(child_candidate, cost_model) {
                        continue;
                    }
                    let Replacement::SubDAG(summary) = &child_candidate.replacement else {
                        continue;
                    };
                    if is_logical_rewrite(summary) || !composition.accepts_child(summary) {
                        continue;
                    }
                    let Some(prepared) = plans.iter().find(|p| {
                        p.target == Rc::as_ptr(&group.target)
                            && p.operation.same_as(composition)
                            && Rc::ptr_eq(&p.child, summary)
                    }) else {
                        continue;
                    };
                    let Some((rate, baseline, inputs)) = cost(summary, already_committed.is_some())
                    else {
                        continue;
                    };
                    options.push(CompositionOption {
                        candidate,
                        decision: CompositionDecision {
                            plan: Rc::clone(&prepared.plan),
                            child_target: &composition.child_target,
                            child_candidate: Some(child_candidate),
                            cost_rate: rate,
                            baseline_rate: baseline,
                            inputs,
                        },
                    });
                }
            }
            OperationPlacement::Maintenance => {
                let Some(prepared) = plans.iter().find(|p| {
                    p.target == Rc::as_ptr(&group.target) && p.operation.same_as(composition)
                }) else {
                    continue;
                };
                // An maintenance-time operation only pays off beneath a
                // maintained summary; with nothing above it, its output is
                // never read and the raw fallback is the same computation.
                let Some(parent) = context.maintaining_parent.get(&Rc::as_ptr(&group.target))
                else {
                    continue;
                };
                let Some((rate, baseline, inputs)) = cost(parent, false) else {
                    continue;
                };
                options.push(CompositionOption {
                    candidate,
                    decision: CompositionDecision {
                        plan: Rc::clone(&prepared.plan),
                        child_target: &composition.child_target,
                        child_candidate: None,
                        cost_rate: rate,
                        baseline_rate: baseline,
                        inputs,
                    },
                });
            }
        }
    }
    options
}

/// The whole-plan (cross-group) selection step the module docs'
/// "Whole-plan (cross-group) selection" section describes: one
/// [`TargetSubDAGSelection`] per discovered site, each ranked against an
/// `effective_consumer_count` that accounts for every ancestor
/// [`SharedSubDAGStrategy`] decision on the path to it — unlike
/// [`cost_sorted`], whose per-group ranking only ever sees a
/// group's own raw [`TargetSubDAGCandidates::consumer_count`].
/// Uncertified DDSketch ratios remain in [`CandidateLogicalASAPDAGs`] for downstream
/// inspection but are not chosen automatically by this selector.
pub fn global_selection<'a, Id>(
    space: &'a CandidateLogicalASAPDAGs<Id>,
    cost_model: &dyn CostModel,
) -> CostedGlobalSelection<'a> {
    global_selection_impl(space, cost_model, None, None)
        .expect("structural global selection cannot produce a recurrence error")
}

/// Recurrence-aware counterpart to [`global_selection`]. The same
/// whole-plan traversal and effective structural consumer counts are
/// retained, while every CSE share/recompute choice is made from the
/// corresponding recurrence profile.
pub fn global_selection_with_recurrence<'a, Id>(
    space: &'a CandidateLogicalASAPDAGs<Id>,
    cost_model: &dyn CostModel,
    profiles: &RecurrenceProfileMap,
    horizon: Option<Horizon>,
) -> Result<CostedGlobalSelection<'a>, RecurrenceError> {
    global_selection_impl(space, cost_model, Some(profiles), horizon)
}

fn global_selection_impl<'a, Id>(
    space: &'a CandidateLogicalASAPDAGs<Id>,
    cost_model: &dyn CostModel,
    profiles: Option<&RecurrenceProfileMap>,
    horizon: Option<Horizon>,
) -> Result<CostedGlobalSelection<'a>, RecurrenceError> {
    let dag = reference_dag(space);
    let topo = topological_order(space.order(), &dag);

    let mut effective_uses = dag.external_root_uses.clone();
    let mut chosen_share: HashMap<*const OperatorNode, ShareDecision> = HashMap::new();
    let mut groups: HashMap<*const OperatorNode, TargetSubDAGSelection<'a>> = HashMap::new();
    let mut compositions: HashMap<*const OperatorNode, CompositionDecision<'a>> = HashMap::new();
    let mut context = CompositionContext::default();

    for ptr in &topo {
        let group = &space.groups()[ptr];

        let effective = effective_uses.get(ptr).copied().unwrap_or(0);
        effective_uses.insert(*ptr, effective);

        // ── Exact compositions (issue #171) ─────────────────────────
        // A child an earlier parent's composition committed to is
        // forced to exactly that candidate — the parent/child pair is
        // one decision. Otherwise, a composition here wins only when
        // its cost-units-per-second rate is *known* and beats the raw
        // recompute baseline; missing statistics keep the conservative
        // path below.
        let mut composition_decision = None;
        let forced = context
            .committed_child
            .get(ptr)
            .and_then(|&cptr| group.candidates.iter().find(|c| std::ptr::eq(*c, cptr)));
        let composed = if forced.is_some() {
            None
        } else {
            composition_options(
                group,
                space.groups(),
                effective,
                cost_model,
                &context,
                space.composition_plans(),
            )
            .into_iter()
            .min_by(|a, b| a.decision.cost_rate.0.total_cmp(&b.decision.cost_rate.0))
        };
        if let Some(option) = &composed {
            if let Some(child_candidate) = option.decision.child_candidate {
                context.committed_child.insert(
                    Rc::as_ptr(option.decision.child_target),
                    child_candidate as *const ReplacementSubDAG,
                );
            }
            if let Replacement::ExactComposition(composition) = &option.candidate.replacement {
                if composition.placement == OperationPlacement::Maintenance {
                    // A chain of functions feeds the same summary.
                    if let Some(parent) = context.maintaining_parent.get(ptr).cloned() {
                        context
                            .maintaining_parent
                            .insert(Rc::as_ptr(&composition.child_target), parent);
                    }
                }
            }
        }

        let complete_plan_choice = (!forced.is_some()
            && composed.is_none()
            && cost_model.candidate_cost_covers_complete_plan())
        .then(|| {
            let effective_target = TargetSubDAG::with_consumer_count(&group.target, effective);
            let bound = group
                .candidates
                .iter()
                .filter(|candidate| {
                    !is_cse_candidate(candidate)
                        && !is_composition_candidate(candidate)
                        && is_automatically_selectable(candidate, cost_model)
                })
                .filter_map(|candidate| {
                    cost_model
                        .candidate_cost(candidate, &effective_target)
                        .map(|cost| (candidate, cost))
                })
                .min_by(|(_, left), (_, right)| left.0.total_cmp(&right.0))
                .map(|(candidate, _)| candidate);
            bound.or_else(|| {
                (cost_model.allow_uncosted_legacy_selection() && effective >= 2)
                    .then(|| {
                        decide_with_effective_count(group, effective, cost_model).and_then(
                            |decision| {
                                let candidate = pick_shared_sub_dag_candidate(group, decision)?;
                                chosen_share.insert(*ptr, decision);
                                Some(candidate)
                            },
                        )
                    })
                    .flatten()
            })
        })
        .flatten();

        let chosen = if let Some(forced) = forced {
            Some(forced)
        } else if let Some(option) = composed {
            composition_decision = Some(option.decision);
            Some(option.candidate)
        } else if cost_model.candidate_cost_covers_complete_plan() {
            complete_plan_choice
        } else if effective >= 2 && cse_candidate_pair(group).is_some() {
            let decision = if let Some(profiles) = profiles {
                decide_group_with_recurrence(
                    group,
                    effective,
                    profiles.for_target(&group.target),
                    horizon,
                    cost_model,
                )?
            } else {
                decide_with_effective_count(group, effective, cost_model)
            };
            match decision {
                Some(decision) => {
                    let cse = pick_shared_sub_dag_candidate(group, decision);
                    let effective_target =
                        TargetSubDAG::with_consumer_count(&group.target, effective);
                    let logical = group
                        .candidates
                        .iter()
                        .filter(|candidate| {
                            !is_cse_candidate(candidate)
                                && !is_composition_candidate(candidate)
                                && is_automatically_selectable(candidate, cost_model)
                        })
                        .filter_map(|candidate| {
                            cost_model
                                .candidate_cost(candidate, &effective_target)
                                .map(|cost| (candidate, cost))
                        })
                        .min_by(|(_, a), (_, b)| a.0.total_cmp(&b.0))
                        .map(|(candidate, _)| candidate);
                    let cse = cse.filter(|candidate| {
                        cost_model
                            .candidate_cost(candidate, &effective_target)
                            .is_some()
                            || cost_model.allow_uncosted_legacy_selection()
                    });
                    match (cse, logical) {
                        (Some(cse), Some(logical))
                            if cost_model
                                .candidate_cost(cse, &effective_target)
                                .is_none_or(|cse_cost| {
                                    cost_model
                                        .candidate_cost(logical, &effective_target)
                                        .is_some_and(|logical_cost| logical_cost.0 < cse_cost.0)
                                }) =>
                        {
                            Some(logical)
                        }
                        (cse, _) => {
                            if cse.is_some() {
                                chosen_share.insert(*ptr, decision);
                            }
                            cse
                        }
                    }
                }
                // `realize_child` couldn't produce even a logical fallback —
                // not expected in practice for a target that's already
                // part of a legitimate workload DAG (mirrors
                // `cse_preference`'s own doc on this same degrade).
                // Falling back to ordinary local ranking is still a
                // valid answer, just not a cross-group-aware one; this
                // group also contributes no Share collapse to its own
                // children (see `multiplier`'s `_ => effective` arm).
                None => rank_group(group, cost_model).into_iter().find(|candidate| {
                    !is_composition_candidate(candidate)
                        && is_automatically_selectable(candidate, cost_model)
                        && (cost_model
                            .candidate_cost(
                                candidate,
                                &TargetSubDAG::with_consumer_count(&group.target, effective),
                            )
                            .is_some()
                            || cost_model.allow_uncosted_legacy_selection())
                }),
            }
        } else {
            let effective_target = TargetSubDAG::with_consumer_count(&group.target, effective);
            rank_group(group, cost_model)
                .into_iter()
                .find(|candidate| {
                    !is_cse_candidate(candidate)
                        && !is_composition_candidate(candidate)
                        && is_automatically_selectable(candidate, cost_model)
                        && (cost_model
                            .candidate_cost(candidate, &effective_target)
                            .is_some()
                            || cost_model.allow_uncosted_legacy_selection())
                })
                .or_else(|| {
                    cse_candidate_pair(group)
                        .map(|(share, _)| share)
                        .filter(|candidate| {
                            cost_model
                                .candidate_cost(candidate, &effective_target)
                                .is_some()
                                || cost_model.allow_uncosted_legacy_selection()
                        })
                })
        };

        // Record the maintained summary this site's bound candidate
        // builds, for a child that may compose an `ValueOperationAtIngestionTime`
        // beneath it.
        if let (Some(Replacement::SubDAG(node)), Some(NonASAPOp::Aggregate { child, .. })) =
            (chosen.map(|c| &c.replacement), group.target.non_asap())
        {
            if let Some(summary) = maintained_summary(node) {
                context
                    .maintaining_parent
                    .insert(Rc::as_ptr(child), Rc::clone(summary));
            }
        }

        let outgoing_multiplier = multiplier(*ptr, &effective_uses, &chosen_share);
        match chosen {
            Some(ReplacementSubDAG {
                replacement: Replacement::SubDAG(source),
                provenance: ReplacementProvenance::AccuracyReconciliation,
                ..
            }) => {
                // Accuracy reconciliation reads another discovered memo
                // group, rather than inlining that group's children. Let
                // the source group receive the uses and propagate them
                // through its own selected realization when its turn
                // arrives in topological order.
                *effective_uses.entry(Rc::as_ptr(source)).or_insert(0) += outgoing_multiplier;
            }
            _ => {
                let selected_rewrite = match chosen.map(|candidate| &candidate.replacement) {
                    Some(Replacement::SubDAG(rewrite)) if is_logical_rewrite(rewrite) => rewrite,
                    Some(Replacement::SubDAG(_) | Replacement::ExactComposition(_)) | None => {
                        &group.target
                    }
                };
                for (child, edge_count) in direct_child_counts(selected_rewrite) {
                    *effective_uses.entry(child).or_insert(0) += edge_count * outgoing_multiplier;
                }
            }
        }

        groups.insert(
            *ptr,
            TargetSubDAGSelection {
                target: &group.target,
                consumer_count: group.consumer_count,
                effective_consumer_count: effective,
                chosen,
            },
        );
        if let Some(decision) = composition_decision {
            compositions.insert(*ptr, decision);
        }
    }

    let composition_plans = compositions
        .iter()
        .map(|(ptr, decision)| (*ptr, Rc::clone(&decision.plan)))
        .collect();
    Ok(CostedGlobalSelection {
        selection: GlobalSelection::new(space.order().to_vec(), groups, composition_plans),
        compositions,
    })
}

fn is_cse_candidate(candidate: &ReplacementSubDAG) -> bool {
    matches!(
        candidate.provenance,
        ReplacementProvenance::CseShare | ReplacementProvenance::CseRecompute
    )
}

fn is_automatically_selectable(candidate: &ReplacementSubDAG, cost_model: &dyn CostModel) -> bool {
    candidate.provenance != ReplacementProvenance::RootPhysicalRealization
        && !candidate.has_missing_accuracy_evidence()
        && runtime_support_evidence(candidate, cost_model) != Some(false)
}

/// How much one direct reference to `parent_ptr` actually costs, once
/// `parent_ptr`'s own chosen candidate (if it has a Share/Recompute pair at
/// all) is taken into account:
///
/// - `1`, if `parent_ptr` chose [`ShareDecision::Share`] — one shared
///   execution backs every reference to it, so referencing it costs no more
///   than referencing it once.
/// - `parent_ptr`'s own `effective_consumer_count` otherwise — either it
///   chose [`ShareDecision::RecomputeIndependently`] (each of its own uses
///   gets its own independent execution, so referencing it costs as much as
///   its *own* full multiplicity), or it has no Share/Recompute decision at
///   all (not a [`SharedSubDAGStrategy`] shape — nothing here collapses
///   its multiplicity to one, so whatever multiplicity *its* ancestors
///   established simply passes through).
///
/// Composing this recurrence transitively up the whole ancestor chain (not
/// just the immediate parent) is exactly what makes
/// [`global_selection`]'s `effective_consumer_count` differ from
/// [`TargetSubDAGCandidates::consumer_count`] whenever a `RecomputeIndependently`
/// ancestor sits anywhere on the path from a root to a site — see the
/// module docs' "Whole-plan (cross-group) selection" section.
fn multiplier(
    parent_ptr: *const OperatorNode,
    effective_uses: &HashMap<*const OperatorNode, usize>,
    chosen_share: &HashMap<*const OperatorNode, ShareDecision>,
) -> usize {
    let effective = *effective_uses.get(&parent_ptr).expect(
        "topological_order guarantees a parent is processed (and its effective_consumer_count \
         recorded) before any of its children",
    );
    match chosen_share.get(&parent_ptr) {
        Some(ShareDecision::Share) => 1,
        _ => effective,
    }
}

/// [`CostModel::cse_share_decision`] for `group`, against an explicit
/// `effective_consumer_count` instead of `group.consumer_count` — the
/// cross-group-aware counterpart to [`cse_preference`], which uses the raw
/// structural count. `None` only when [`realize_child`] can't produce even a
/// logical fallback for `group.target` (see that function's own doc).
fn decide_with_effective_count(
    group: &TargetSubDAGCandidates,
    effective_consumer_count: usize,
    cost_model: &dyn CostModel,
) -> Option<ShareDecision> {
    let bound = realize_child(&group.target).ok()?;
    let candidate = CseCandidate {
        sub_dag: &group.target,
        bound_summary: &bound,
        consumer_count: effective_consumer_count,
    };
    Some(cost_model.cse_share_decision(&candidate))
}

fn decide_group_with_recurrence(
    group: &TargetSubDAGCandidates,
    effective_consumer_count: usize,
    recurrence: RecurrenceProfile,
    horizon: Option<Horizon>,
    cost_model: &dyn CostModel,
) -> Result<Option<ShareDecision>, RecurrenceError> {
    let Some(bound) = realize_child(&group.target).ok() else {
        return Ok(None);
    };
    let candidate = CseCandidate {
        sub_dag: &group.target,
        bound_summary: &bound,
        consumer_count: effective_consumer_count,
    };
    Ok(Some(
        cost_model
            .cse_share_decision_with_recurrence(&candidate, &recurrence, horizon)?
            .decision,
    ))
}

/// The [`SharedSubDAGStrategy`] candidate matching `decision`: the one
/// that shares `group.target`'s own `Rc` for [`ShareDecision::Share`], the
/// freshly-allocated one for [`ShareDecision::RecomputeIndependently`] —
/// the same `Rc`-identity distinction [`is_duplicate_rewrite`]'s own doc
/// explains is the *only* signal this IR carries for that choice.
fn pick_shared_sub_dag_candidate(
    group: &TargetSubDAGCandidates,
    decision: ShareDecision,
) -> Option<&ReplacementSubDAG> {
    let (share, recompute) = cse_candidate_pair(group)?;
    Some(match decision {
        ShareDecision::Share => share,
        ShareDecision::RecomputeIndependently => recompute,
    })
}

// ── reference DAG + topological order ─────────────────────────────────

/// The parent/child structure [`global_selection`]'s DP walks —
/// built separately from `discover_targets`'s own `order`/`nodes`/`counts`
/// maps (which only track *aggregate* reference counts, not per-parent
/// breakdown or direction). Selection needs per-parent edge counts to
/// distinguish shared producers from repeated uses within one consumer.
struct ReferenceDAG {
    /// child ptr -> `(parent ptr, edge count from that one parent)`, for
    /// every direct operator-child edge in the relational-skeleton scope
    /// [`walk_children`] itself uses (an edge count above 1 happens when
    /// one parent references the same child from two different fields,
    /// e.g. a `Join`'s `left`/`right` both being the same `Rc`).
    parents_of: HashMap<*const OperatorNode, Vec<(*const OperatorNode, usize)>>,
    /// parent ptr -> every distinct child ptr it directly references — the
    /// reverse of `parents_of`, for [`topological_order`]'s Kahn's-algorithm
    /// traversal.
    children_of: HashMap<*const OperatorNode, Vec<*const OperatorNode>>,
    /// How many of the workload's own `roots` point directly at each node —
    /// a node's "external" use. Nothing inside the DAG decides this (it
    /// isn't a reference from another discovered site), so it's never
    /// subject to any ancestor's Share/Recompute choice — it's the base
    /// case [`global_selection`]'s recurrence starts from.
    external_root_uses: HashMap<*const OperatorNode, usize>,
}

/// Build an ordering DAG containing every edge that could be selected:
/// the original target's edges plus every rewrite candidate's edges. An
/// accuracy-reconciliation rewrite points at another discovered memo group,
/// so it contributes an edge to that group itself; other rewrites contribute
/// their relational children as before. The
/// DAG is deliberately only used for topological ordering; effective-use
/// counts are propagated through the one candidate actually selected.
fn reference_dag<Id>(space: &CandidateLogicalASAPDAGs<Id>) -> ReferenceDAG {
    let mut dag = ReferenceDAG {
        parents_of: HashMap::new(),
        children_of: HashMap::new(),
        external_root_uses: HashMap::new(),
    };
    for (_, root) in &space.roots {
        *dag.external_root_uses.entry(Rc::as_ptr(root)).or_insert(0) += 1;
    }
    for ptr in space.order() {
        let group = &space.groups()[ptr];
        record_possible_edges(*ptr, &group.target, &mut dag);
        for candidate in &group.candidates {
            if let Replacement::SubDAG(rewrite) = &candidate.replacement {
                if !is_logical_rewrite(rewrite) {
                    continue;
                }
                if candidate.provenance == ReplacementProvenance::AccuracyReconciliation {
                    add_edge(*ptr, Rc::as_ptr(rewrite), 1, &mut dag);
                } else {
                    record_possible_edges(*ptr, rewrite, &mut dag);
                }
            }
        }
    }
    dag
}

/// Record one `parent_ptr -> child` edge (both directions — see
/// [`ReferenceDAG`]'s fields), retaining the greatest multiplicity seen
/// when the target and alternative rewrites expose the same edge.
fn add_edge(
    parent_ptr: *const OperatorNode,
    child_ptr: *const OperatorNode,
    edge_count: usize,
    dag: &mut ReferenceDAG,
) {
    let siblings = dag.parents_of.entry(child_ptr).or_default();
    match siblings.iter_mut().find(|(p, _)| *p == parent_ptr) {
        Some((_, count)) => *count = (*count).max(edge_count),
        None => siblings.push((parent_ptr, edge_count)),
    }
    let kids = dag.children_of.entry(parent_ptr).or_default();
    if !kids.contains(&child_ptr) {
        kids.push(child_ptr);
    }
}

fn record_possible_edges(
    parent_ptr: *const OperatorNode,
    node: &OperatorNode,
    dag: &mut ReferenceDAG,
) {
    for (child_ptr, edge_count) in direct_child_counts(node) {
        add_edge(parent_ptr, child_ptr, edge_count, dag);
    }
}

/// A topological order over `order` (parent before every child) via Kahn's
/// algorithm on `dag`'s reverse adjacency — needed because
/// `discover_targets`'s own `order` is only a valid *discovery* order
/// (first-seen-first), not a valid topological one: a node reached via two
/// different root paths can have a parent that's discovered *after* it (see
/// this function's own test for a worked diamond example), which is exactly
/// backwards for [`global_selection`]'s recurrence.
fn topological_order(
    order: &[*const OperatorNode],
    dag: &ReferenceDAG,
) -> Vec<*const OperatorNode> {
    let mut in_degree: HashMap<*const OperatorNode, usize> = HashMap::new();
    for ptr in order {
        let degree = dag.parents_of.get(ptr).map(Vec::len).unwrap_or(0);
        in_degree.insert(*ptr, degree);
    }

    let mut queue: VecDeque<*const OperatorNode> = order
        .iter()
        .copied()
        .filter(|ptr| in_degree[ptr] == 0)
        .collect();

    let mut topo = Vec::with_capacity(order.len());
    while let Some(ptr) = queue.pop_front() {
        topo.push(ptr);
        if let Some(children) = dag.children_of.get(&ptr) {
            for child in children {
                if let Some(degree) = in_degree.get_mut(child) {
                    *degree -= 1;
                    if *degree == 0 {
                        queue.push_back(*child);
                    }
                }
            }
        }
    }

    assert_eq!(
        topo.len(),
        order.len(),
        "topological_order: the discovered-site reference dag has a cycle — every \
         OperatorNode is built from Rc children, which can't form one, so this indicates a bug \
         in reference_dag rather than a real cyclic workload",
    );
    topo
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::cost_model::{Cost, DefaultCostModel};
    use crate::test_support::{agg, lower_promql, metric_scan};
    use asap_logical_optimizer::accuracy::{
        AccuracyModel, DefaultAccuracyModel, EqualSplitAllocator, PropagationStats,
    };
    use asap_logical_optimizer::pass1::replacement::{
        default_strategies, search_workload, search_workload_with, search_workload_with_targets,
        ASAPStrategies, ReplacementStrategy,
    };
    use asap_logical_optimizer::pass2::reconciliation::AccuracyReconciliationStrategy;
    use asap_types::ir::operator::agg_intent::{default_cardinality, default_quantile, AggIntent};
    use asap_types::ir::operator::operator_properties::Reduction;
    use asap_types::ir::properties::{
        AccuracyError, CompositionOperator, ErrorMetric, ResultGuarantee,
    };
    use asap_types::ir::schema::ColumnId;
    use asap_types::ir::schema::SketchStatistic;
    use asap_types::ir::{Predicate, ScalarExpr};
    use asap_types::types::AccuracyTarget;

    fn equi_pred(left: ColumnId, right: ColumnId) -> Predicate {
        Predicate(ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(left)),
            op: asap_types::ir::scalar::CompareOpKind::Eq,
            right: Box::new(ScalarExpr::Column(right)),
            semantics: asap_types::ir::ExprSemantics::Sql,
        })
    }

    fn quantile_eps_intent(q: f64, e: f64) -> AggIntent {
        AggIntent::Quantile {
            col: None,
            q,
            accuracy: AccuracyTarget::Epsilon(e),
        }
    }

    #[test]
    fn relational_join_is_exact_only_when_both_inputs_are_exact() {
        // `relational_join_guarantee` folded into assembly's generic
        // "keep the operator, assemble its children" branch: an assembled
        // inner equi-`Join` is exact exactly when both assembled inputs are.
        let join = |left_intent: AggIntent, right_intent: AggIntent| {
            let left = agg(vec![2], left_intent, metric_scan(&["job"]));
            let right = agg(
                vec![2],
                right_intent,
                crate::test_support::scan("n", metric_scan(&["job"]).schema.clone()),
            );
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Join {
                kind: asap_types::ir::operator::operator_properties::JoinKind::Inner,
                pred: equi_pred(0, 2),
                left,
                right,
            }))
            .unwrap()
        };
        let is_exact = |node: &OperatorNode| {
            node.guarantee
                .as_ref()
                .is_some_and(ResultGuarantee::is_exact)
        };
        for (root, both_exact_expected) in [
            (
                join(AggIntent::Sum { col: None }, AggIntent::Sum { col: None }),
                true,
            ),
            (
                join(AggIntent::Sum { col: None }, quantile_eps_intent(0.5, 0.05)),
                false,
            ),
        ] {
            let space = search_workload(vec![(0usize, Rc::clone(&root))]);
            let assembled = global_selection(&space, &DefaultCostModel)
                .assemble_selected_query(&space.roots[0].1)
                .unwrap()
                .unwrap();
            let Some(NonASAPOp::Join { left, right, .. }) = assembled.non_asap() else {
                panic!("the join is kept and its inputs assembled: {assembled:?}");
            };
            assert_eq!(
                is_exact(&assembled),
                is_exact(left) && is_exact(right),
                "join guarantee must be exact iff both inputs are exact"
            );
            if both_exact_expected {
                assert!(is_exact(&assembled), "exact inputs give an exact join");
            }
        }
    }

    // ── cost-based ranking ───────────────────────────────────────────────

    #[test]
    fn cost_sorted_orders_shared_sub_dag_candidates_by_cse_share_decision() {
        // Many consumers of a cheap-to-recompute, cheap-to-maintain exact
        // accumulator: cse_share_decision should prefer Share (see
        // cost_model.rs's own `cse_share_decision_shares_when_recompute_dominates_maintenance`).
        let mut roots = Vec::new();
        let shared = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        for i in 0..20 {
            roots.push((i, Rc::new((*shared).clone())));
        }
        let space = search_workload(roots);
        let group = space.candidates_for_target(&space.roots[0].1).unwrap();
        assert_eq!(group.consumer_count, 20);

        let ranked = cost_sorted(&space, &DefaultCostModel);
        let ranked_group = ranked
            .iter()
            .find(|g| Rc::ptr_eq(g.target, &space.roots[0].1))
            .unwrap();
        assert!(matches!(
            &ranked_group.candidates[0].replacement,
            Replacement::SubDAG(rc) if Rc::ptr_eq(rc, &group.target)
        ));
        let rewrites: Vec<&ReplacementSubDAG> = ranked_group
            .candidates
            .iter()
            .filter(|c| matches!(&c.replacement, Replacement::SubDAG(n) if !n.contains_asap()))
            .copied()
            .collect();
        assert_eq!(rewrites.len(), 2);
        let first_shares_target = match &rewrites[0].replacement {
            Replacement::SubDAG(rc) => Rc::ptr_eq(rc, &group.target),
            Replacement::ExactComposition(_) => false,
        };
        assert!(
            first_shares_target,
            "with 20 cheap consumers, Share should rank first: {rewrites:?}"
        );
    }

    #[test]
    fn cost_sorted_orders_sketch_candidates_by_rank_candidates() {
        struct PreferDDSketch;
        impl CostModel for PreferDDSketch {
            fn rank_candidates(
                &self,
                _intent: &AggIntent,
                candidates: &[SketchAlgorithm],
            ) -> Vec<SketchAlgorithm> {
                let mut v = candidates.to_vec();
                if let Some(pos) = v.iter().position(|k| *k == SketchAlgorithm::DDSketch) {
                    let dd = v.remove(pos);
                    v.insert(0, dd);
                }
                v
            }
        }

        let root = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let space = search_workload(vec![("q", root)]);
        let ranked = cost_sorted(&space, &PreferDDSketch);
        let agg_group = ranked
            .iter()
            .find(|g| matches!(g.target.non_asap(), Some(NonASAPOp::Aggregate { .. })))
            .unwrap();
        assert_eq!(agg_group.candidates.len(), 2);
        let first_kind = match &agg_group.candidates[0].replacement {
            Replacement::SubDAG(node) => sketch_kind_of(node),
            Replacement::ExactComposition(_) => None,
        };
        assert_eq!(first_kind, Some(SketchAlgorithm::DDSketch));
    }

    #[test]
    fn grouping_cost_cannot_resurrect_unprovable_hydra_candidates() {
        struct EstimatedSubpopulations(usize);

        impl CostModel for EstimatedSubpopulations {
            fn rank_candidates(
                &self,
                _intent: &AggIntent,
                candidates: &[SketchAlgorithm],
            ) -> Vec<SketchAlgorithm> {
                candidates.to_vec()
            }

            fn estimated_subpopulation_count(&self, _target: &OperatorNode) -> Option<usize> {
                Some(self.0)
            }
        }

        fn first_grouping(estimated_count: usize) -> GroupingStrategy {
            let model = EstimatedSubpopulations(estimated_count);
            let intent = AggIntent::Count {
                accuracy: AccuracyTarget::EpsilonDelta {
                    epsilon: 0.01,
                    delta: 0.01,
                },
            };
            let root = agg(vec![2, 3], intent, metric_scan(&["tenant_id", "endpoint"]));
            let space = search_workload(vec![("tenant_endpoint_count", root)]);
            let ranked = cost_sorted(&space, &model);
            let aggregate = ranked
                .iter()
                .find(|group| matches!(group.target.non_asap(), Some(NonASAPOp::Aggregate { .. })))
                .expect("aggregate group");
            let Replacement::SubDAG(node) = &aggregate.candidates[0].replacement else {
                panic!("grouping candidate must be a summary")
            };
            summary_grouping(node)
                .expect("bound summary grouping")
                .clone()
        }

        assert_eq!(
            first_grouping(10_000),
            GroupingStrategy::PerSubpopulationInstance
        );
        assert_eq!(
            first_grouping(10),
            GroupingStrategy::PerSubpopulationInstance
        );
    }

    /// [`RankedTargetSubDAGCandidates::costs`] is a per-candidate annotation, aligned
    /// index-for-index with `candidates` — each entry must equal what
    /// calling [`CostModel::estimate_cost`] directly on that same candidate
    /// and target produces, not some other (or stale) number.
    #[test]
    fn cost_sorted_pairs_each_candidate_with_its_own_estimate_cost() {
        let root = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let space = search_workload(vec![("q", root)]);
        let ranked = cost_sorted(&space, &DefaultCostModel);
        let agg_group = ranked
            .iter()
            .find(|g| matches!(g.target.non_asap(), Some(NonASAPOp::Aggregate { .. })))
            .unwrap();
        assert_eq!(
            agg_group.costs.len(),
            agg_group.candidates.len(),
            "costs must be aligned 1:1 with candidates"
        );
        assert!(!agg_group.costs.is_empty());

        let target = TargetSubDAG::with_consumer_count(agg_group.target, agg_group.consumer_count);
        for (candidate, &cost) in agg_group.candidates.iter().zip(&agg_group.costs) {
            assert_eq!(
                cost,
                DefaultCostModel.estimate_cost(candidate, &target),
                "RankedTargetSubDAGCandidates::costs must match calling CostModel::estimate_cost directly \
                 for the same candidate/target"
            );
        }
    }

    // ── global_selection (issue #271) ───────────────────────────────────

    /// A `CostModel` with a constant, `sub-DAG`-independent recompute cost
    /// and shared-maintenance cost, chosen (40 recompute-per-use, 100
    /// maintenance) so that a `SharedSubDAGStrategy` group's
    /// `cse_share_decision` flips exactly between a consumer count of 2
    /// (recompute total 80, below maintenance: `RecomputeIndependently`)
    /// and a consumer count of 3 (recompute total 120, above
    /// maintenance: `Share`) — the precise threshold
    /// `effective_consumer_count_corrects_a_nested_groups_share_decision`
    /// needs to cross.
    struct ConstantCseCost;
    impl CostModel for ConstantCseCost {
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
        fn cse_recompute_cost(&self, _candidate: &CseCandidate) -> Cost {
            Cost(40.0)
        }
        fn cse_shared_maintenance_cost(&self, _candidate: &CseCandidate) -> Cost {
            Cost(100.0)
        }
    }

    /// A costed logical choice must not panic when an explicitly allowed CSE
    /// choice has no numeric cost.
    #[test]
    fn costed_logical_candidate_beats_uncosted_legacy_cse_choice() {
        struct MixedCost;
        impl CostModel for MixedCost {
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

            fn candidate_cost(
                &self,
                candidate: &ReplacementSubDAG,
                _target: &TargetSubDAG<'_>,
            ) -> Option<Cost> {
                (!is_cse_candidate(candidate)).then_some(Cost(1.0))
            }
        }

        let aggregate = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let space = search_workload(vec![("left", Rc::clone(&aggregate)), ("right", aggregate)]);
        let root = &space.roots[0].1;
        assert!(cse_candidate_pair(space.candidates_for_target(root).unwrap()).is_some());
        let selected = global_selection(&space, &MixedCost);
        let chosen = selected.for_target(root).unwrap().chosen.unwrap();
        assert!(!is_cse_candidate(chosen));
    }

    #[test]
    fn global_selection_matches_cost_sorted_for_a_non_interacting_workload() {
        // No nested sharing at all — global_selection's effective_consumer_count
        // must equal the group's own raw consumer_count, and its `chosen`
        // candidate must be cost_sorted's top pick, for both the sketch
        // group and its child Scan.
        let root = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let space = search_workload(vec![("q", root)]);

        let ranked = cost_sorted(&space, &DefaultCostModel);
        let selected = global_selection(&space, &DefaultCostModel);
        assert_eq!(ranked.len(), selected.target_selections().count());

        for ranked_group in &ranked {
            let selected_group = selected.for_target(ranked_group.target).unwrap();
            assert_eq!(
                selected_group.effective_consumer_count, ranked_group.consumer_count,
                "no ancestor is ever RecomputeIndependently here, so effective must equal raw"
            );
            assert_eq!(
                selected_group.chosen.map(|c| &c.rationale),
                ranked_group.candidates.first().map(|c| &c.rationale),
                "with no cross-group interaction, global_selection's pick must match \
                 cost_sorted's top-ranked candidate"
            );
        }
    }

    #[test]
    fn global_selection_leaves_an_unmatched_group_as_none() {
        // A bare Scan: no registered strategy has an opinion on it, so it
        // gets a group with an empty candidate list (see TargetSubDAGCandidates's own
        // doc) — global_selection must not invent a candidate for it.
        let root = metric_scan(&["job"]);
        let space = search_workload(vec![("q", root)]);
        let selected = global_selection(&space, &DefaultCostModel);
        let scan_group = selected
            .target_selections()
            .find(|g| matches!(g.target.non_asap(), Some(NonASAPOp::Scan { .. })))
            .unwrap();
        assert!(scan_group.chosen.is_none());
        assert_eq!(scan_group.effective_consumer_count, 1);
    }

    #[test]
    fn global_selection_falls_back_to_local_ranking_for_sketch_family_groups() {
        // ASAPStrategies groups have no cross-group-aware cost hook
        // (rank_candidates takes no consumer_count) — global_selection must
        // still return cost_sorted's own top pick for them (documented in
        // the module docs' "Whole-plan (cross-group) selection" section),
        // not silently drop the candidate or fall back to discovery order.
        struct PreferDDSketch;
        impl CostModel for PreferDDSketch {
            fn allow_uncosted_legacy_selection(&self) -> bool {
                true
            }

            fn rank_candidates(
                &self,
                _intent: &AggIntent,
                candidates: &[SketchAlgorithm],
            ) -> Vec<SketchAlgorithm> {
                let mut v = candidates.to_vec();
                if let Some(pos) = v.iter().position(|k| *k == SketchAlgorithm::DDSketch) {
                    let dd = v.remove(pos);
                    v.insert(0, dd);
                }
                v
            }
        }

        let root = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let space = search_workload(vec![("q", root)]);
        let selected = global_selection(&space, &PreferDDSketch);
        let agg_group = selected
            .target_selections()
            .find(|g| matches!(g.target.non_asap(), Some(NonASAPOp::Aggregate { .. })))
            .unwrap();
        let kind = match &agg_group.chosen.unwrap().replacement {
            Replacement::SubDAG(node) => sketch_kind_of(node),
            Replacement::ExactComposition(_) => None,
        };
        assert_eq!(kind, Some(SketchAlgorithm::DDSketch));

        struct Uncosted;
        impl CostModel for Uncosted {
            fn rank_candidates(
                &self,
                _intent: &AggIntent,
                candidates: &[SketchAlgorithm],
            ) -> Vec<SketchAlgorithm> {
                candidates.to_vec()
            }
        }
        assert!(global_selection(&space, &Uncosted)
            .for_target(&space.roots[0].1)
            .unwrap()
            .chosen
            .is_none());
    }

    #[test]
    fn mixed_rewrite_group_keeps_and_selects_its_explicit_cse_pair() {
        let target = metric_scan(&["job"]);
        let group = TargetSubDAGCandidates {
            target: Rc::clone(&target),
            consumer_count: 2,
            rejected: Vec::new(),
            candidates: vec![
                ReplacementSubDAG {
                    strategy: "TestStrategy",
                    replacement: Replacement::SubDAG(Rc::clone(&target)),
                    provenance: ReplacementProvenance::CseShare,
                    rationale: "share".into(),
                },
                ReplacementSubDAG {
                    strategy: "TestStrategy",
                    replacement: Replacement::SubDAG(Rc::new(target.as_ref().clone())),
                    provenance: ReplacementProvenance::CseRecompute,
                    rationale: "recompute".into(),
                },
                ReplacementSubDAG {
                    strategy: "TestStrategy",
                    replacement: Replacement::SubDAG(
                        OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(
                            NonASAPOp::PromqlVectorFromScalar(ScalarExpr::EvalTimestamp),
                        ))
                        .unwrap(),
                    ),
                    provenance: ReplacementProvenance::LogicalRewrite,
                    rationale: "different rewrite strategy".into(),
                },
            ],
        };

        assert!(cse_candidate_pair(&group).is_some());
        let ranked = rank_group(&group, &ConstantCseCost);
        assert_eq!(
            ranked
                .iter()
                .map(|c| c.rationale.as_str())
                .collect::<Vec<_>>(),
            vec!["recompute", "different rewrite strategy", "share"],
            "the preferred CSE choice must be ranked without losing the unrelated rewrite"
        );
        let chosen = pick_shared_sub_dag_candidate(
            &group,
            decide_with_effective_count(&group, 2, &ConstantCseCost).unwrap(),
        )
        .unwrap();
        assert_eq!(chosen.provenance, ReplacementProvenance::CseRecompute);
    }

    #[test]
    fn effective_consumer_count_corrects_a_nested_groups_share_decision() {
        // The interaction issue #271 describes: an outer shared sub-DAG `a`
        // (referenced by 2 roots, so consumer_count == 2) wraps an inner
        // shared sub-DAG `c` (referenced once through `a`'s own child edge,
        // plus once more directly by a third, separate root — so `c`'s own
        // *raw* structural consumer_count is also 2, independent of `a`).
        //
        //   root1 ─┐
        //          ├─▶ a = Filter(child = c) ─▶ c = Dedup(job)
        //   root2 ─┘
        //   root3 ───────────────────────────▶ c  (same shared Rc)
        //
        // `a` and `c` are both non-`Aggregate` nodes (`Filter`/`Dedup`) so
        // neither is bindable — each group is a *clean* two-candidate
        // SharedSubDAGStrategy share-vs-recompute pair, with no
        // ASAPStrategies `Summary` candidate mixed in to complicate
        // ranking (see `shared_aggregate_across_two_roots_gets_both_strategies_candidates`
        // for what a *mixed*-shape group looks like — deliberately avoided
        // here to isolate the SharedSubDAGStrategy-only interaction).
        //
        // Under ConstantCseCost, consumer_count == 2 loses to maintenance
        // (2 * 40 = 80 < 100 ⇒ RecomputeIndependently); consumer_count == 3 wins
        // (3 * 40 = 120 > 100 ⇒ Share). `cost_sorted` only ever sees `c`'s raw
        // count (2) and picks RecomputeIndependently for it — the WRONG
        // answer once `a` itself is accounted for: `a`'s own decision is
        // also RecomputeIndependently (same 80-vs-100 threshold), so `a`
        // actually runs twice, and each run recomputes `c` once more —
        // `c`'s *true* effective count is 2 (via `a`) + 1 (via root3) = 3,
        // which flips its own decision to Share. Only global_selection,
        // which folds `a`'s decision into `c`'s effective_consumer_count
        // before deciding `c`, gets this right.
        use asap_types::ir::scalar::ScalarValue;
        use asap_types::ir::Predicate;

        let c = || {
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Dedup {
                cols: vec![0],
                child: metric_scan(&["job"]),
            }))
            .unwrap()
        };
        let a = || {
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Filter {
                pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
                child: c(),
            }))
            .unwrap()
        };

        let space = search_workload(vec![("root1", a()), ("root2", a()), ("root3", c())]);

        // Fixture sanity: root1/root2 merged onto one shared `a`, and `c`
        // (root1/root2's shared child, and root3 itself) merged onto one
        // shared `c` with raw consumer_count 2, and both groups are clean
        // (non-mixed) two-candidate SharedSubDAGStrategy pairs.
        assert!(Rc::ptr_eq(&space.roots[0].1, &space.roots[1].1));
        let a_rc = &space.roots[0].1;
        let Some(NonASAPOp::Filter { child: c_via_a, .. }) = a_rc.non_asap() else {
            panic!("expected root1/root2 to still be a Filter");
        };
        assert!(Rc::ptr_eq(c_via_a, &space.roots[2].1));
        let a_group = space.candidates_for_target(a_rc).unwrap();
        let c_group = space.candidates_for_target(c_via_a).unwrap();
        assert_eq!(
            a_group.consumer_count, 2,
            "fixture sanity: a has 2 consumers"
        );
        assert_eq!(
            c_group.consumer_count, 2,
            "fixture sanity: c has 2 raw consumers (via a's child edge, and via root3)"
        );
        assert_eq!(
            a_group.candidates.len(),
            2,
            "fixture sanity: a is a clean Rewrite pair"
        );
        assert_eq!(
            c_group.candidates.len(),
            2,
            "fixture sanity: c is a clean Rewrite pair"
        );

        // The naive/local answer: cost_sorted ranks c using its raw count
        // (2) alone and prefers RecomputeIndependently.
        let ranked = cost_sorted(&space, &ConstantCseCost);
        let c_ranked = ranked
            .iter()
            .find(|g| Rc::ptr_eq(g.target, c_via_a))
            .unwrap();
        let c_top_shares = matches!(
            &c_ranked.candidates[0].replacement,
            Replacement::SubDAG(rc) if Rc::ptr_eq(rc, c_via_a)
        );
        assert!(
            !c_top_shares,
            "cost_sorted, blind to a's own decision, must (wrongly) prefer \
             RecomputeIndependently for c using its raw consumer_count of 2"
        );

        // The corrected, cross-group-aware answer: global_selection folds
        // a's own RecomputeIndependently choice into c's effective count
        // (2 from a + 1 from root3 = 3) and flips to Share.
        let selected = global_selection(&space, &ConstantCseCost);
        let a_selected = selected.for_target(a_rc).unwrap();
        let c_selected = selected.for_target(c_via_a).unwrap();

        assert_eq!(
            a_selected.effective_consumer_count, 2,
            "a has no interacting ancestor"
        );
        let a_shares = matches!(
            &a_selected.chosen.unwrap().replacement,
            Replacement::SubDAG(rc) if Rc::ptr_eq(rc, a_rc)
        );
        assert!(
            !a_shares,
            "fixture sanity: a itself must also choose RecomputeIndependently"
        );

        assert_eq!(
            c_selected.effective_consumer_count, 3,
            "c's effective count must be 2 (a, itself recomputed twice) + 1 (root3)"
        );
        let c_shares = matches!(
            &c_selected.chosen.unwrap().replacement,
            Replacement::SubDAG(rc) if Rc::ptr_eq(rc, c_via_a)
        );
        assert!(
            c_shares,
            "global_selection must flip c to Share once a's own recomputation is accounted for"
        );
    }

    #[test]
    fn complete_plan_costs_reject_unbound_cse_arms() {
        struct CompletePlanCost;
        impl CostModel for CompletePlanCost {
            fn candidate_cost_covers_complete_plan(&self) -> bool {
                true
            }

            fn candidate_cost(
                &self,
                candidate: &ReplacementSubDAG,
                _target: &TargetSubDAG<'_>,
            ) -> Option<Cost> {
                assert!(!is_cse_candidate(candidate));
                None
            }

            fn rank_candidates(
                &self,
                _intent: &AggIntent,
                candidates: &[SketchAlgorithm],
            ) -> Vec<SketchAlgorithm> {
                candidates.to_vec()
            }

            fn cse_share_decision(&self, _candidate: &CseCandidate) -> ShareDecision {
                ShareDecision::RecomputeIndependently
            }
        }

        let shared =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Dedup {
                cols: vec![0],
                child: metric_scan(&["job"]),
            }))
            .unwrap();
        let space = search_workload(vec![
            ("left", Rc::clone(&shared)),
            ("right", Rc::clone(&shared)),
        ]);
        let planned = &space.roots[0].1;

        let selected = global_selection(&space, &CompletePlanCost);
        assert!(selected.for_target(planned).unwrap().chosen.is_none());
    }

    #[test]
    fn effective_repetition_materializes_a_cse_choice_for_a_single_edge_child() {
        use asap_types::ir::scalar::ScalarValue;
        use asap_types::ir::Predicate;

        let c = || {
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Dedup {
                cols: vec![0],
                child: metric_scan(&["job"]),
            }))
            .unwrap()
        };
        let a = || {
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Filter {
                pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
                child: c(),
            }))
            .unwrap()
        };
        let space = search_workload(vec![("root1", a()), ("root2", a())]);
        let a_rc = &space.roots[0].1;
        let Some(NonASAPOp::Filter { child: c_rc, .. }) = a_rc.non_asap() else {
            panic!("expected Filter root");
        };

        assert_eq!(space.candidates_for_target(c_rc).unwrap().consumer_count, 1);
        assert!(cse_candidate_pair(space.candidates_for_target(c_rc).unwrap()).is_some());

        let selected = global_selection(&space, &ConstantCseCost);
        let child = selected.for_target(c_rc).unwrap();
        assert_eq!(child.effective_consumer_count, 2);
        assert!(child.chosen.is_some());
    }

    #[test]
    fn shared_ancestor_keeps_a_single_use_cse_descendant_selected() {
        use asap_types::ir::scalar::ScalarValue;
        use asap_types::ir::Predicate;

        struct AlwaysShare;
        impl CostModel for AlwaysShare {
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

            fn cse_share_decision(&self, _candidate: &CseCandidate) -> ShareDecision {
                ShareDecision::Share
            }
        }

        let child = || {
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Dedup {
                cols: vec![0],
                child: metric_scan(&["job"]),
            }))
            .unwrap()
        };
        let parent = || {
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Filter {
                pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
                child: child(),
            }))
            .unwrap()
        };
        let space = search_workload(vec![("root1", parent()), ("root2", parent())]);
        let parent_rc = &space.roots[0].1;
        let Some(NonASAPOp::Filter {
            child: child_rc, ..
        }) = parent_rc.non_asap()
        else {
            panic!("expected Filter root");
        };

        let selected = global_selection(&space, &AlwaysShare);
        assert_eq!(
            selected
                .for_target(parent_rc)
                .unwrap()
                .effective_consumer_count,
            2
        );
        let child_selection = selected.for_target(child_rc).unwrap();
        assert_eq!(child_selection.effective_consumer_count, 1);
        assert_eq!(
            child_selection.chosen.map(|candidate| candidate.provenance),
            Some(ReplacementProvenance::CseShare),
            "a descendant collapsed to one execution still needs a selected plan"
        );
    }

    #[test]
    fn global_selection_propagates_uses_through_the_selected_rewrite() {
        use asap_types::ir::scalar::ScalarValue;
        use asap_types::ir::Predicate;

        struct ReplaceFilterChild;
        impl ReplacementStrategy for ReplaceFilterChild {
            fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
                matches!(target.root.non_asap(), Some(NonASAPOp::Filter { .. }))
            }

            fn replacements(&self, _target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
                vec![ReplacementSubDAG {
                    strategy: "ReplaceFilterChild",
                    replacement: Replacement::SubDAG(
                        OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(
                            NonASAPOp::Dedup {
                                cols: vec![0],
                                child: metric_scan(&["replacement"]),
                            },
                        ))
                        .unwrap(),
                    ),
                    provenance: ReplacementProvenance::LogicalRewrite,
                    rationale: "replace the Filter and its input".into(),
                }]
            }
        }

        let original_child = metric_scan(&["original"]);
        let root = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
            child: Rc::clone(&original_child),
        }))
        .unwrap();
        let strategies: Vec<Box<dyn ReplacementStrategy>> = vec![Box::new(ReplaceFilterChild)];
        let space = search_workload_with(vec![("q", root)], &strategies);
        let root = &space.roots[0].1;
        let selected = global_selection(&space, &DefaultCostModel);
        let Replacement::SubDAG(rewrite) = &selected
            .for_target(root)
            .unwrap()
            .chosen
            .unwrap()
            .replacement
        else {
            panic!("expected logical rewrite");
        };
        let Some(NonASAPOp::Dedup {
            child: replacement_child,
            ..
        }) = rewrite.non_asap()
        else {
            panic!("expected Dedup rewrite");
        };
        let Some(NonASAPOp::Filter {
            child: original_child,
            ..
        }) = root.non_asap()
        else {
            panic!("expected Filter root");
        };

        assert_eq!(
            selected
                .for_target(original_child)
                .unwrap()
                .effective_consumer_count,
            0
        );
        assert_eq!(
            selected
                .for_target(replacement_child)
                .unwrap()
                .effective_consumer_count,
            1
        );
    }

    // A cheap but physically infeasible candidate must not be selected.
    #[test]
    fn explicit_summary_infeasibility_prevents_selection() {
        struct Unsupported;
        impl CostModel for Unsupported {
            fn rank_candidates(
                &self,
                _: &AggIntent,
                candidates: &[SketchAlgorithm],
            ) -> Vec<SketchAlgorithm> {
                candidates.to_vec()
            }
            fn candidate_cost(&self, _: &ReplacementSubDAG, _: &TargetSubDAG<'_>) -> Option<Cost> {
                Some(Cost(1.0))
            }
            fn summary_support_evidence(&self, _: &OperatorNode) -> Option<bool> {
                Some(false)
            }
        }
        let root = lower_promql("sum_over_time(a[1m])", AccuracyTarget::Exact);
        let space = search_workload(vec![("q", root)]);
        let selected = global_selection(&space, &Unsupported);
        assert!(selected
            .for_target(&space.roots[0].1)
            .unwrap()
            .chosen
            .is_none());
    }

    // Composable temporal/grouped Sum must be executable as one producer.
    #[test]
    fn grouped_temporal_sum_has_one_summary_producer_candidate() {
        let root = lower_promql("sum by(job)(sum_over_time(a[1m]))", AccuracyTarget::Exact);
        let candidates = ASAPStrategies::default().replacements(&TargetSubDAG::new(&root));
        assert!(candidates
            .iter()
            .any(|candidate| matches!(&candidate.replacement,
            Replacement::SubDAG(node) if matches!(&node.operator,
                Operator::ASAP(ASAPOp::SummaryAgg { reduction: Reduction::Reduce(_), child, .. })
                    if !child.contains_asap()))));
        struct PreferComposed;
        impl CostModel for PreferComposed {
            fn rank_candidates(
                &self,
                _: &AggIntent,
                candidates: &[SketchAlgorithm],
            ) -> Vec<SketchAlgorithm> {
                candidates.to_vec()
            }
            fn candidate_cost(
                &self,
                candidate: &ReplacementSubDAG,
                _: &TargetSubDAG<'_>,
            ) -> Option<Cost> {
                Some(Cost(
                    if matches!(&candidate.replacement,
                    Replacement::SubDAG(node) if matches!(&node.operator,
                        Operator::ASAP(ASAPOp::SummaryAgg { reduction: Reduction::Reduce(_), child, .. })
                            if !child.contains_asap()))
                    {
                        1.0
                    } else {
                        100.0
                    },
                ))
            }
        }
        let space = search_workload(vec![("q", root.clone())]);
        let selected = global_selection(&space, &PreferComposed);
        let node = selected
            .assemble_selected_dag(&space.roots[0].1)
            .unwrap()
            .unwrap();
        assert!(matches!(&node.operator,
            Operator::ASAP(ASAPOp::SummaryAgg { reduction: Reduction::Reduce(_), child, .. })
                if !child.contains_asap()));
    }

    // Mixed candidate ranking must honor explicit costs, not legacy estimates.
    #[test]
    fn mixed_candidate_ranking_uses_explicit_candidate_costs() {
        struct ExplicitCosts;
        impl CostModel for ExplicitCosts {
            fn rank_candidates(
                &self,
                _: &AggIntent,
                candidates: &[SketchAlgorithm],
            ) -> Vec<SketchAlgorithm> {
                candidates.to_vec()
            }
            fn candidate_cost(
                &self,
                candidate: &ReplacementSubDAG,
                _: &TargetSubDAG<'_>,
            ) -> Option<Cost> {
                Some(Cost(
                    if candidate.provenance == ReplacementProvenance::LogicalRewrite {
                        1.0
                    } else {
                        100.0
                    },
                ))
            }
        }
        let root = lower_promql("sum by(job)(sum_over_time(a[1m]))", AccuracyTarget::Exact);
        let space = search_workload(vec![("q", root)]);
        let selection = global_selection(&space, &ExplicitCosts);
        let selected = selection
            .for_target(&space.roots[0].1)
            .unwrap()
            .chosen
            .unwrap();
        assert_eq!(selected.provenance, ReplacementProvenance::LogicalRewrite);
    }

    #[test]
    fn global_selection_compares_a_logical_rewrite_with_the_cse_choice() {
        struct PreferLogicalRewrite;

        impl CostModel for PreferLogicalRewrite {
            fn rank_candidates(
                &self,
                _intent: &AggIntent,
                candidates: &[SketchAlgorithm],
            ) -> Vec<SketchAlgorithm> {
                candidates.to_vec()
            }

            fn estimate_cost(
                &self,
                candidate: &ReplacementSubDAG,
                _target: &TargetSubDAG<'_>,
            ) -> f64 {
                match candidate.provenance {
                    ReplacementProvenance::LogicalRewrite => 0.0,
                    _ => 100.0,
                }
            }
        }

        let a = agg(vec![2], AggIntent::Avg { col: None }, metric_scan(&["job"]));
        let b = agg(vec![2], AggIntent::Avg { col: None }, metric_scan(&["job"]));
        let space = search_workload(vec![("a", a), ("b", b)]);
        let root = &space.roots[0].1;
        let selected = global_selection(&space, &PreferLogicalRewrite);

        assert_eq!(
            selected
                .for_target(root)
                .and_then(|group| group.chosen)
                .map(|candidate| candidate.provenance),
            Some(ReplacementProvenance::LogicalRewrite)
        );
    }

    #[test]
    fn topological_order_puts_a_later_discovered_parent_before_its_child() {
        // Mirrors nested_shared_sub-DAG_below_an_unshared_parent_is_still_discovered's
        // diamond fixture: discover_targets's own `order` visits root_b (a
        // parent of `shared`) *after* `shared` itself, because `shared` was
        // already fully walked via root_a first. A naive "process
        // discover_targets's own order" DP would see root_b's child edge
        // after already processing `shared` — topological_order must not
        // make that mistake.
        use asap_types::ir::scalar::ScalarValue;
        use asap_types::ir::Predicate;

        let shared = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let root_a =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Filter {
                pred: Predicate(ScalarExpr::Literal(ScalarValue::Int64(1))),
                child: shared.clone(),
            }))
            .unwrap();
        let root_b =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Filter {
                pred: Predicate(ScalarExpr::Literal(ScalarValue::Int64(2))),
                child: shared,
            }))
            .unwrap();
        let roots = vec![("a", root_a), ("b", root_b)];

        let space = search_workload(roots);
        let order = space.order();
        let dag = reference_dag(&space);

        // Discovery-order sanity: root_b comes after the shared child in
        // discover_targets's own order (the exact non-topological case this
        // test exists to cover).
        let Some(NonASAPOp::Filter {
            child: shared_via_a,
            ..
        }) = space.roots[0].1.non_asap()
        else {
            panic!("expected a Filter root");
        };
        let shared_ptr = Rc::as_ptr(shared_via_a);
        let root_b_ptr = Rc::as_ptr(&space.roots[1].1);
        let shared_discovery_pos = order.iter().position(|p| *p == shared_ptr).unwrap();
        let root_b_discovery_pos = order.iter().position(|p| *p == root_b_ptr).unwrap();
        assert!(
            root_b_discovery_pos > shared_discovery_pos,
            "fixture sanity: discover_targets's own order must NOT already be topological here"
        );

        let topo = topological_order(order, &dag);
        let shared_topo_pos = topo.iter().position(|p| *p == shared_ptr).unwrap();
        let root_b_topo_pos = topo.iter().position(|p| *p == root_b_ptr).unwrap();
        assert!(
            root_b_topo_pos < shared_topo_pos,
            "topological_order must place root_b (a parent of the shared node) before it, \
             unlike discover_targets's own discovery order"
        );
    }

    // ── Selection over Stage 1 candidates (split from `replacement` tests) ─

    // Every exposed query result has a evaluation; internal accumulator frontiers stay states.
    #[test]
    fn selected_query_roots_do_not_leak_exact_accumulator_state() {
        for query in [
            "sum by(job)(rate(m[1m]))",
            "sum by(job)(m)",
            "sum_over_time(m[1m])",
        ] {
            let root = lower_promql(query, AccuracyTarget::Exact);
            let space = search_workload(vec![(0usize, root)]);
            let selected = global_selection(&space, &DefaultCostModel)
                .assemble_selected_query(&space.roots[0].1)
                .unwrap()
                .unwrap();
            assert!(
                selected
                    .schema
                    .fields
                    .iter()
                    .all(|field| matches!(field.dtype, FieldDataType::Plain(_))),
                "{query}: query root leaks state: {:?}",
                selected.schema
            );
        }
    }

    /// Candidates kept with missing accuracy evidence are never chosen automatically.
    #[test]
    fn selection_never_chooses_a_candidate_missing_accuracy_evidence() {
        let never_unproven = |space: &CandidateLogicalASAPDAGs<&str>| {
            let selected = global_selection(space, &DefaultCostModel);
            assert!(!selected
                .for_target(&space.roots[0].1)
                .unwrap()
                .chosen
                .is_some_and(ReplacementSubDAG::has_missing_accuracy_evidence));
            assert!(selected
                .assemble_selected_dag(&space.roots[0].1)
                .unwrap()
                .is_some());
        };
        let count = AggIntent::Count {
            accuracy: AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            },
        };
        never_unproven(&search_workload(vec![(
            "q",
            agg(vec![2], count, metric_scan(&["job"])),
        )]));
        never_unproven(&search_workload_with_targets(
            vec![(
                "q",
                agg(vec![2], default_cardinality(), metric_scan(&["job"])),
                Some(AccuracyTarget::EpsilonDelta {
                    epsilon: 0.01,
                    delta: 0.01,
                }),
            )],
            &default_strategies(),
            &DefaultAccuracyModel,
        ));
        let inner = agg(
            vec![2],
            AggIntent::Count {
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            metric_scan(&["job"]),
        );
        let topk = agg(
            vec![],
            AggIntent::TopK {
                k: 10,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            inner,
        );
        never_unproven(&search_workload_with_targets(
            vec![("q", topk, Some(AccuracyTarget::Epsilon(0.01)))],
            &default_strategies(),
            &DefaultAccuracyModel,
        ));
    }

    /// A root target that rejects every summary leaves nothing to select.
    #[test]
    fn nothing_is_selected_when_the_root_target_rejects_every_summary() {
        let q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let space = search_workload_with_targets(
            vec![("q", q, Some(AccuracyTarget::Epsilon(0.001)))],
            &default_strategies(),
            &DefaultAccuracyModel,
        );
        let root = &space.roots[0].1;
        assert!(global_selection(&space, &DefaultCostModel)
            .for_target(root)
            .unwrap()
            .chosen
            .is_none());
    }

    /// Admits rank-over-rank composition, which `DefaultAccuracyModel` has no
    /// rule for, so a nested quantile summary can be selected.
    struct RankAdditiveModel;

    impl AccuracyModel for RankAdditiveModel {
        fn local_guarantee(
            &self,
            family: &FieldDataType,
            query: &SketchStatistic,
        ) -> Option<ResultGuarantee> {
            DefaultAccuracyModel.local_guarantee(family, query)
        }

        fn propagate(
            &self,
            op: &CompositionOperator,
            inputs: &[ResultGuarantee],
            local: Option<&ResultGuarantee>,
            stats: &PropagationStats,
        ) -> Result<ResultGuarantee, AccuracyError> {
            let rank = |g: &ResultGuarantee| g.is_exact() || g.metric == ErrorMetric::Rank;
            if let (CompositionOperator::ApproximateAggregate, true, Some(local)) =
                (op, inputs.iter().all(rank), local)
            {
                let relabel = |g: &ResultGuarantee| ResultGuarantee {
                    metric: ErrorMetric::AbsoluteValue,
                    ..g.clone()
                };
                let inputs: Vec<_> = inputs.iter().map(relabel).collect();
                let mut out =
                    DefaultAccuracyModel.propagate(op, &inputs, Some(&relabel(local)), stats)?;
                out.metric = local.metric;
                return Ok(out);
            }
            DefaultAccuracyModel.propagate(op, inputs, local, stats)
        }

        fn satisfies(&self, guarantee: &ResultGuarantee, target: &AccuracyTarget) -> bool {
            DefaultAccuracyModel.satisfies(guarantee, target)
        }
    }

    #[test]
    fn global_selection_can_choose_nested_summaries() {
        // The same nested summary remains available through workload search
        // and global cost ranking.
        let inner = agg(
            vec![2],
            quantile_eps_intent(0.5, 0.1),
            metric_scan(&["job"]),
        );
        let outer = agg(vec![], quantile_eps_intent(0.99, 0.1), inner);
        let strategies: Vec<Box<dyn ReplacementStrategy>> = vec![Box::new(
            ASAPStrategies::new_with_planning_inputs(&RankAdditiveModel, &EqualSplitAllocator),
        )];
        let space = search_workload_with(vec![("q", Rc::clone(&outer))], &strategies);
        let root = &space.roots[0].1;
        let group = space.candidates_for_target(root).unwrap();
        assert!(!group.rejected.is_empty());
        assert!(group.candidates.iter().all(|c| match &c.replacement {
            // A summary candidate (old `Replacement::Summary`) contains an
            // ASAP node; a logical rewrite (old `Replacement::Rewrite`) does not.
            Replacement::SubDAG(node) if node.contains_asap() => {
                node.guarantee.as_ref().is_some_and(|g| {
                    DefaultAccuracyModel.satisfies(g, &AccuracyTarget::Epsilon(0.1))
                })
            }
            Replacement::SubDAG(_) => false,
            Replacement::ExactComposition(_) => false,
        }));
        let ranked = cost_sorted(&space, &DefaultCostModel);
        let root_ranked = ranked.iter().find(|g| Rc::ptr_eq(g.target, root)).unwrap();
        assert_eq!(root_ranked.candidates.len(), group.candidates.len());

        let selection = global_selection(&space, &DefaultCostModel);
        let chosen = selection
            .for_target(root)
            .unwrap()
            .chosen
            .expect("a nested summary candidate wins");
        let Replacement::SubDAG(node) = &chosen.replacement else {
            panic!()
        };
        assert!(matches!(
            node.operator,
            Operator::ASAP(ASAPOp::SummaryEstimate { .. })
        ));
    }

    // ── Accuracy reconciliation (moved from `accuracy::reconciliation` tests) ─

    mod accuracy_reconciliation {
        use super::*;
        use asap_types::ir::operator::operator_properties::Source;
        use asap_types::ir::schema::{DataType, Field, Schema};

        /// `[ts(0), value(1), job(2)]`, uniquely keyed by `ts` so CSE can hoist it.
        fn metric_scan() -> Rc<OperatorNode> {
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
                source: Source::TimeSeries { metric: "m".into() },
                predicates: vec![],
                schema: Schema::with_time_index(
                    vec![
                        Field::plain("ts", DataType::Timestamp, false),
                        Field::plain("value", DataType::Float64, false),
                        Field::plain("job", DataType::Utf8, true),
                    ],
                    0,
                    vec![vec![0]],
                ),
            }))
            .unwrap()
        }

        fn quantile(
            q: f64,
            accuracy: AccuracyTarget,
            child: &Rc<OperatorNode>,
        ) -> Rc<OperatorNode> {
            agg(
                vec![2],
                AggIntent::Quantile {
                    col: None,
                    q,
                    accuracy,
                },
                Rc::clone(child),
            )
        }

        // ── cost: reading the sibling must not be priced like recomputing
        //    `target` independently per consumer ────────────────────────────────

        #[test]
        fn estimate_cost_does_not_scale_with_the_readers_own_consumer_count() {
            // Regression guard for the review-reported sign inversion: pricing
            // this candidate like `CseRecompute` ("rebuild `target`, once per
            // consumer") made it artificially *more* expensive exactly as more
            // of `target`'s own consumers stood to benefit from reading the
            // already-necessary tighter sibling instead — the literal opposite
            // of the intended incentive. The real cost is "one more read against
            // `rc`'s own build," which must not scale with `target`'s own
            // `consumer_count`.
            let scan = metric_scan();
            let tight = quantile(0.99, AccuracyTarget::Epsilon(0.01), &scan);
            let loose = quantile(0.99, AccuracyTarget::Epsilon(0.05), &scan);
            let strategy =
                AccuracyReconciliationStrategy::new(&[Rc::clone(&tight), Rc::clone(&loose)]);
            let candidate = strategy
                .replacements(&TargetSubDAG::new(&loose))
                .into_iter()
                .next()
                .expect("loose has a reconciliation candidate reading the tight sibling");

            let cost_model = DefaultCostModel;
            let single_consumer = TargetSubDAG::with_consumer_count(&loose, 1);
            let many_consumers = TargetSubDAG::with_consumer_count(&loose, 5);

            let cost_single = cost_model.estimate_cost(&candidate, &single_consumer);
            let cost_many = cost_model.estimate_cost(&candidate, &many_consumers);

            assert!(
                cost_single.is_finite(),
                "expected a real cost, not the NaN placeholder: {cost_single}"
            );
            assert_eq!(
                cost_single, cost_many,
                "AccuracyReconciliation's estimate_cost must price 'read the sibling', not scale \
                 with the reader's own consumer_count the way CseRecompute's 'rebuild independently \
                 per consumer' formula does (single-consumer: {cost_single}, 5 consumers: \
                 {cost_many})"
            );
        }

        // ── cost_sorted / global_selection: single-consumer and shared-consumer ─

        #[test]
        fn cost_sorted_and_global_selection_handle_a_single_consumer_looser_target() {
            let scan = metric_scan();
            let tight = quantile(0.99, AccuracyTarget::Epsilon(0.01), &scan);
            let loose = quantile(0.99, AccuracyTarget::Epsilon(0.05), &scan);
            let space = search_workload(vec![("tight", tight), ("loose", loose)]);
            let loose_root = &space.roots[1].1;

            let cost_model = DefaultCostModel;
            let ranked = cost_sorted(&space, &cost_model);
            let loose_ranked = ranked
                .iter()
                .find(|group| Rc::ptr_eq(group.target, loose_root))
                .expect("loose has its own ranked group");
            assert!(
                loose_ranked.costs.iter().all(|cost| cost.is_finite()),
                "no candidate should cost NaN under DefaultCostModel: {:?}",
                loose_ranked.costs
            );
            assert!(
                loose_ranked
                    .candidates
                    .iter()
                    .any(|c| c.strategy == "AccuracyReconciliationStrategy"),
                "the reconciliation candidate must still be present, ranked, not filtered"
            );

            let selected = global_selection(&space, &cost_model);
            let chosen = selected
                .for_target(loose_root)
                .and_then(|group| group.chosen);
            assert!(
                chosen.is_some(),
                "global_selection must commit to some candidate for a single-consumer looser target"
            );
            // With no recompute term at all (it never rebuilds `target`), this
            // candidate strictly undercuts every ASAPStrategies
            // candidate (which each pay a recompute term on top of their own
            // maintenance term) under DefaultCostModel's numbers — the sane
            // direction: reading an already-necessary sibling should be able to
            // win on its own merit, not just fail to lose as badly as before.
            assert_eq!(
                chosen.map(|c| c.provenance),
                Some(asap_logical_optimizer::pass1::replacement::ReplacementProvenance::AccuracyReconciliation)
            );
        }

        #[test]
        fn cost_sorted_and_global_selection_handle_a_shared_looser_target() {
            // The loose accuracy target itself has 2 direct consumers (two
            // independently-built but structurally identical loose queries
            // merge onto one Rc via ordinary CSE), *and* a separate,
            // single-consumer tight sibling exists over the same input — the
            // scenario the issue itself targets: `SharedSubDAGStrategy`'s own
            // CseShare/CseRecompute pair is on the table for the loose target's
            // own 2 consumers at the same time as this strategy's "read the
            // tight sibling instead" candidate.
            let scan = metric_scan();
            let loose_a = (*quantile(0.99, AccuracyTarget::Epsilon(0.05), &scan)).clone();
            let loose_b = (*quantile(0.99, AccuracyTarget::Epsilon(0.05), &scan)).clone();
            let tight = (*quantile(0.99, AccuracyTarget::Epsilon(0.01), &scan)).clone();

            let space = search_workload(vec![
                ("loose_a", Rc::new(loose_a)),
                ("loose_b", Rc::new(loose_b)),
                ("tight", Rc::new(tight)),
            ]);

            // Fixture sanity: the two loose roots really did merge onto one Rc.
            assert!(Rc::ptr_eq(&space.roots[0].1, &space.roots[1].1));
            let loose_group = space
                .candidates_for_target(&space.roots[0].1)
                .expect("the merged loose target has a group");
            assert_eq!(loose_group.consumer_count, 2);
            assert!(
                loose_group
                    .candidates
                    .iter()
                    .any(|c| c.strategy == "AccuracyReconciliationStrategy"),
                "the reconciliation candidate must still be proposed alongside the CSE share/recompute \
                 pair, not crowded out: {:?}",
                loose_group
                    .candidates
                    .iter()
                    .map(|c| (c.strategy, c.provenance))
                    .collect::<Vec<_>>()
            );

            let cost_model = DefaultCostModel;
            let ranked = cost_sorted(&space, &cost_model);
            let loose_ranked = ranked
                .iter()
                .find(|group| Rc::ptr_eq(group.target, &space.roots[0].1))
                .expect("loose has its own ranked group");
            assert!(
                loose_ranked.costs.iter().all(|cost| cost.is_finite()),
                "no candidate should cost NaN under DefaultCostModel, shared or not: {:?}",
                loose_ranked.costs
            );

            let selected = global_selection(&space, &cost_model);
            let chosen = selected
                .for_target(&space.roots[0].1)
                .and_then(|group| group.chosen);
            assert!(
                chosen.is_some(),
                "global_selection must commit to some candidate for the shared looser target"
            );
            // Under `DefaultCostModel`'s numbers, `CseShare` (flat maintenance,
            // no recompute term) and this strategy's own candidate (also a
            // flat, non-scaling read cost after the fix) land tied, and
            // `global_selection` breaks ties in `CseShare`'s favor (it only ever
            // overrides the CSE choice on a *strict* `<`, not `<=`) — a sane,
            // deliberate tie-break, not the "reconciliation always loses to
            // CseShare regardless of its own real merit" bug this test guards
            // against (see `estimate_cost_does_not_scale_with_the_readers_own_consumer_count`
            // for the direct regression check that the old `* consumer_count`
            // scaling — which made this an unfair, ever-widening loss instead
            // of a tie — is gone).
            assert_eq!(
                chosen.map(|c| c.provenance),
                Some(asap_logical_optimizer::pass1::replacement::ReplacementProvenance::CseShare)
            );
        }

        #[test]
        fn global_selection_propagates_reconciled_consumers_to_the_tighter_group() {
            struct PreferReconciliation;

            impl CostModel for PreferReconciliation {
                fn rank_candidates(
                    &self,
                    _intent: &AggIntent,
                    candidates: &[SketchAlgorithm],
                ) -> Vec<SketchAlgorithm> {
                    candidates.to_vec()
                }

                fn estimate_cost(
                    &self,
                    candidate: &ReplacementSubDAG,
                    _target: &TargetSubDAG<'_>,
                ) -> f64 {
                    if candidate.provenance == ReplacementProvenance::AccuracyReconciliation {
                        0.0
                    } else {
                        100.0
                    }
                }
            }

            let scan = metric_scan();
            let tight = quantile(0.99, AccuracyTarget::Epsilon(0.01), &scan);
            let loose = quantile(0.99, AccuracyTarget::Epsilon(0.05), &scan);
            let space = search_workload(vec![
                ("tight", Rc::clone(&tight)),
                ("loose", Rc::clone(&loose)),
            ]);

            let selected = global_selection(&space, &PreferReconciliation);
            let tight_root = &space.roots[0].1;
            let loose_root = &space.roots[1].1;
            assert_eq!(
                selected
                    .for_target(loose_root)
                    .and_then(|group| group.chosen)
                    .map(|candidate| candidate.provenance),
                Some(ReplacementProvenance::AccuracyReconciliation),
                "fixture must select the cross-sibling rewrite"
            );
            assert_eq!(
                selected
                    .for_target(tight_root)
                    .expect("the tighter sibling is a discovered memo group")
                    .effective_consumer_count,
                2,
                "the tighter build serves its original root and the reconciled looser root"
            );
        }
    }
}
