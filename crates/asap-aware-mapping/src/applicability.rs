//! Optimization-applicability *reporting* over a workload's pre-ASAP query
//! roots (issue #33: "Add logic to detect which optimizations are applicable
//! to a query workload"; this module: issue #257).
//!
//! ## The reframing: applicability *is* "this `TargetSubDAG`'s candidate
//! list is non-trivial"
//!
//! Earlier (PR #247, superseded by this module — see "What this replaces"
//! below), "is optimization X applicable here?" was a yes/no fact each rule
//! re-derived by walking the tree itself: [`crate::implementation::implementations_for_with`]
//! for sketches, `share_common_subtrees` for reuse. That made sense before
//! there was any other structure to consult. But [`crate::search::search_workload`]
//! (issue #252) now *already* computes, for every [`TargetSubDAG`](crate::replacement::TargetSubDAG)
//! in the workload, every semantically valid [`crate::replacement::ReplacementSubDAG`]
//! a registered [`ReplacementStrategy`] can propose — a [`PlanSpace`] of
//! [`MemoGroup`]s. A rule re-deriving the same yes/no fact from scratch would
//! be answering a question the search already answered, via a second,
//! independently maintained traversal that has to keep agreeing with the
//! first one.
//!
//! Once that candidate space exists, "which optimizations are applicable"
//! collapses into a single question this module asks of *that* data instead:
//! **for a given `TargetSubDAG`, does its candidate list contain anything
//! other than the trivial, no-op realization?** A `TargetSubDAG` whose only
//! candidate is "the one thing `implementation::implementations_for_with`
//! would have committed to anyway, with no alternative" has no optimization
//! to report — that candidate isn't an *opportunity*, it's just the target's
//! existing shape reflected back. A `TargetSubDAG` with more than one
//! candidate (several sketch families to choose between), or one candidate
//! that is itself a genuine alternative to the status quo (share this
//! already-shared subtree instead of recomputing it at every consumer), *is*
//! an applicability finding — [`find_applicable_optimizations`] and
//! [`find_applicable_optimizations_with`] just translate [`PlanSpace`]'s
//! [`MemoGroup`]s into that shape:
//!
//! - [`OptimizationKind::SketchApproximation`] — the `TargetSubDAG`'s
//!   candidate list contains at least one [`Replacement::Summary`] that
//!   actually realizes a sketch family (`SummaryFamilyType::Sketch`), i.e.
//!   [`SketchFamilyStrategy`] found something to offer beyond the
//!   exact/pass-through candidate [`crate::implementation::implementations_for_with`]
//!   would have committed to on its own.
//! - [`OptimizationKind::CommonSubexpressionReuse`] — the `TargetSubDAG`
//!   has two or more consumers *and* its candidate list contains the
//!   [`SharedSubtreeStrategy`] "build once and share" candidate (the one
//!   whose `Rc` is the group's own `target`) — i.e. sharing this subtree
//!   instead of recomputing it independently is a real, reported choice, not
//!   just an accident of how the workload happened to be built.
//!
//! Each finding's `reason` is literally the matching candidate's own
//! [`ReplacementSubDAG::rationale`] (joined, if more than one candidate
//! qualifies) — this module invents no new prose to explain *why* a
//! candidate is valid; that explanation already exists on the candidate a
//! [`ReplacementStrategy`] produced, and repeating it here (rather than
//! re-describing the same fact in different words) keeps exactly one place
//! that has to be right about "why is this a valid alternative".
//!
//! ## What this replaces, and what carries over unmodified
//!
//! [`ApplicabilityFinding`] and [`OptimizationKind`] keep the exact public
//! shape PR #247 shipped — same fields, same two variants, same
//! `#[non_exhaustive]` discipline (only an optimization backed by a real,
//! registered [`ReplacementStrategy`] gets a variant; see the catalog table
//! below for everything still deliberately unrepresented). So do the two
//! top-level entry points, [`find_applicable_optimizations`] and
//! [`find_applicable_optimizations_with`] — same "workload roots in, findings
//! out" contract, only their *data source* changed underneath: they now call
//! [`crate::search::search_workload`]/[`crate::search::search_workload_with`]
//! and translate the result, rather than running their own two rules
//! (`SketchApplicabilityRule`, `SharedSubexpressionRule`) and their supporting
//! traversal (`collect_sketch_findings`, `register_site`, `walk_rc_children`,
//! `for_each_operator_child`) over the tree a second time. All of that old
//! traversal is deleted, not kept alongside the new implementation — see
//! "Two guarantees the old traversal made, re-verified" below for the two
//! properties it's important that deletion didn't quietly lose.
//!
//! ## Why [`ApplicabilityRule`] (PR #247's extension point) is gone, not kept
//!
//! PR #247 gave this module its own extension-point trait, `ApplicabilityRule`
//! (`fn optimization(&self) -> OptimizationKind` + `fn evaluate(&self, roots)
//! -> Vec<ApplicabilityFinding>`), the same shape [`crate::cost_model::CostModel`]
//! and [`crate::implementation::Matcher`] use elsewhere in this crate. Once
//! findings are a *view* over [`PlanSpace`] rather than an independent
//! computation, that trait becomes a second extension point answering a
//! question [`ReplacementStrategy`] (issue #251) already answers: "does this
//! `TargetSubDAG` have an alternative worth reporting, and why". A caller who wants a
//! new optimization represented as a finding needs a new
//! `impl ReplacementStrategy` wired into [`crate::search::search_workload_with`]'s
//! strategy set *regardless* (that's the only way its candidates end up in
//! the [`PlanSpace`] this module reads) — adding an `ApplicabilityRule` too
//! would mean maintaining two extension points for the same new capability,
//! one of which (the rule) would just be re-describing candidates the other
//! (the strategy) already produced. So this module ships no extension-point
//! trait of its own: [`ReplacementStrategy`] already *is* that extension
//! point, one layer down, and [`find_applicable_optimizations_with`]'s own
//! `strategies` parameter is where a caller plugs in a custom one (or a
//! custom `CostModel`, via [`SketchFamilyStrategy::new`]) — the identical
//! spot [`crate::search::search_workload_with`] itself exposes.
//!
//! ## Two guarantees the old traversal made, re-verified against the new one
//!
//! 1. **A finding is reported at the maximal `TargetSubDAG`, never once more
//!    per subsumed descendant.** [`crate::search::discover_sites`] (used by
//!    [`crate::search::search_workload_with`], and so by this module) walks
//!    every workload root's whole DAG but — exactly like PR #247's own
//!    `register_site` did — only *recurses into a node's children the first
//!    time that node's `Rc` is seen*; every subsequent occurrence still
//!    counts towards `consumer_count`, but never triggers a second descent.
//!    A node nested under an already-discovered shared ancestor therefore
//!    only becomes its own `TargetSubDAG` if something *outside* that
//!    ancestor also references it — identical to the old code's rationale
//!    for why
//!    `SharedSubexpressionRule` reported "the highest point sharing starts,"
//!    not a finding at every subsumed level below it. Same guarantee, same
//!    mechanism, just living in `search.rs` now instead of here.
//! 2. **A node reachable via more than one path is one finding, not one per
//!    path.** [`MemoGroup`]s are keyed by `Rc` pointer identity in
//!    [`PlanSpace`]'s internal map — there is exactly one group per distinct
//!    `Rc`, full stop, so a shared `Aggregate` reached via two different
//!    `BinaryOp` branches (or two different workload roots) is exactly one
//!    group, hence at most one [`OptimizationKind::SketchApproximation`]
//!    finding, no matter how many paths reach it.
//!    [`tests::a_shared_sketchable_aggregate_is_reported_only_once`] pins
//!    this directly, unchanged from PR #247's own test of the same name.
//!
//! ## One thing [`PlanSpace`] doesn't carry that this module still needs:
//! human-readable `location` text
//!
//! [`MemoGroup`]/[`PlanSpace`] deliberately track only `Rc<QueryExpr>` pointer
//! identity — the currency the search itself needs — not caller-facing
//! prose. [`ApplicabilityFinding::location`] is prose (a breadcrumb like
//! `root "dash_a" > lhs`), so this module keeps one small, self-contained
//! walk of its own, [`collect_locations`], whose *only* job is turning "this
//! `Rc`" into "the human-readable place(s) it occurs" for a finding already
//! decided by [`PlanSpace`]. This is not a reincarnation of the deleted
//! rule traversal: it makes no applicability decision (it runs the same
//! regardless of what any strategy found) and duplicating this small,
//! self-contained shape rather than threading location strings through
//! `search.rs`'s own [`crate::search::discover_sites`] matches the same call
//! that module's own docs already make for its (test-only) `count_consumers`
//! counterpart — see [`crate::search`]'s "Where `for site in
//! plan.bindable_sites()` comes from" section.
//!
//! ## Catalog primitives deliberately left as future work
//!
//! The internal catalog
//! (`ProjectASAP/internal-docs/catalog_of_optimizations.md`) lists several
//! primitives with **no [`ReplacementStrategy`] implementation anywhere in
//! this codebase today**. Faking a variant for one of them would report a
//! finding this codebase cannot back with a real candidate, so none of the
//! below get an [`OptimizationKind`] variant yet — each gets one once a real
//! strategy exists and is wired into [`crate::search::default_strategies`]:
//!
//! | Catalog entry | Status | Where a future `OptimizationKind` would come from |
//! |---|---|---|
//! | Semantic-equivalent rewriting (e.g. `avg` → `sum`/`count`) | A `ReplacementStrategy` exists (`AvgToSumOverCountStrategy`, issue #253) but is not yet in `crate::search::default_strategies()` | Once wired in: any `Replacement::Rewrite` candidate that strategy proposes |
//! | Roll-ups (fine-to-coarse group-by reuse) | A `ReplacementStrategy` exists (`RollupStrategy`, issue #254) but is not yet in `crate::search::default_strategies()` | Once wired in: any `Replacement::Rewrite` candidate that strategy proposes |
//! | Wavelets/OMP | Params type exists (`WaveletKind`/`WaveletParams`), reachable only via a deployment `CostModel::realize_extension` (no core `AggIntent` dispatch picks it) | A `ReplacementStrategy` that inspects a deployment's own `CostModel`, once some intent shape actually maps to `Implementation::Wavelet` |
//! | Sampling | Same story as Wavelets: `SamplingKind`/`SamplingParams` exist, unreachable from core dispatch | Same hook as Wavelets, for `Implementation::Sample` |
//! | Deep generative compression | No representation at all — no `Implementation`/`SummaryFamilyType` variant | Needs a new summary family added to `asap_types::post_asap` first |
//! | Approximation frameworks for windows | No representation — `TimeRange`/`PromqlSubquery` windows are always evaluated exactly | Would key off those node types once an approximate-window operator exists |
//! | Function decomposition | No representation anywhere | No hook point identified yet |
//! | Continuous distributed monitoring | No representation — `RepeatingEntry`/`RepetitionInterval` in `asap_types::workload` describe *that* a query repeats, not any monitoring-specific decomposition | Would likely key off `RepeatingEntry` once such logic exists |
//! | Incremental computation across time | No representation — nothing carries state across repeated evaluations of a `RepeatingEntry` today | Would key off `RepeatingEntry` + `TimeShift`/`TimeRange` once incremental state-carry exists |
//! | Delta encoding | No representation — `AggIntent::Delta`/`IDelta` are PromQL *value*-difference semantics, not a wire/storage delta-encoding optimization | Would plug into a future deployment-side wire/storage encoding decision (post-ASAP), not this crate's IR-level dispatch |
//!
//! [`ReplacementStrategy`]: crate::replacement::ReplacementStrategy
//! [`ReplacementSubDAG`]: crate::replacement::ReplacementSubDAG
//! [`Replacement`]: crate::replacement::Replacement
//! [`Replacement::Summary`]: crate::replacement::Replacement::Summary
//! [`Replacement::Rewrite`]: crate::replacement::Replacement::Rewrite
//! [`SketchFamilyStrategy`]: crate::replacement::SketchFamilyStrategy
//! [`SharedSubtreeStrategy`]: crate::replacement::SharedSubtreeStrategy
//! [`PlanSpace`]: crate::search::PlanSpace
//! [`MemoGroup`]: crate::search::MemoGroup

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::rc::Rc;

