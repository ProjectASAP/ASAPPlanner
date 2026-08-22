//! Optimization-applicability rules over a workload's pre-ASAP query roots
//! (issue #33: "Add logic to detect which optimizations are applicable to a
//! query workload").
//!
//! [`ApplicabilityRule`] is a small extension-point trait, the same shape as
//! [`CostModel`](crate::cost_model::CostModel) and [`Matcher`](crate::boundary::Matcher)
//! elsewhere in this crate: a new optimization gets a new rule implementation
//! without restructuring anything already here. [`find_applicable_optimizations`]
//! runs the built-in rule set ([`default_rules`]) over a workload's roots and
//! returns every positive [`ApplicabilityFinding`] — the optimization kind,
//! which workload location it applies to, and a human-readable reason.
//!
//! ## Two real rules, wrapping code that already decides applicability
//!
//! This module deliberately implements **no new decision procedure**. Both
//! rules below just reframe an existing, already-correct pass as an
//! applicability finding:
//!
//! - [`SketchApplicabilityRule`] — wraps [`boundary::implementation_for_with`]
//!   (the sketch-vs-exact boundary, issue #98). For every bindable `Aggregate`
//!   node (the same single-intent, no-`HAVING` shape [`bind::implement_tree`]
//!   itself requires) whose realization comes back
//!   [`Implementation::Sketch`], that *is* "sketch-approximation is
//!   applicable here" — the boundary decision already answers exactly this
//!   question, per node; this rule only collects and narrates it.
//! - [`SharedSubexpressionRule`] — wraps
//!   [`asap_types::pre_asap::cse::share_common_subtrees`] (issue #212/#222/#223).
//!   CSE's hash-consing already decides, correctly and conservatively (see
//!   that module's "Correctness"/"Legality" sections), which subtrees across
//!   a workload's roots are safe to share. Two or more locations ending up
//!   with the same `Rc<QueryExpr>` after that pass *is* a positive
//!   applicability finding for the catalog's "cross-statistic reuse",
//!   "cross-metrics reuse", and "cross-subpopulation reuse" entries — they
//!   all reduce to the same structural question (does more than one logical
//!   query need end up served by one shared computation?), which CSE's own
//!   detection already answers. This rule does not re-derive sharing
//!   legality or structural equality; it only observes where
//!   `share_common_subtrees` already aliased an `Rc`.
//!
//! [`find_applicable_optimizations`] runs `share_common_subtrees` itself
//! (once, shared by every rule) before evaluating any rule, so
//! `SketchApplicabilityRule` also runs against the already-CSE'd tree —
//! harmless for it (the boundary decision is per-node and doesn't care
//! whether its `Rc` is aliased), and exactly the input shape
//! [`SharedSubexpressionRule`] needs.
//!
//! ## Catalog primitives deliberately left as future work
//!
//! The internal catalog
//! (`ProjectASAP/internal-docs/catalog_of_optimizations.md`) lists several
//! primitives with **no implementation anywhere in this codebase today**.
//! Faking a rule for one of them would report a finding this codebase cannot
//! back up, so none of the below get an [`OptimizationKind`] variant yet.
//! [`OptimizationKind`] is `#[non_exhaustive]` for exactly this reason — each
//! gets a variant + a rule once its primitive exists for real:
//!
//! | Catalog entry | Status | Where a future rule would hook in |
//! |---|---|---|
//! | Wavelets/OMP | Params type exists ([`WaveletKind`]/[`WaveletParams`]), reachable only via a deployment `CostModel::realize_extension` (no core `AggIntent` dispatch picks it) | Extend [`boundary::implementation_for`]'s dispatch (or add a rule that inspects a deployment's own `CostModel`) once some intent shape actually maps to `Implementation::Wavelet` |
//! | Sampling | Same story as Wavelets: [`SamplingKind`]/[`SamplingParams`] exist, unreachable from core dispatch | Same hook as Wavelets, for `Implementation::Sample` |
//! | Deep generative compression | No representation at all — no `Implementation`/`SummaryFamilyType` variant | Needs a new summary family added to `asap_types::post_asap` first |
//! | Roll-ups | No representation — `QueryExpr::Scan`'s `Source` has no pre-aggregated/rollup leaf variant | A future `Source` variant, matched here the same way `SketchApplicabilityRule` matches `Implementation::Sketch` |
//! | Approximation frameworks for windows | No representation — `TimeRange`/`PromqlSubquery` windows are always evaluated exactly | Would key off those node types once an approximate-window operator exists |
//! | Function decomposition | No representation anywhere | No hook point identified yet |
//! | Continuous distributed monitoring | No representation — `RepeatingEntry`/`RepetitionInterval` in [`asap_types::workload`] describe *that* a query repeats, not any monitoring-specific decomposition | Would likely key off `RepeatingEntry` once such logic exists |
//! | Incremental computation across time | No representation — nothing carries state across repeated evaluations of a `RepeatingEntry` today | Would key off `RepeatingEntry` + `TimeShift`/`TimeRange` once incremental state-carry exists |
//! | Delta encoding | No representation — `AggIntent::Delta`/`IDelta` are PromQL *value*-difference semantics, not a wire/storage delta-encoding optimization; [`asap_types::workload::DataDistribution`]'s doc already discusses delta-compression benefit as a function of key distribution, but nothing computes or applies it | Would plug into a future deployment-side wire/storage encoding decision (post-ASAP), not this crate's IR-level dispatch |
//!
//! [`WaveletKind`]: asap_types::post_asap::WaveletKind
//! [`WaveletParams`]: asap_types::post_asap::WaveletParams
//! [`SamplingKind`]: asap_types::post_asap::SamplingKind
//! [`SamplingParams`]: asap_types::post_asap::SamplingParams
//! [`boundary`]: crate::boundary
//! [`bind`]: crate::bind

