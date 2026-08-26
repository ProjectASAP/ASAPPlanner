//! This crate's **explanation of a replacement**: for a `TargetSubDAG` that
//! [`crate::replacement::search_workload`] found something to say about, why
//! does that candidate exist? (issue #33: "Add logic to detect which
//! optimizations are applicable to a query workload"; this module: issue
//! #257.)
//!
//! This module does not answer "is optimization X applicable here, yes or
//! no" — that framing implies a classifier deciding admissibility from
//! scratch. What it actually does is narrower and more mechanical: reuse a
//! matching candidate's own [`crate::replacement::ReplacementSubDAG::rationale`]
//! to explain, in the candidate's own words, why a [`Replacement`] exists at
//! a given target. No new prose is invented here; see "The reframing" below
//! for exactly what's being reused and why.
//!
//! ## The reframing: an explanation *is* "this `TargetSubDAG`'s candidate
//! list is non-trivial"
//!
//! Earlier (PR #247, superseded by this module — see "What this replaces"
//! below), "is optimization X applicable here?" was a yes/no fact each rule
//! re-derived by walking the tree itself. That made sense before there was
//! any other structure to consult. But [`crate::replacement::search_workload`]
//! (issue #252) now *already* computes, for every
//! [`TargetSubDAG`](crate::replacement::TargetSubDAG) in the workload, every
//! semantically valid [`crate::replacement::ReplacementSubDAG`] a registered
//! [`ReplacementStrategy`] can propose — a [`PlanSpace`] of [`MemoGroup`]s. A
//! rule re-deriving the same yes/no fact from scratch would be answering a
//! question the search already answered, via a second, independently
//! maintained traversal that has to keep agreeing with the first one.
//!
//! Once that candidate space exists, "which optimizations are applicable"
//! collapses into a single question this module asks of *that* data instead:
//! **for a given `TargetSubDAG`, does its candidate list contain anything
//! other than the trivial, no-op realization?** A `TargetSubDAG` whose only
//! candidate is "the one thing `SketchAlgorithmStrategy` would have committed
//! to anyway, with no alternative" has no optimization to report — that
//! candidate isn't an *opportunity*, it's just the target's existing shape
//! reflected back. A `TargetSubDAG` with more than one candidate (several
//! sketch families to choose between), or one candidate that is itself a
//! genuine alternative to the status quo (share this already-shared subtree
//! instead of recomputing it at every consumer), *is* an applicability
//! finding — [`explain_replacements`] and
//! [`explain_replacements_with`] just translate [`PlanSpace`]'s
//! [`MemoGroup`]s into that shape:
//!
//! - [`ExplanationKind::SketchApproximation`] — the `TargetSubDAG`'s
//!   candidate list contains at least one [`Replacement::Summary`] that
//!   actually realizes a sketch family (`SummaryFamilyType::Sketch`), i.e.
//!   [`SketchAlgorithmStrategy`] found something to offer beyond whatever
//!   exact/pass-through candidate [`crate::replacement`]'s own
//!   `implementations_for_with` would have committed to on its own.
//! - [`ExplanationKind::CommonSubexpressionReuse`] — the `TargetSubDAG`
//!   has two or more consumers *and* its candidate list contains the
//!   [`SharedSubtreeStrategy`] "build once and share" candidate (the one
//!   whose `Rc` is the group's own `target`) — i.e. sharing this subtree
//!   instead of recomputing it independently is a real, reported choice, not
//!   just an accident of how the workload happened to be built.
//!
//! Each finding's `reason` is literally the matching candidate's own
//! [`crate::replacement::ReplacementSubDAG::rationale`] (joined, if more than one candidate
//! qualifies) — this module invents no new prose to explain *why* a
//! candidate is valid; that explanation already exists on the candidate a
//! [`ReplacementStrategy`] produced, and repeating it here (rather than
//! re-describing the same fact in different words) keeps exactly one place
//! that has to be right about "why is this a valid alternative".
//!
//! ## What this replaces, and what carries over unmodified
//!
//! [`ReplacementExplanation`] and [`ExplanationKind`] keep PR #247's original
//! shape and contract — a struct/enum pair meant for a downstream
//! DAG-visualization consumer, `#[non_exhaustive]` discipline (only an
//! optimization backed by a real, registered [`ReplacementStrategy`] gets a
//! variant; see the catalog table below for everything still deliberately
//! unrepresented). So do the two top-level entry points,
//! [`explain_replacements`] and [`explain_replacements_with`]
//! — same "workload roots in, findings out" contract, mirroring
//! [`crate::replacement::search_workload`]/[`crate::replacement::search_workload_with`]'s
//! own signature shape. Only the *data source* changed: this module now
//! calls those two functions and translates the result, rather than running
//! its own rules and their supporting traversal over the tree a second time.
//! All of that old traversal is deleted, not kept alongside the new
//! implementation — see "Two guarantees the old traversal made, re-verified"
//! below for the two properties it's important that deletion didn't quietly
//! lose.
//!
//! ## Why a second, applicability-specific rule trait doesn't exist here
//!
//! PR #247 gave this module its own extension-point trait, `ApplicabilityRule`
//! (`fn optimization(&self) -> ExplanationKind` + `fn evaluate(&self, roots)
//! -> Vec<ReplacementExplanation>`), the same shape [`crate::cost_model::CostModel`]
//! and [`crate::replacement::Matcher`] use elsewhere in this crate. Once
//! findings are a *view* over [`PlanSpace`] rather than an independent
//! computation, that trait would be a second extension point answering a
//! question [`ReplacementStrategy`] (issue #251) already answers: "does this
//! `TargetSubDAG` have an alternative worth reporting, and why". A caller who
//! wants a new optimization represented as a finding needs a new
//! `impl ReplacementStrategy` wired into
//! [`crate::replacement::search_workload_with`]'s strategy set *regardless*
//! (that's the only way its candidates end up in the [`PlanSpace`] this
//! module reads) — adding an `ApplicabilityRule` too would mean maintaining
//! two extension points for the same new capability, one of which (the rule)
//! would just be re-describing candidates the other (the strategy) already
//! produced. So this module ships no extension-point trait of its own:
//! [`ReplacementStrategy`] already *is* that extension point, one layer
//! down, and [`explain_replacements_with`]'s own `strategies`
//! parameter is where a caller plugs in a custom one (or a custom
//! `CostModel`, via [`crate::replacement::SketchAlgorithmStrategy::new`]) — the identical spot
//! [`crate::replacement::search_workload_with`] itself exposes.
//!
//! ## Two guarantees the old traversal made, re-verified against the new one
//!
//! 1. **A finding is reported at the maximal `TargetSubDAG`, never once more
//!    per subsumed descendant.** [`crate::replacement`]'s own
//!    `discover_targets` (used by [`crate::replacement::search_workload_with`],
//!    and so by this module) walks every workload root's whole DAG but only
//!    *recurses into a node's children the first time that node's `Rc` is
//!    seen*; every subsequent occurrence still counts towards
//!    `consumer_count`, but never triggers a second descent. A node nested
//!    under an already-discovered shared ancestor therefore only becomes its
//!    own `TargetSubDAG` if something *outside* that ancestor also
//!    references it — identical to PR #247's own discovery pass, which
//!    reported "the highest point sharing starts," not a finding at every
//!    subsumed level below it. Same guarantee, same mechanism, just living in
//!    [`crate::replacement`] now instead of here.
//! 2. **A node reachable via more than one path is one finding, not one per
//!    path.** [`MemoGroup`]s are keyed by `Rc` pointer identity in
//!    [`PlanSpace`]'s internal map — there is exactly one group per distinct
//!    `Rc`, full stop, so a shared `Aggregate` reached via two different
//!    `BinaryOp` branches (or two different workload roots) is exactly one
//!    group, hence at most one [`ExplanationKind::SketchApproximation`]
//!    finding, no matter how many paths reach it.
//!    [`tests::a_shared_sketchable_aggregate_is_reported_only_once`] pins
//!    this directly.
//!
//! ## One thing [`PlanSpace`] doesn't carry that this module still needs:
//! human-readable `location` text
//!
//! [`MemoGroup`]/[`PlanSpace`] deliberately track only `Rc<QueryExpr>`
//! pointer identity — the currency the search itself needs — not
//! caller-facing prose. [`ReplacementExplanation::location`] is prose (a
//! breadcrumb like `root "dash_a" > lhs`), so this module keeps one small,
//! self-contained walk of its own, [`collect_locations`], whose *only* job
//! is turning "this `Rc`" into "the human-readable place(s) it occurs" for a
//! finding already decided by [`PlanSpace`]. This is not a reincarnation of
//! the deleted rule traversal: it makes no applicability decision (it runs
//! the same regardless of what any strategy found), and duplicating this
//! small, self-contained shape rather than threading location strings
//! through [`crate::replacement`]'s own `discover_targets` matches the same
//! call that module's own docs already make for its (test-only)
//! `count_consumers` counterpart — see [`crate::replacement`]'s "Where
//! `TargetSubDAG` discovery comes from" section.
//!
//! ## Catalog primitives deliberately left as future work
//!
//! The internal catalog
//! (`ProjectASAP/internal-docs/catalog_of_optimizations.md`) lists several
//! primitives with **no [`ReplacementStrategy`] implementation anywhere in
//! this codebase today**. Faking a variant for one of them would report a
//! finding this codebase cannot back with a real candidate, so none of the
//! below get an [`ExplanationKind`] variant yet — each gets one once a real
//! strategy exists and is wired into [`crate::replacement::default_strategies`]:
//!
//! | Catalog entry | Status | Where a future `ExplanationKind` would come from |
//! |---|---|---|
//! | Semantic-equivalent rewriting (e.g. `avg` → `sum`/`count`) | [`AvgToSumOverCountStrategy`](crate::rewrite::AvgToSumOverCountStrategy) exists and is wired into `default_strategies()` (issue #253) — but still no `ExplanationKind` of its own below, since this table is about *direct* findings for a catalog entry, and this strategy's whole point is indirect: its `Replacement::Rewrite` candidate exposes `sum`/`count` as independently bindable discovered targets, which can then earn `CommonSubexpressionReuse` findings when the workload actually reuses them | A dedicated variant would need `findings_from_plan_space` to recognize a `LogicalRewrite`-provenance candidate as a finding in its own right, not just rely on what it exposes downstream |
//! | Roll-ups (fine-to-coarse group-by reuse) | [`RollupStrategy`](crate::rollup::RollupStrategy), derived from workload siblings after CSE/target discovery (issue #254) | Any `Replacement::Rewrite` candidate that rolls a coarse aggregate up from a compatible finer aggregate |
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
//! [`SketchAlgorithmStrategy`]: crate::replacement::SketchAlgorithmStrategy
//! [`SharedSubtreeStrategy`]: crate::replacement::SharedSubtreeStrategy
//! [`PlanSpace`]: crate::replacement::PlanSpace
//! [`MemoGroup`]: crate::replacement::MemoGroup