use asap_types::post_asap::{SummaryExpr, SummaryFamilyType, SummaryNode};
use asap_types::pre_asap::query_expr::QueryExpr;

use crate::replacement::{Replacement, ReplacementStrategy};
use crate::search::{self, MemoGroup, PlanSpace};

/// Which optimization an [`ApplicabilityFinding`] is about.
///
/// `#[non_exhaustive]`: only optimizations with a real [`ReplacementStrategy`]
/// behind them get a variant (see the module docs' "Catalog primitives
/// deliberately left as future work" table for everything else in the
/// catalog).
///
/// [`ReplacementStrategy`]: crate::replacement::ReplacementStrategy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OptimizationKind {
    /// A `TargetSubDAG`'s candidate list contains at least one
    /// [`Replacement::Summary`] that realizes a sketch family —
    /// [`crate::replacement::SketchFamilyStrategy`] found a genuine sketch
    /// alternative for this `Aggregate`, beyond whatever exact/pass-through
    /// candidate [`crate::implementation::implementations_for_with`] would
    /// have committed to on its own.
    SketchApproximation,
    /// A `TargetSubDAG` has two or more consumers *and* its candidate list
    /// contains [`crate::replacement::SharedSubtreeStrategy`]'s "build once and
    /// share" candidate — the catalog's cross-statistic / cross-metrics /
    /// cross-subpopulation reuse entries, all the same underlying structural
    /// fact.
    CommonSubexpressionReuse,
}