use std::collections::HashMap;
use std::fmt::Display;
use std::rc::Rc;

use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::cse::share_common_subtrees;
use asap_types::pre_asap::query_expr::QueryExpr;

use crate::boundary::{implementation_for_with, Implementation};
use crate::cost_model::{CostModel, DefaultCostModel};

/// Which optimization an [`ApplicabilityFinding`] is about.
///
/// `#[non_exhaustive]`: only optimizations with a real rule behind them get a
/// variant (see the module docs' "deliberately left as future work" table
/// for everything else in the catalog).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OptimizationKind {
    /// An `AggIntent` is realizable as an approximate sketch (the
    /// [`boundary`](crate::boundary) sketch-vs-exact decision came back
    /// [`Implementation::Sketch`]).
    SketchApproximation,
    /// Two or more workload locations end up sharing one interned subtree
    /// after [`share_common_subtrees`] — the catalog's cross-statistic /
    /// cross-metrics / cross-subpopulation reuse entries, all the same
    /// underlying structural fact.
    SharedSubexpressionReuse,
}

/// One positive applicability result: `optimization` is applicable at
/// `location` (a caller-meaningful description of where in the workload —
/// e.g. `root "dashboard_p99"` or a small breadcrumb path into its tree),
/// for `reason` (human-readable, meant for a report/log, not machine parsing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicabilityFinding {
    pub optimization: OptimizationKind,
    pub location: String,
    pub reason: String,
}

/// An applicability rule: given a workload's already-CSE'd pre-ASAP query
/// roots, report every place the rule's optimization applies.
///
/// The extension point this module exists for — matches the shape of
/// [`CostModel`] and [`Matcher`](crate::boundary::Matcher) elsewhere in this
/// crate. A new optimization gets a new `impl ApplicabilityRule`, added to
/// (or used alongside, via [`find_applicable_optimizations_with`]) the
/// built-in [`default_rules`] set, without touching this trait or any
/// existing rule.
pub trait ApplicabilityRule {
    /// The optimization this rule detects.
    fn optimization(&self) -> OptimizationKind;