use std::collections::HashMap;
use std::fmt::Display;
use std::rc::Rc;

use asap_types::post_asap::{SummaryExpr, SummaryFamilyType, SummaryNode};
use asap_types::pre_asap::cse::{structural_hash, HashCache};
use asap_types::pre_asap::query_expr::QueryExpr;

use crate::replacement::{self, MemoGroup, PlanSpace, Replacement, ReplacementStrategy};

/// Which kind of replacement a [`ReplacementExplanation`] is about.
///
/// `#[non_exhaustive]`: only optimizations with a real [`ReplacementStrategy`]
/// behind them get a variant (see the module docs' "Catalog primitives
/// deliberately left as future work" table for everything else in the
/// catalog).
///
/// [`ReplacementStrategy`]: crate::replacement::ReplacementStrategy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExplanationKind {
    /// A `TargetSubDAG`'s candidate list contains at least one
    /// [`Replacement::Summary`] that realizes a sketch family —
    /// [`crate::replacement::SketchAlgorithmStrategy`] found a genuine sketch
    /// alternative for this `Aggregate`, beyond whatever exact/pass-through
    /// candidate `crate::replacement`'s own `implementations_for_with` would
    /// have committed to on its own.
    SketchApproximation,
    /// A `TargetSubDAG` has two or more consumers *and* its candidate list
    /// contains [`crate::replacement::SharedSubtreeStrategy`]'s "build once
    /// and share" candidate — the catalog's cross-statistic / cross-metrics /
    /// cross-subpopulation reuse entries, all the same underlying structural
    /// fact.
    CommonSubexpressionReuse,
}