/// One positive applicability result: `optimization` is applicable at
/// `location` (a human-readable breadcrumb into the workload — e.g.
/// `root "dashboard_p99"` or `root "ratio" > lhs`), for `reason`
/// (human-readable, meant for a report/log, not machine parsing — literally
/// the matching candidate's own [`crate::replacement::ReplacementSubDAG::rationale`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicabilityFinding {
    pub optimization: OptimizationKind,
    pub location: String,
    pub reason: String,
}

/// Determine which known optimizations are applicable to a workload's
/// pre-ASAP query roots, using [`crate::search::default_strategies`].
///
/// `roots` — like [`crate::search::search_workload`]'s own `Id` type
/// parameter — is caller-chosen: a `QueryWorkload` entry's own key, an index,
/// a query name. It only needs [`Display`], since a finding's `location` is
/// prose, not a structured key back to the caller.
///
/// Internally runs [`crate::search::search_workload`] to build the
/// candidate-plan space, then reads findings off it — see the module docs'
/// "The reframing" section for what that translation actually checks.
pub fn find_applicable_optimizations<Id: Display>(
    roots: Vec<(Id, QueryExpr)>,
) -> Vec<ApplicabilityFinding> {
    find_applicable_optimizations_with(roots, &search::default_strategies())
}

/// Like [`find_applicable_optimizations`], but searches with `strategies`
/// instead of [`crate::search::default_strategies`] — the extension point for
/// a deployment-specific [`ReplacementStrategy`], or a custom `CostModel`
/// plugged into [`crate::replacement::SketchFamilyStrategy::new`] (e.g. via
/// [`crate::search::default_strategies_with`]).
///
/// [`ReplacementStrategy`]: crate::replacement::ReplacementStrategy
pub fn find_applicable_optimizations_with<'s, Id: Display>(
    roots: Vec<(Id, QueryExpr)>,
    strategies: &[Box<dyn ReplacementStrategy + 's>],
) -> Vec<ApplicabilityFinding> {
    let ided: Vec<(String, Rc<QueryExpr>)> = roots
        .into_iter()
        .map(|(id, expr)| (id.to_string(), Rc::new(expr)))
        .collect();
    let space = search::search_workload_with(ided, strategies);
    findings_from_plan_space(&space)
}