    /// Evaluate this rule against `roots` — a workload's pre-ASAP query
    /// roots, each already run through `share_common_subtrees` (see
    /// [`find_applicable_optimizations`]), labeled by a caller-chosen
    /// `location` string identifying which workload entry each root came
    /// from.
    fn evaluate(&self, roots: &[(String, Rc<QueryExpr>)]) -> Vec<ApplicabilityFinding>;
}

/// Wraps [`boundary::implementation_for_with`](crate::boundary::implementation_for_with):
/// reports "sketch-approximation is applicable" for every bindable
/// `Aggregate` node whose boundary decision is [`Implementation::Sketch`].
///
/// Ranks candidate summaries via a `CostModel` — [`DefaultCostModel`] unless
/// constructed with [`SketchApplicabilityRule::new`] — exactly like
/// [`bind::implement_tree_with`](crate::bind::implement_tree_with) does, so a
/// deployment-specific cost model sees the same candidate ranking here as it
/// would at actual binding time.
pub struct SketchApplicabilityRule<'a> {
    cost_model: &'a dyn CostModel,
}

impl SketchApplicabilityRule<'static> {
    /// A rule that ranks candidates via the built-in [`DefaultCostModel`],
    /// matching what a deployment gets from
    /// [`bind::implement_tree`](crate::bind::implement_tree) (no custom cost
    /// model plugged in).
    pub fn default_cost_model() -> Self {
        Self {
            cost_model: &DEFAULT_COST_MODEL,
        }
    }
}

/// A single static instance so [`SketchApplicabilityRule::default_cost_model`]
/// can hand out a `&'static dyn CostModel` without heap-allocating one.
/// `DefaultCostModel` is a unit struct with no state, so one instance serves
/// every caller.
static DEFAULT_COST_MODEL: DefaultCostModel = DefaultCostModel;

impl<'a> SketchApplicabilityRule<'a> {
    /// A rule that ranks candidates via `cost_model` instead of the built-in
    /// static preference order — the same customization point
    /// [`bind::implement_tree_with`](crate::bind::implement_tree_with) and
    /// [`boundary::implementation_for_with`](crate::boundary::implementation_for_with)
    /// already offer.
    pub fn new(cost_model: &'a dyn CostModel) -> Self {
        Self { cost_model }
    }
}

impl ApplicabilityRule for SketchApplicabilityRule<'_> {
    fn optimization(&self) -> OptimizationKind {
        OptimizationKind::SketchApproximation
    }

    fn evaluate(&self, roots: &[(String, Rc<QueryExpr>)]) -> Vec<ApplicabilityFinding> {
        let mut findings = Vec::new();
        for (id, root) in roots {
            collect_sketch_findings(
                root,
                &format!("root {id:?}"),
                self.cost_model,
                &mut findings,
            );
        }
        findings
    }
}

/// Walk `node` looking for bindable `Aggregate`s (the same single-intent,
/// no-`HAVING` shape [`bind::implement_tree_with`](crate::bind::implement_tree_with)
/// itself requires — a multi-intent or `HAVING`-bearing `Aggregate`, or one
/// under a logical parent that would subsume it, stays logical at actual
/// binding time too, so it is not reported here either), recursing through
/// every operator position a nested `Aggregate` could appear in.
fn collect_sketch_findings(
    node: &QueryExpr,
    location: &str,
    cost_model: &dyn CostModel,
    findings: &mut Vec<ApplicabilityFinding>,
) {
    if let QueryExpr::Aggregate {
        measures, having, ..
    } = node
    {
        if let ([intent], None) = (measures.as_slice(), having) {
            if let Implementation::Sketch { kind, .. } = implementation_for_with(intent, cost_model)
            {
                findings.push(ApplicabilityFinding {
                    optimization: OptimizationKind::SketchApproximation,
                    location: location.to_string(),
                    reason: format!(
                        "{} realizes as a {kind:?} sketch under its accuracy target \
                         (asap_aware_mapping::boundary::implementation_for)",
                        describe_intent(intent),
                    ),
                });
            }
        }
    }
    for_each_operator_child(node, location, |child, child_location| {
        collect_sketch_findings(child, &child_location, cost_model, findings);
    });
}