/// Why a [`Replacement`] of `kind` exists at `location` (a human-readable
/// breadcrumb into the workload — e.g. `root "dashboard_p99"` or
/// `root "ratio" > lhs`): `reason` (human-readable, meant for a report/log,
/// not machine parsing — literally the matching candidate's own
/// [`crate::replacement::ReplacementSubDAG::rationale`]).
///
/// `node_hash` is [`structural_hash`](asap_types::pre_asap::cse::structural_hash)
/// of the `TargetSubDAG`'s own `target` subtree — the same function, on the
/// same `Rc<QueryExpr>` shape, that [`asap_types::dag_export::DagNode::hash`]
/// is computed with. A downstream consumer that independently exported the
/// same `QueryExpr` (e.g. via `asap_types::dag_export::export`) can match
/// this explanation to a `DagNode` by first comparing hashes and then
/// confirming structural equality with [`ReplacementExplanation::target`].
#[derive(Debug, Clone, PartialEq)]
pub struct ReplacementExplanation {
    pub kind: ExplanationKind,
    pub location: String,
    pub reason: String,
    pub node_hash: u64,
    /// The exact target expression the explanation describes. Reporting
    /// integrations use this together with `node_hash`: the hash narrows the
    /// search, and structural equality makes the final match collision-safe.
    pub target: Rc<QueryExpr>,
}