/// Translate every discovered [`MemoGroup`] in `space` into zero, one, or two
/// [`ApplicabilityFinding`]s (a `TargetSubDAG` can be both sketch-approximable
/// *and* shared — the two optimizations are independent axes, not mutually
/// exclusive).
///
/// `space`'s own `Id` is always `String` here: [`find_applicable_optimizations_with`]
/// already converted the caller's `Id: Display` into a `String` (via
/// `to_string()`) before calling [`crate::search::search_workload_with`], so
/// this function (and [`collect_locations`], which formats `id` with
/// [`std::fmt::Debug`] for the breadcrumb text) doesn't need its own generic
/// `Id` bound.
fn findings_from_plan_space(space: &PlanSpace<String>) -> Vec<ApplicabilityFinding> {
    let locations = collect_locations(&space.roots);
    let mut findings = Vec::new();
    for group in space.groups() {
        let location = locations
            .get(&Rc::as_ptr(&group.target))
            .map(|locs| locs.join(", "))
            .unwrap_or_default();

        if let Some(reason) = sketch_finding_reason(group) {
            findings.push(ApplicabilityFinding {
                optimization: OptimizationKind::SketchApproximation,
                location: location.clone(),
                reason,
            });
        }
        if let Some(reason) = shared_subexpr_finding_reason(group) {
            findings.push(ApplicabilityFinding {
                optimization: OptimizationKind::CommonSubexpressionReuse,
                location,
                reason,
            });
        }
    }
    findings
}