/// A short human-readable label for an `AggIntent`, for
/// [`ApplicabilityFinding::reason`] text. Not exhaustive by design (unlike
/// the crate's other `AggIntent` matches) — this is prose, not a decision,
/// so an unlisted variant just falls back to its `Debug` tag rather than
/// forcing every future intent to be named here too.
fn describe_intent(intent: &AggIntent) -> String {
    match intent {
        AggIntent::Quantile { q, .. } => format!("quantile(q={q})"),
        AggIntent::Cardinality { .. } => "cardinality (distinct count)".to_string(),
        AggIntent::TopK { k, .. } => format!("top-{k} heavy-hitters"),
        AggIntent::Count { .. } => "count".to_string(),
        other => format!("{other:?}"),
    }
}

/// Wraps [`share_common_subtrees`]: reports "shared-subexpression reuse is
/// applicable" wherever two or more workload locations end up referencing
/// the same interned `Rc<QueryExpr>` — whether that's two different roots
/// (cross-statistic / cross-metric / cross-subpopulation reuse across the
/// batch) or the same root twice (a repeated subexpression within one
/// query, e.g. `a / a`).
///
/// This rule does not decide sharing itself — by the time
/// [`ApplicabilityRule::evaluate`] runs, `roots` already went through
/// `share_common_subtrees` (see [`find_applicable_optimizations`]), which
/// made every sharing/legality decision. This rule only observes where that
/// pass already aliased an `Rc`.
pub struct SharedSubexpressionRule;

impl ApplicabilityRule for SharedSubexpressionRule {
    fn optimization(&self) -> OptimizationKind {
        OptimizationKind::SharedSubexpressionReuse
    }

    fn evaluate(&self, roots: &[(String, Rc<QueryExpr>)]) -> Vec<ApplicabilityFinding> {
        let mut sites: HashMap<usize, Vec<String>> = HashMap::new();
        for (id, root) in roots {
            register_site(root, &format!("root {id:?}"), &mut sites);
        }
        let mut findings: Vec<ApplicabilityFinding> = sites
            .into_values()
            .filter(|locations| locations.len() >= 2)
            .map(|locations| {
                let count = locations.len();
                ApplicabilityFinding {
                    optimization: OptimizationKind::SharedSubexpressionReuse,
                    location: locations.join(", "),
                    reason: format!(
                        "share_common_subtrees interned this subtree once and reused it across \
                         {count} locations — one build can answer all of them instead of \
                         computing it {count} times"
                    ),
                }
            })
            .collect();
        findings.sort_by(|a, b| a.location.cmp(&b.location));
        findings
    }
}

/// Register `node`'s own `Rc` identity under `location`, then walk its
/// operator children — unless `node`'s pointer has already been registered
/// (from an earlier root, or an earlier branch of this same root): its
/// subtree was already walked in full on that first visit, and being the
/// same `Rc` it cannot have changed, so re-walking it here would only
/// re-report every descendant a second time as a *nested*, subsumed
/// "finding" (the whole subtree is already shared because this node is).
/// Skipping the re-walk keeps every reported finding maximal — the
/// highest point in the tree at which sharing starts — rather than one
/// finding per shared node down the entire shared subtree.
fn register_site(node: &Rc<QueryExpr>, location: &str, sites: &mut HashMap<usize, Vec<String>>) {
    let ptr = Rc::as_ptr(node) as usize;
    let already_visited = sites.contains_key(&ptr);
    sites.entry(ptr).or_default().push(location.to_string());
    if !already_visited {
        walk_rc_children(node, location, sites);
    }
}