/// Explain every replacement [`crate::replacement::search_workload`] finds
/// across a workload's pre-ASAP query roots, using
/// [`crate::replacement::default_strategies`].
///
/// `roots` — like [`crate::replacement::search_workload`]'s own `Id` type
/// parameter — is caller-chosen: a `QueryWorkload` entry's own key, an index,
/// a query name. It only needs [`Display`], since a finding's `location` is
/// prose, not a structured key back to the caller.
///
/// Internally runs [`crate::replacement::search_workload`] to build the
/// candidate-plan space, then reads findings off it — see the module docs'
/// "The reframing" section for what that translation actually checks.
pub fn explain_replacements<Id: Display>(
    roots: Vec<(Id, QueryExpr)>,
) -> Vec<ReplacementExplanation> {
    explain_replacements_with(roots, &replacement::default_strategies())
}

/// Like [`explain_replacements`], but searches with `strategies`
/// instead of [`crate::replacement::default_strategies`] — the extension
/// point for a deployment-specific [`ReplacementStrategy`], or a custom
/// `CostModel` plugged into
/// [`crate::replacement::SketchAlgorithmStrategy::new`] (e.g. via
/// [`crate::replacement::default_strategies_with`]).
///
/// [`ReplacementStrategy`]: crate::replacement::ReplacementStrategy
pub fn explain_replacements_with<'s, Id: Display>(
    roots: Vec<(Id, QueryExpr)>,
    strategies: &[Box<dyn ReplacementStrategy + 's>],
) -> Vec<ReplacementExplanation> {
    let ided: Vec<(String, Rc<QueryExpr>)> = roots
        .into_iter()
        .map(|(id, expr)| (id.to_string(), Rc::new(expr)))
        .collect();
    let space = replacement::search_workload_with(ided, strategies);
    findings_from_plan_space(&space)
}