/// Does `group`'s candidate list contain a genuine sketch-family realization?
/// If so, the finding's `reason` is every such candidate's own `rationale`,
/// joined — this module does not invent new prose to restate why a candidate
/// is valid.
fn sketch_finding_reason(group: &MemoGroup) -> Option<String> {
    let reasons: Vec<&str> = group
        .candidates
        .iter()
        .filter(
            |c| matches!(&c.replacement, Replacement::Summary(node) if is_sketch_realization(node)),
        )
        .map(|c| c.rationale.as_str())
        .collect();
    if reasons.is_empty() {
        None
    } else {
        Some(reasons.join("; "))
    }
}

/// Does `group` have two or more consumers *and* a "build once and share"
/// candidate (the [`Replacement::Rewrite`] whose `Rc` is the group's own
/// `target`) in its candidate list? If so, the finding's `reason` is that
/// candidate's own `rationale`.
fn shared_subexpr_finding_reason(group: &MemoGroup) -> Option<String> {
    if group.consumer_count < 2 {
        return None;
    }
    group
        .candidates
        .iter()
        .find(
            |c| matches!(&c.replacement, Replacement::Rewrite(rc) if Rc::ptr_eq(rc, &group.target)),
        )
        .map(|c| c.rationale.clone())
}

/// Does `node` (unwrapping any `SummaryEstimate` layer, the same shape
/// [`crate::search`]'s own private `sketch_kind_of` unwraps) ultimately
/// realize a [`SummaryFamilyType::Sketch`] family? This module only needs the
/// yes/no fact (a candidate's own `rationale` already names the specific
/// `SketchKind` for a finding's `reason` text), so unlike `search.rs`'s
/// counterpart this returns `bool`, not the kind itself.
fn is_sketch_realization(node: &SummaryNode) -> bool {
    match &node.expr {
        SummaryExpr::SummaryEstimate { summary_input, .. } => is_sketch_realization(summary_input),
        SummaryExpr::SummaryAgg { family, .. } => matches!(family, SummaryFamilyType::Sketch(..)),
        _ => false,
    }
}

// ── location breadcrumbs ─────────────────────────────────────────────────

/// Build `location` text for every distinct `TargetSubDAG` reachable from
/// `roots` — see
/// the module docs' "One thing `PlanSpace` doesn't carry" section for why
/// this module needs its own small walk for this. Returns every breadcrumb
/// path that reaches a given `Rc`, not just the first: a shared node
/// referenced from two workload roots (or two branches of one root) needs
/// both breadcrumbs in its finding's `location`, not just one.
fn collect_locations(roots: &[(String, Rc<QueryExpr>)]) -> HashMap<*const QueryExpr, Vec<String>> {
    let mut locations: HashMap<*const QueryExpr, Vec<String>> = HashMap::new();
    let mut children_walked: HashSet<*const QueryExpr> = HashSet::new();
    for (id, root) in roots {
        visit(
            root,
            format!("root {id:?}"),
            &mut locations,
            &mut children_walked,
        );
    }
    locations
}