/// Visit `node`'s operator children (the same relational-skeleton scope
/// `pre_asap::canonicalize`'s `children_mut` and `pre_asap::cse`'s
/// `rebuild_children` use — see those modules' docs), registering each one
/// reached via an `Rc<QueryExpr>` field. `Concat`'s branches are the one
/// operator position stored by value instead (`Vec<QueryExpr>`, never itself
/// `Rc`-aliased — see `pre_asap::cse`'s module doc on why), so a branch is
/// walked structurally without registering a site for itself, while any
/// `Rc`-typed descendant beneath it still gets registered.
///
/// A deliberately independent traversal (same status as `dag_export`'s own
/// `structural_hash` next to `pre_asap::cse`'s — see that module's doc on
/// unifying these being future work, not attempted here): this crate depends
/// only on `asap_types`, never on that private helper.
fn walk_rc_children(node: &QueryExpr, location: &str, sites: &mut HashMap<usize, Vec<String>>) {
    use QueryExpr::*;
    match node {
        Scan { .. } | PromqlScalar(_) | QueryTimestamp => {}
        PromqlVectorFromScalar(c) | PromqlScalarFromVector(c) => {
            register_site(c, &format!("{location} > child"), sites)
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
        | Limit { child, .. } => register_site(child, &format!("{location} > child"), sites),
        Concat { children } => {
            for (i, c) in children.iter().enumerate() {
                walk_rc_children(c, &format!("{location} > concat[{i}]"), sites);
            }
        }
        Join { left, right, .. } | SetOp { left, right, .. } => {
            register_site(left, &format!("{location} > left"), sites);
            register_site(right, &format!("{location} > right"), sites);
        }
        BinaryOp { lhs, rhs, .. } => {
            register_site(lhs, &format!("{location} > lhs"), sites);
            register_site(rhs, &format!("{location} > rhs"), sites);
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

/// Visit every **operator** child of `node` (the same relational-skeleton
/// scope `pre_asap::canonicalize`'s `children_mut` and `pre_asap::cse`'s
/// `rebuild_children` use — see those modules' docs) with a breadcrumb
/// location string built from `location`. A deliberately independent
/// traversal (same status as `dag_export`'s own `structural_hash` next to
/// `pre_asap::cse`'s — see that module's doc on unifying these being future
/// work, not attempted here): this crate depends only on `asap_types`, never
/// on that private helper.
fn for_each_operator_child(
    node: &QueryExpr,
    location: &str,
    mut visit: impl FnMut(&QueryExpr, String),
) {
    use QueryExpr::*;
    match node {
        Scan { .. } | PromqlScalar(_) | QueryTimestamp => {}
        PromqlVectorFromScalar(c) | PromqlScalarFromVector(c) => {
            visit(c, format!("{location} > child"))
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
        | Limit { child, .. } => visit(child, format!("{location} > child")),
        Concat { children } => {
            for (i, c) in children.iter().enumerate() {
                visit(c, format!("{location} > concat[{i}]"));
            }
        }
        Join { left, right, .. } | SetOp { left, right, .. } => {
            visit(left, format!("{location} > left"));
            visit(right, format!("{location} > right"));
        }
        BinaryOp { lhs, rhs, .. } => {
            visit(lhs, format!("{location} > lhs"));
            visit(rhs, format!("{location} > rhs"));
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

/// The built-in rule set: [`SketchApplicabilityRule::default_cost_model`]
/// and [`SharedSubexpressionRule`]. Used by [`find_applicable_optimizations`];
/// pass your own list (e.g. this set plus a deployment-specific rule, or
/// [`SketchApplicabilityRule::new`] with a custom `CostModel`) to
/// [`find_applicable_optimizations_with`] instead.
pub fn default_rules() -> Vec<Box<dyn ApplicabilityRule>> {
    vec![
        Box::new(SketchApplicabilityRule::default_cost_model()),
        Box::new(SharedSubexpressionRule),
    ]
}

/// Determine which known optimizations are applicable to a workload's
/// pre-ASAP query roots, using the built-in [`default_rules`].
///
/// `roots` — like [`share_common_subtrees`](asap_types::pre_asap::cse::share_common_subtrees)'s
/// and [`bind::implement_workload`](crate::bind::implement_workload)'s own
/// `Id` type parameter — is caller-chosen: a `QueryWorkload` entry's own key,
/// an index, a query name. It only needs `Display`, since a finding's
/// `location` is prose, not a structured key back to the caller.
///
/// Runs `share_common_subtrees` itself once (`asap_aware_mapping` has no
/// other caller-visible way to get CSE'd roots without also binding them),
/// so every rule — including ones with nothing to do with CSE — sees the
/// same already-deduplicated tree [`bind::implement_workload`](crate::bind::implement_workload)
/// would.
pub fn find_applicable_optimizations<Id: Display>(
    roots: Vec<(Id, QueryExpr)>,
) -> Vec<ApplicabilityFinding> {
    let rules = default_rules();
    find_applicable_optimizations_with(roots, &rules)
}

/// Like [`find_applicable_optimizations`], but evaluates `rules` instead of
/// [`default_rules`] — the extension point for a deployment-specific rule,
/// or a custom `CostModel` plugged into [`SketchApplicabilityRule::new`].
pub fn find_applicable_optimizations_with<'a, Id: Display>(
    roots: Vec<(Id, QueryExpr)>,
    rules: &[Box<dyn ApplicabilityRule + 'a>],
) -> Vec<ApplicabilityFinding> {
    let ided: Vec<(String, QueryExpr)> = roots
        .into_iter()
        .map(|(id, expr)| (id.to_string(), expr))
        .collect();
    let shared = share_common_subtrees(ided);
    rules
        .iter()
        .flat_map(|rule| rule.evaluate(&shared))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::pre_asap::agg_intent::default_quantile;
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

    // ── SketchApplicabilityRule ─────────────────────────────────────────

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
        // quantile under an exact sum, the same nesting `bind.rs`'s own
        // `nested_aggregates_bind_per_node` test uses.
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

    // ── SharedSubexpressionRule ─────────────────────────────────────────

    #[test]
    fn two_roots_with_the_same_grouped_aggregate_share_a_reuse_finding() {
        // Grouped (`by (job)`), so the shared `Aggregate`'s output schema
        // carries a provable unique key — `share_common_subtrees`'s legality
        // gate (see `pre_asap::cse`'s module doc) — and identical across both
        // roots, so it is shareable.
        let a = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let b = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let findings = find_applicable_optimizations(vec![("dash_a", a), ("dash_b", b)]);
        let reuse: Vec<_> = findings
            .iter()
            .filter(|f| f.optimization == OptimizationKind::SharedSubexpressionReuse)
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
                .all(|f| f.optimization != OptimizationKind::SharedSubexpressionReuse),
            "structurally different queries must not report reuse, got {findings:?}"
        );
    }

    #[test]
    fn ungrouped_identical_aggregates_are_not_shareable_so_no_finding() {
        // Empty `by`: no provable unique key (see `pre_asap::cse`'s module
        // doc + its `no_unique_keys_means_no_merge_even_when_structurally_identical`
        // test) — `share_common_subtrees` never hoists these, so this rule
        // must not report a finding either.
        let a = agg(vec![], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let b = agg(vec![], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let findings = find_applicable_optimizations(vec![("a", a), ("b", b)]);
        assert!(findings
            .iter()
            .all(|f| f.optimization != OptimizationKind::SharedSubexpressionReuse));
    }

    #[test]
    fn single_query_repeated_subexpression_is_a_reuse_finding() {
        // The same shared branch appearing twice within one query (a `a/a`
        // shape) — single-query CSE, see `pre_asap::cse`'s module doc.
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
            .filter(|f| f.optimization == OptimizationKind::SharedSubexpressionReuse)
            .collect();
        assert_eq!(
            reuse.len(),
            1,
            "expected one reuse finding, got {findings:?}"
        );
        assert!(reuse[0].location.contains("lhs"));
        assert!(reuse[0].location.contains("rhs"));
    }

    // ── Custom rule set / cost model plumbing ───────────────────────────

    struct AlwaysDDSketch;
    impl CostModel for AlwaysDDSketch {
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
        let rules: Vec<Box<dyn ApplicabilityRule + '_>> =
            vec![Box::new(SketchApplicabilityRule::new(&custom_model))];
        let findings = find_applicable_optimizations_with(vec![("q", q)], &rules);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].reason.to_lowercase().contains("ddsketch"));
    }
}