/// Translate every discovered [`MemoGroup`] in `space` into zero, one, or two
/// [`ReplacementExplanation`]s (a `TargetSubDAG` can be both sketch-approximable
/// *and* shared — the two optimizations are independent axes, not mutually
/// exclusive).
///
/// `space`'s own `Id` is always `String` here: [`explain_replacements_with`]
/// already converted the caller's `Id: Display` into a `String` (via
/// `to_string()`) before calling [`crate::replacement::search_workload_with`],
/// so this function (and [`collect_locations`], which formats `id` with
/// [`std::fmt::Debug`] for the breadcrumb text) doesn't need its own generic
/// `Id` bound.
fn findings_from_plan_space(space: &PlanSpace<String>) -> Vec<ReplacementExplanation> {
    let locations = collect_locations(&space.roots);
    // One cache for the whole pass, mirroring `dag_export::export`'s own
    // `HashCache` reuse — this is a bottom-up pass over every discovered
    // group, so amortizing the cache across groups (rather than resetting it
    // per group) is real, not just a micro-optimization.
    let mut hash_cache = HashCache::new();
    let mut findings = Vec::new();
    for group in space.groups() {
        let location = locations
            .get(&Rc::as_ptr(&group.target))
            .map(|locs| locs.join(", "))
            .unwrap_or_default();
        let node_hash = structural_hash(&group.target, &mut hash_cache);

        if let Some(reason) = sketch_finding_reason(group) {
            findings.push(ReplacementExplanation {
                kind: ExplanationKind::SketchApproximation,
                location: location.clone(),
                reason,
                node_hash,
                target: Rc::clone(&group.target),
            });
        }
        if let Some(reason) = shared_subexpr_finding_reason(group) {
            findings.push(ReplacementExplanation {
                kind: ExplanationKind::CommonSubexpressionReuse,
                location,
                reason,
                node_hash,
                target: Rc::clone(&group.target),
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
/// [`crate::replacement`]'s own private `sketch_kind_of` unwraps) ultimately
/// realize a [`SummaryFamilyType::Sketch`] family? This module only needs the
/// yes/no fact (a candidate's own `rationale` already names the specific
/// `SketchKind`/`SketchAlgorithm` for a finding's `reason` text), so unlike
/// `replacement.rs`'s counterpart this returns `bool`, not the kind itself.
fn is_sketch_realization(node: &SummaryNode) -> bool {
    match &node.expr {
        SummaryExpr::SummaryEstimate { summary_input, .. } => is_sketch_realization(summary_input),
        SummaryExpr::SummaryAgg { family, .. } => matches!(family, SummaryFamilyType::Sketch(..)),
        _ => false,
    }
}

// ── location breadcrumbs ─────────────────────────────────────────────────

/// Build `location` text for every distinct `TargetSubDAG` reachable from
/// `roots` — see the module docs' "One thing `PlanSpace` doesn't carry"
/// section for why this module needs its own small walk for this. Returns
/// every breadcrumb path that reaches a given `Rc`, not just the first: a
/// shared node referenced from two workload roots (or two branches of one
/// root) needs both breadcrumbs in its finding's `location`, not just one.
fn collect_locations(roots: &[(String, Rc<QueryExpr>)]) -> HashMap<*const QueryExpr, Vec<String>> {
    let mut locations: HashMap<*const QueryExpr, Vec<String>> = HashMap::new();
    for (id, root) in roots {
        visit(root, format!("root {id:?}"), &mut locations);
    }
    locations
}

/// Record `label` as one of `node`'s breadcrumbs, then propagate that path
/// through its children. A shared ancestor is intentionally traversed once
/// per incoming path so every descendant receives every valid breadcrumb.
fn visit(
    node: &Rc<QueryExpr>,
    label: String,
    locations: &mut HashMap<*const QueryExpr, Vec<String>>,
) {
    let ptr = Rc::as_ptr(node);
    locations.entry(ptr).or_default().push(label.clone());
    visit_children(node, &label, locations);
}

/// `node`'s own **relational-skeleton** operator children — the same scope
/// `crate::replacement`'s own target-discovery `walk_children` (and
/// `asap_types::pre_asap::cse::share_common_subtrees`'s `rebuild_children`)
/// use. Exhaustive over every `QueryExpr` variant: a new variant fails to
/// compile here until this match is extended too.
fn visit_children(
    node: &QueryExpr,
    label: &str,
    locations: &mut HashMap<*const QueryExpr, Vec<String>>,
) {
    use QueryExpr::*;
    match node {
        Scan { .. } | PromqlScalarBridge(_) | EvalTimestamp | CurrentTimestamp => {}
        PromqlVectorFromScalar(c) | PromqlScalarFromVector(c) => {
            visit(c, format!("{label} > child"), locations)
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
        | Limit { child, .. } => visit(child, format!("{label} > child"), locations),
        Concat { children } => {
            for (i, c) in children.iter().enumerate() {
                visit_children(c, &format!("{label} > concat[{i}]"), locations);
            }
        }
        Join { left, right, .. } | SetOp { left, right, .. } => {
            visit(left, format!("{label} > left"), locations);
            visit(right, format!("{label} > right"), locations);
        }
        BinaryOp { lhs, rhs, .. } => {
            visit(lhs, format!("{label} > lhs"), locations);
            visit(rhs, format!("{label} > rhs"), locations);
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
        let findings = explain_replacements(vec![("dashboard_p99", q)]);
        let sketch: Vec<_> = findings
            .iter()
            .filter(|f| f.kind == ExplanationKind::SketchApproximation)
            .collect();
        assert_eq!(
            sketch.len(),
            1,
            "expected one sketch finding, got {findings:?}"
        );
        assert!(sketch[0].location.contains("dashboard_p99"));
        assert!(sketch[0].reason.to_lowercase().contains("kll"));
    }

    /// `node_hash` must be the literal `structural_hash` a downstream
    /// consumer would compute over the *same* `QueryExpr` subtree via
    /// `asap_types::dag_export::export` — the whole point of carrying it is
    /// that two independent exports of the same tree agree, with no
    /// string-matching against `location` required.
    #[test]
    fn node_hash_matches_dag_export_hash_for_the_same_subtree() {
        let q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let graph = asap_types::dag_export::export(&q);
        let expected_hash = graph.nodes[graph.root as usize].hash;

        let findings = explain_replacements(vec![("dashboard_p99", q)]);
        let sketch = findings
            .iter()
            .find(|f| f.kind == ExplanationKind::SketchApproximation)
            .expect("expected a sketch finding");
        assert_eq!(
            sketch.node_hash, expected_hash,
            "ReplacementExplanation::node_hash must match dag_export's DagNode::hash \
             for the same QueryExpr subtree"
        );
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
        let findings = explain_replacements(vec![("exact_p99", q)]);
        assert!(
            findings
                .iter()
                .all(|f| f.kind != ExplanationKind::SketchApproximation),
            "an Exact accuracy target must not report sketch-applicability, got {findings:?}"
        );
    }

    #[test]
    fn nested_aggregate_still_finds_the_inner_sketchable_node() {
        // avg(quantile(0.9, sum by (job) (m))) shaped test isn't representable
        // (avg is PassThrough, not a wrapper we recurse through structurally
        // the way an Aggregate's own child is) — instead nest a sketchable
        // quantile under an exact sum, the same nesting replacement.rs's own
        // nested_aggregates_bind_per_node test uses.
        let inner = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let outer = agg(vec![], default_quantile(0.9), inner);
        let findings = explain_replacements(vec![("q", outer)]);
        let sketch_count = findings
            .iter()
            .filter(|f| f.kind == ExplanationKind::SketchApproximation)
            .count();
        assert_eq!(
            sketch_count, 1,
            "expected the outer quantile only, got {findings:?}"
        );
    }

    #[test]
    fn pass_through_intent_reports_no_sketch_finding() {
        let q = agg(vec![2], AggIntent::Avg { col: None }, metric_scan(&["job"]));
        let findings = explain_replacements(vec![("avg_latency", q)]);
        assert!(findings
            .iter()
            .all(|f| f.kind != ExplanationKind::SketchApproximation));
    }

    /// A sketch-applicable `Aggregate` reachable via two paths that CSE
    /// collapses onto one `Rc` — the same `median(x) == median(x)` shape
    /// `pre_asap::cse`'s own `single_query_shares_its_own_repeated_subtree`
    /// test uses — must be reported once, not once per path: it is exactly
    /// one [`crate::replacement::MemoGroup`], keyed by `Rc` pointer identity,
    /// not one per path that reaches it.
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
        let findings = explain_replacements(vec![("ratio", root)]);
        let sketch: Vec<_> = findings
            .iter()
            .filter(|f| f.kind == ExplanationKind::SketchApproximation)
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
        let findings = explain_replacements(vec![("dash_a", a), ("dash_b", b)]);
        let reuse: Vec<_> = findings
            .iter()
            .filter(|f| f.kind == ExplanationKind::CommonSubexpressionReuse)
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
    fn descendant_of_a_shared_root_keeps_every_root_breadcrumb() {
        let inner = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let outer = agg(vec![2], AggIntent::Sum { col: None }, inner);
        let findings = explain_replacements(vec![("dash_a", outer.clone()), ("dash_b", outer)]);
        let inner_sketch = findings
            .iter()
            .find(|f| {
                f.kind == ExplanationKind::SketchApproximation && f.location.contains("child")
            })
            .expect("expected the nested sketch explanation");
        assert!(inner_sketch.location.contains("dash_a"));
        assert!(inner_sketch.location.contains("dash_b"));
    }

    #[test]
    fn distinct_queries_report_no_reuse_finding() {
        let a = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let b = agg(
            vec![2],
            AggIntent::Sum { col: None },
            metric_scan(&["route"]),
        );
        let findings = explain_replacements(vec![("dash_a", a), ("dash_b", b)]);
        assert!(
            findings
                .iter()
                .all(|f| f.kind != ExplanationKind::CommonSubexpressionReuse),
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
        let findings = explain_replacements(vec![("a", a), ("b", b)]);
        assert!(findings
            .iter()
            .all(|f| f.kind != ExplanationKind::CommonSubexpressionReuse));
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
        let findings = explain_replacements(vec![("ratio", q)]);
        let reuse: Vec<_> = findings
            .iter()
            .filter(|f| f.kind == ExplanationKind::CommonSubexpressionReuse)
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
    /// `Filter` parents (mirrors `crate::replacement::tests::
    /// nested_shared_subtree_below_an_unshared_parent_is_still_discovered`)
    /// must still be exactly one finding — the maximal-`TargetSubDAG`
    /// guarantee the module docs describe, now provided by
    /// `crate::replacement`'s own target discovery rather than this module's
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
        let findings = explain_replacements(vec![("a", root_a), ("b", root_b)]);
        let reuse: Vec<_> = findings
            .iter()
            .filter(|f| f.kind == ExplanationKind::CommonSubexpressionReuse)
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
            candidates: &[asap_types::post_asap::SketchAlgorithm],
        ) -> Vec<asap_types::post_asap::SketchAlgorithm> {
            let mut v = candidates.to_vec();
            if let Some(pos) = v
                .iter()
                .position(|k| *k == asap_types::post_asap::SketchAlgorithm::DDSketch)
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
            crate::replacement::SketchAlgorithmStrategy::new(&custom_model),
        )];
        let findings = explain_replacements_with(vec![("q", q)], &strategies);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].reason.to_lowercase().contains("ddsketch"));
    }
}