/// Record `label` as one of `node`'s breadcrumbs, then — only the first time
/// this exact `Rc` is seen — recurse into its children. Every subsequent
/// visit still records its own `label` (a genuinely different path reaching
/// the same shared node), it just doesn't re-walk that node's children a
/// second time — the same "walk once, but every occurrence still counts"
/// split [`crate::search::discover_sites`]'s own `walk` makes, applied here
/// to text instead of a consumer count.
fn visit(
    node: &Rc<QueryExpr>,
    label: String,
    locations: &mut HashMap<*const QueryExpr, Vec<String>>,
    children_walked: &mut HashSet<*const QueryExpr>,
) {
    let ptr = Rc::as_ptr(node);
    locations.entry(ptr).or_default().push(label.clone());
    if children_walked.insert(ptr) {
        visit_children(node, &label, locations, children_walked);
    }
}

/// `node`'s own **relational-skeleton** operator children — the same scope
/// [`crate::search::discover_sites`]'s own `walk_children` (and
/// `asap_types::pre_asap::cse::share_common_subtrees`'s `rebuild_children`)
/// use. Exhaustive over every `QueryExpr` variant: a new variant fails to
/// compile here until this match is extended too.
fn visit_children(
    node: &QueryExpr,
    label: &str,
    locations: &mut HashMap<*const QueryExpr, Vec<String>>,
    children_walked: &mut HashSet<*const QueryExpr>,
) {
    use QueryExpr::*;
    match node {
        Scan { .. } | PromqlScalarBridge(_) | QueryTimestamp => {}
        PromqlVectorFromScalar(c) | PromqlScalarFromVector(c) => {
            visit(c, format!("{label} > child"), locations, children_walked)
        }
        PromqlRelabel { child, .. }
        | PromqlInfoEnrich { child, .. }
        | PromqlSeriesSample { child, .. }
        | Filter { child, .. }
        | Project { child, .. }
        | Aggregate { child, .. }
        | Dedup { child, .. }
        | PromqlSubquery { child, .. }
        | TimeRange { child, .. }
        | TimeShift { child, .. }
        | SQLWindowFunc { child, .. }
        | Sort { child, .. }
        | Limit { child, .. } => visit(
            child,
            format!("{label} > child"),
            locations,
            children_walked,
        ),
        Concat { children } => {
            for (i, c) in children.iter().enumerate() {
                visit_children(
                    c,
                    &format!("{label} > concat[{i}]"),
                    locations,
                    children_walked,
                );
            }
        }
        Join { left, right, .. } | SetOp { left, right, .. } => {
            visit(left, format!("{label} > left"), locations, children_walked);
            visit(
                right,
                format!("{label} > right"),
                locations,
                children_walked,
            );
        }
        BinaryOp { lhs, rhs, .. } => {
            visit(lhs, format!("{label} > lhs"), locations, children_walked);
            visit(rhs, format!("{label} > rhs"), locations, children_walked);
        }
        Column(_)
        | Literal(_)
        | Compare { .. }
        | BoolAnd(_)
        | BoolOr(_)
        | Not(_)
        | IsNull(_)
        | IsNotNull(_)
        | Cast { .. }
        | InList { .. }
        | FunctionCall { .. }
        | Arithmetic { .. }
        | Case { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::pre_asap::agg_intent::{default_quantile, AggIntent};
    use asap_types::pre_asap::query_expr::{Reduction, Source};
    use asap_types::pre_asap::schema::{Column, DataType, Schema};
    use asap_types::types::AccuracyTarget;

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

    // ── SketchApproximation ──────────────────────────────────────────────

    #[test]
    fn approximate_quantile_is_a_sketch_applicability_finding() {
        let q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let findings = find_applicable_optimizations(vec![("dashboard_p99", q)]);
        let sketch: Vec<_> = findings
            .iter()
            .filter(|f| f.optimization == OptimizationKind::SketchApproximation)
            .collect();
        assert_eq!(
            sketch.len(),
            1,
            "expected one sketch finding, got {findings:?}"
        );
        assert!(sketch[0].location.contains("dashboard_p99"));
        assert!(sketch[0].reason.to_lowercase().contains("kll"));
    }

    #[test]
    fn exact_quantile_is_not_a_sketch_applicability_finding() {
        let q = agg(
            vec![2],
            AggIntent::Quantile {
                col: None,
                q: 0.99,
                accuracy: AccuracyTarget::Exact,
            },
            metric_scan(&["job"]),
        );
        let findings = find_applicable_optimizations(vec![("exact_p99", q)]);
        assert!(
            findings
                .iter()
                .all(|f| f.optimization != OptimizationKind::SketchApproximation),
            "an Exact accuracy target must not report sketch-applicability, got {findings:?}"
        );
    }

    #[test]
    fn nested_aggregate_still_finds_the_inner_sketchable_node() {
        // avg(quantile(0.9, sum by (job) (m))) shaped test isn't representable
        // (avg is PassThrough, not a wrapper we recurse through structurally
        // the way an Aggregate's own child is) — instead nest a sketchable
        // quantile under an exact sum, the same nesting bind.rs's own
        // nested_aggregates_bind_per_node test uses.
        let inner = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let outer = agg(vec![], default_quantile(0.9), inner);
        let findings = find_applicable_optimizations(vec![("q", outer)]);
        let sketch_count = findings
            .iter()
            .filter(|f| f.optimization == OptimizationKind::SketchApproximation)
            .count();
        assert_eq!(
            sketch_count, 1,
            "expected the outer quantile only, got {findings:?}"
        );
    }

    #[test]
    fn pass_through_intent_reports_no_sketch_finding() {
        let q = agg(vec![2], AggIntent::Avg { col: None }, metric_scan(&["job"]));
        let findings = find_applicable_optimizations(vec![("avg_latency", q)]);
        assert!(findings
            .iter()
            .all(|f| f.optimization != OptimizationKind::SketchApproximation));
    }

    /// A sketch-applicable `Aggregate` reachable via two paths that CSE
    /// collapses onto one `Rc` — the same `median(x) == median(x)` shape
    /// `pre_asap::cse`'s own `single_query_shares_its_own_repeated_subtree`
    /// test uses — must be reported once, not once per path: it is exactly
    /// one [`crate::search::MemoGroup`], keyed by `Rc` pointer identity, not
    /// one per path that reaches it.
    #[test]
    fn a_shared_sketchable_aggregate_is_reported_only_once() {
        let quantile = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let root = QueryExpr::BinaryOp {
            op: asap_types::pre_asap::query_expr::BinaryOpKind::Compare(
                asap_types::pre_asap::expr_ir::CompareOpKind::Eq,
            ),
            lhs: Rc::new(quantile.clone()),
            rhs: Rc::new(quantile),
            vector_match: None,
        };
        let findings = find_applicable_optimizations(vec![("ratio", root)]);
        let sketch: Vec<_> = findings
            .iter()
            .filter(|f| f.optimization == OptimizationKind::SketchApproximation)
            .collect();
        assert_eq!(
            sketch.len(),
            1,
            "a single shared Aggregate must produce one finding, not one \
             per path that reaches it: got {findings:?}"
        );
    }

    // ── CommonSubexpressionReuse ─────────────────────────────────────────

    #[test]
    fn two_roots_with_the_same_grouped_aggregate_share_a_reuse_finding() {
        // Grouped (`by (job)`), so the shared `Aggregate`'s output schema
        // carries a provable unique key — share_common_subtrees's legality
        // gate — and identical across both roots, so it is shareable.
        let a = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let b = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let findings = find_applicable_optimizations(vec![("dash_a", a), ("dash_b", b)]);
        let reuse: Vec<_> = findings
            .iter()
            .filter(|f| f.optimization == OptimizationKind::CommonSubexpressionReuse)
            .collect();
        assert_eq!(
            reuse.len(),
            1,
            "expected one reuse finding, got {findings:?}"
        );
        assert!(reuse[0].location.contains("dash_a"));
        assert!(reuse[0].location.contains("dash_b"));
    }

    #[test]
    fn distinct_queries_report_no_reuse_finding() {
        let a = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let b = agg(
            vec![2],
            AggIntent::Sum { col: None },
            metric_scan(&["route"]),
        );
        let findings = find_applicable_optimizations(vec![("dash_a", a), ("dash_b", b)]);
        assert!(
            findings
                .iter()
                .all(|f| f.optimization != OptimizationKind::CommonSubexpressionReuse),
            "structurally different queries must not report reuse, got {findings:?}"
        );
    }

    #[test]
    fn ungrouped_identical_aggregates_are_not_shareable_so_no_finding() {
        // Empty `by`: no provable unique key — share_common_subtrees never
        // hoists these, so consumer_count stays 1 for each and this module
        // must not report a finding either.
        let a = agg(vec![], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let b = agg(vec![], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let findings = find_applicable_optimizations(vec![("a", a), ("b", b)]);
        assert!(findings
            .iter()
            .all(|f| f.optimization != OptimizationKind::CommonSubexpressionReuse));
    }

    #[test]
    fn single_query_repeated_subexpression_is_a_reuse_finding() {
        // The same shared branch appearing twice within one query (an `a/a`
        // shape) — single-query CSE.
        let branch = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let q = QueryExpr::BinaryOp {
            op: asap_types::pre_asap::query_expr::BinaryOpKind::Arithmetic(
                asap_types::pre_asap::expr_ir::ArithmeticOpKind::Div,
            ),
            lhs: Rc::new(branch.clone()),
            rhs: Rc::new(branch),
            vector_match: None,
        };
        let findings = find_applicable_optimizations(vec![("ratio", q)]);
        let reuse: Vec<_> = findings
            .iter()
            .filter(|f| f.optimization == OptimizationKind::CommonSubexpressionReuse)
            .collect();
        assert_eq!(
            reuse.len(),
            1,
            "expected one reuse finding, got {findings:?}"
        );
        assert!(reuse[0].location.contains("lhs"));
        assert!(reuse[0].location.contains("rhs"));
    }

    /// A shared node nested three levels under two *different*, unshared
    /// `Filter` parents (mirrors `crate::search::tests::
    /// nested_shared_subtree_below_an_unshared_parent_is_still_discovered`)
    /// must still be exactly one finding — the maximal-`TargetSubDAG`
    /// guarantee the module docs describe, now provided by
    /// `crate::search::discover_sites` rather than this module's own
    /// (deleted) traversal.
    #[test]
    fn a_deeply_shared_subtree_under_different_parents_is_reported_once() {
        use asap_types::pre_asap::expr_ir::ScalarValue;
        use asap_types::pre_asap::query_expr::Predicate;

        let shared = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let root_a = QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Int64(1)))),
            child: Rc::new(shared.clone()),
        };
        let root_b = QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Int64(2)))),
            child: Rc::new(shared),
        };
        let findings = find_applicable_optimizations(vec![("a", root_a), ("b", root_b)]);
        let reuse: Vec<_> = findings
            .iter()
            .filter(|f| f.optimization == OptimizationKind::CommonSubexpressionReuse)
            .collect();
        assert_eq!(
            reuse.len(),
            1,
            "a node shared under two different parents must be one finding, got {findings:?}"
        );
        assert!(reuse[0].location.contains('a'));
        assert!(reuse[0].location.contains('b'));
    }

    // ── Custom strategy set / cost model plumbing ───────────────────────

    struct AlwaysDDSketch;
    impl crate::cost_model::CostModel for AlwaysDDSketch {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[asap_types::post_asap::SketchKind],
        ) -> Vec<asap_types::post_asap::SketchKind> {
            let mut v = candidates.to_vec();
            if let Some(pos) = v
                .iter()
                .position(|k| *k == asap_types::post_asap::SketchKind::DDSketch)
            {
                let dd = v.remove(pos);
                v.insert(0, dd);
            }
            v
        }
    }

    #[test]
    fn custom_cost_model_changes_the_reported_sketch_kind() {
        let q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let custom_model = AlwaysDDSketch;
        let strategies: Vec<Box<dyn ReplacementStrategy + '_>> = vec![Box::new(
            crate::replacement::SketchFamilyStrategy::new(&custom_model),
        )];
        let findings = find_applicable_optimizations_with(vec![("q", q)], &strategies);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].reason.to_lowercase().contains("ddsketch"));
    }
}
