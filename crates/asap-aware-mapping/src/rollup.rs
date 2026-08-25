//! [`RollupStrategy`] — group-by-lattice roll-up reuse (fine-to-coarse) as a
//! [`ReplacementStrategy`] (issue #254, part of #33).
//!
//! ## The optimization: AHA-style roll-up, not independent subpopulations
//!
//! `docs/asap_aware_mapping.md`'s "Degrees of freedom" section names this
//! axis directly: **"AHA vs treating hierarchical subpopulations
//! independently."** Two `Aggregate` nodes over the same source, grouped at
//! different granularities (e.g. `by (job)` and `by (job, region)`), are
//! today two *independent* passes over the source. But when the coarser
//! grouping's keys are a subset of the finer grouping's keys, the coarser
//! answer is derivable from the finer one *without ever touching the raw
//! source again* — the same "roll up the group-by lattice" reuse a SQL
//! `ROLLUP`/OLAP-cube engine already exploits between grouping levels of one
//! `GROUP BY ROLLUP(...)` (see `frontend-sql`'s own `ROLLUP` lowering
//! tests), generalized here to two *independently written* aggregates in a
//! workload rather than one multi-level `ROLLUP` clause.
//!
//! This is exactly Peilin's catalog entry (issue #33's comment thread):
//! "Rolling up aggregations on a fine-grained group by to get a
//! coarse-grained group by (like AHA)," alongside "CSE across aggregations,
//! and group by key management" — this strategy is the *cross-aggregate*
//! sibling of `pre_asap::cse::share_common_subtrees`'s *identical*-subtree
//! sharing: CSE shares two structurally-*equal* aggregates onto one `Rc`;
//! this strategy relates two structurally-*different* (differently grouped)
//! aggregates over the same shared source.
//!
//! ## Why re-aggregating the finer side needs a *combinator*, not literally
//! "the same measure" reapplied
//!
//! Re-deriving `by (job)` from `by (job, region)` means computing, for each
//! `job`, "the combination of every `(job, region)` partial result for that
//! `job`." Whether that combination is *the same operator reapplied* depends
//! on the operator:
//!
//! - `Sum`/`Min`/`Max` are **self-combining**: a sum of sums is a sum, a min
//!   of mins is a min. Reapplying the identical `AggIntent` over the finer
//!   side's own output column is correct.
//! - `Count` is **not** self-combining: re-`Count`ing the finer side's own
//!   output rows counts the number of *finer groups* per coarser group (how
//!   many distinct `region`s a `job` has), not the original row count. The
//!   correct combinator is `Sum` — `count(A ∪ B) = count(A) + count(B)`, the
//!   same "`COUNT(*)` over a pre-aggregated summary table becomes
//!   `SUM(count)`" rule every OLAP rollup/cube/materialized-view-matching
//!   implementation applies. `Increase` mirrors it (a counter's total
//!   increase across sub-windows is additive), so it combines via `Sum` too.
//! - `Rate` (`increase / duration`) has **no** valid self- or sum-combinator
//!   here, so it is deliberately left unhandled (see [`rollup_combinator`]).
//!   In practice this never matters: `Rate`/`Increase` are constructed with
//!   `Reduction::PerEntity` (per-series, no `by` at all — see
//!   `AggIntent::is_per_series` and `asap_frontend_promql::promql::reduction_for`),
//!   never `Reduction::Reduce`, so they never carry a `by` set for this
//!   strategy to compare in the first place; `PerEntity` nodes never match
//!   ([`bindable_grouped_aggregate`] returns `None` for them). `Increase`'s
//!   entry in [`rollup_combinator`] is a defensive completion of the match
//!   (exhaustive-over-the-mergeable-vocabulary, "no silent fallthrough"),
//!   not a case expected to fire today.
//!
//! [`rollup_combinator`] is the single place this substitution is decided —
//! [`is_legal_rollup_source`] (the standalone legality predicate issue #254
//! asks for) consults it, and so does this module's `replacements`, so the
//! two can never disagree about which intents are eligible.
//!
//! ## `ColumnId` comparability — only sound because the child `Rc` is shared
//!
//! A `ColumnId` is a *position* into a specific `Schema` (`crates/types/src/pre_asap/schema.rs`'s
//! own doc: "the same edge, the same schema, the same positional numbering").
//! Comparing the coarser aggregate's `by` positions against the finer
//! aggregate's `by` positions is only meaningful because **both aggregates
//! share the identical child `Rc`** (post-CSE, per
//! `pre_asap::cse::share_common_subtrees`'s doc on why hash-consing is a
//! precondition, not a coincidence) — so both `by` lists are positional
//! offsets into the exact same schema. Two *structurally different but
//! equivalent* sources (e.g. two independently-built scans of the same
//! underlying table with columns in a different order) are explicitly out
//! of scope: this module never attempts to reconcile `ColumnId`s across two
//! distinct schemas.
//!
//! ## Non-goals (tracked separately, not attempted here — same split
//! `replacement.rs`'s own module docs draw for `SharedSubtreeStrategy`'s
//! `consumer_count`)
//!
//! - **No workload-wide sibling discovery.** Finding "every `Aggregate` node
//!   across a whole workload that shares a given `Rc` child" is a traversal
//!   over the *whole* workload, not a fact available from one
//!   [`TargetSubDAG`] in isolation — [`ReplacementStrategy::matches`]/
//!   `replacements` only ever see one target at a time. [`RollupStrategy`]
//!   takes the already-discovered sibling set as constructor state instead
//!   of rediscovering it: a future caller (the search engine, issue #252,
//!   built in parallel and possibly not yet landed on this branch) is
//!   expected to construct this strategy with the real sibling set it
//!   already found — the same "this module wraps a decision, it does not
//!   own the traversal that feeds it" split `replacement.rs` draws for
//!   `SharedSubtreeStrategy::matches`'s `consumer_count`.
//! - **No materialized roll-up operator.** Actually building a pre-aggregated
//!   summary/scan leaf at execution time is separate, larger work outside
//!   `asap-aware-mapping`'s scope (see issue #254's own "Non-goal" section)
//!   — this module only constructs the pre-ASAP [`QueryExpr::Aggregate`]
//!   rewrite; a `CostModel`/search engine decides whether to prefer it.
//! - **No cross-schema reconciliation** (see "`ColumnId` comparability"
//!   above) and **no `without(...)` grouping support** — `without`'s kept
//!   set is runtime-open (never enumerable at plan time, per
//!   `GroupKeys`'s own doc), so there is no fixed `ColumnId` set to compare
//!   against a superset/subset relationship at all; [`is_legal_rollup_source`]
//!   declines both directions.

use std::collections::HashSet;
use std::rc::Rc;

use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::query_expr::{GroupKeys, QueryExpr, Reduction};
use asap_types::pre_asap::schema::{ColumnId, Schema};

use crate::replacement::{Replacement, ReplacementStrategy, ReplacementSubDAG, TargetSubDAG};

/// The `(by, intent, child)` shape this strategy operates on: a single
/// measure, no `HAVING` — the same bindable shape
/// [`crate::replacement::SketchAlgorithmStrategy`] requires (see that module's
/// private `bindable_intent`) — **plus** a genuine [`Reduction::Reduce`]
/// grouping to compare (not [`Reduction::PerEntity`], which has no `by` set
/// at all). `None` for anything else, including a multi-measure or `HAVING`
/// aggregate, a non-`Aggregate` node, or a `PerEntity` reduction.
fn bindable_grouped_aggregate(
    node: &QueryExpr,
) -> Option<(&GroupKeys, &AggIntent, &Rc<QueryExpr>)> {
    let QueryExpr::Aggregate {
        reduction,
        measures,
        having,
        child,
        ..
    } = node
    else {
        return None;
    };
    let ([intent], None) = (measures.as_slice(), having) else {
        return None;
    };
    let Reduction::Reduce(by) = reduction else {
        return None;
    };
    Some((by, intent, child))
}

/// The `AggIntent` to combine `intent`'s partial results with, when
/// re-aggregating over an already-computed finer aggregate's own measure
/// column at position `finer_measure_col` — see the module docs' "Why
/// re-aggregating the finer side needs a *combinator*" section for the
/// reasoning behind each arm. `None` for any intent this module does not
/// (yet) know a correct combinator for — including every intent
/// `agg_is_mergeable` permits but this module doesn't specifically handle
/// (`Rate`, and everything outside the `Sum`/`Count`/`MinMax`/`Increase`
/// vocabulary `agg_is_mergeable`'s own doc names) — so `is_legal_rollup_source`
/// (which calls this) is *strictly narrower* than `agg_is_mergeable` alone,
/// deliberately: `agg_is_mergeable` answers "does *some* partial-state merge
/// exist", not "is self- or sum-recombination the right one," and this
/// module only ever proposes a rewrite it can construct correctly.
fn rollup_combinator(intent: &AggIntent, finer_measure_col: ColumnId) -> Option<AggIntent> {
    match intent {
        // Self-combining: reapplying the identical operator over the finer
        // side's own output column is correct unchanged.
        AggIntent::Sum { .. } => Some(AggIntent::Sum {
            col: Some(finer_measure_col),
        }),
        AggIntent::Min { .. } => Some(AggIntent::Min {
            col: Some(finer_measure_col),
        }),
        AggIntent::Max { .. } => Some(AggIntent::Max {
            col: Some(finer_measure_col),
        }),
        // Not self-combining — see the module docs. Both merge by addition
        // over the finer side's own output column instead.
        AggIntent::Count { .. } | AggIntent::Increase => Some(AggIntent::Sum {
            col: Some(finer_measure_col),
        }),
        _ => None,
    }
}

/// **The standalone legality predicate issue #254 asks for**: is `finer`'s
/// grouping a legal roll-up source for `coarser`'s grouping, given that both
/// nodes compute an aggregation intent over the same shared child?
///
/// Issue #256 (`GroupingStrategy`/Hydra axis, built in parallel from the
/// same base) is expected to call this exact function before assuming a
/// Hydra-backed aggregate composes with a roll-up — it is named and
/// exported for that reason, not left as private inline logic in `matches`/
/// `replacements`.
///
/// Legal iff, **in this order**:
///
/// 1. `finer_intent == coarser_intent` — the two aggregates compute the
///    identical intent + column (verified here, not just assumed by the
///    caller, so this function is a complete, self-contained answer on its
///    own).
/// 2. [`rollup_combinator`] knows how to re-derive `finer_intent` from a
///    pre-aggregated column at all (excludes `Avg`/`StdDev`/`Variance` —
///    never `agg_is_mergeable` — and also excludes every `agg_is_mergeable`
///    intent this module doesn't specifically handle, e.g. `Rate`).
/// 3. Neither grouping is a `without(...)` exclusion grouping — `without`'s
///    kept set is runtime-open, so there is no fixed `ColumnId` set to
///    compare a superset/subset relationship against (see the module docs).
/// 4. `finer_output_schema` (the finer aggregate's own *output* schema, not
///    the shared child's) carries a provable unique key
///    ([`Schema::has_unique_key`]) — **the exact legality gate
///    `pre_asap::cse::share_common_subtrees` already applies to its own
///    sharing decisions**, reused verbatim here rather than re-invented:
///    `share_common_subtrees`'s own doc ("Legality: gated by
///    `Schema::unique_keys`") states a producer's output is only safely
///    reusable across consumers when its row identity is provably stable —
///    exactly the property re-aggregating over `finer` as if it were a
///    fresh source requires.
/// 5. `coarser_by` is a **strict, proper** subset of `finer_by` (same
///    `ColumnId`s, finer strictly more of them) — an *equal* `by` is
///    `SharedSubtreeStrategy`'s CSE-sharing question, not a roll-up, so
///    equality is deliberately excluded here, not treated as a degenerate
///    roll-up.
pub fn is_legal_rollup_source(
    finer_by: &GroupKeys,
    finer_output_schema: &Schema,
    finer_intent: &AggIntent,
    coarser_by: &GroupKeys,
    coarser_intent: &AggIntent,
) -> bool {
    if finer_intent != coarser_intent {
        return false;
    }
    if rollup_combinator(finer_intent, 0).is_none() {
        return false;
    }
    if finer_by.is_without() || coarser_by.is_without() {
        return false;
    }
    if !finer_output_schema.has_unique_key() {
        return false;
    }
    is_strict_column_superset(finer_by.keys(), coarser_by.keys())
}

/// Whether `finer` is a strict, proper superset of `coarser` — every
/// `ColumnId` in `coarser` also appears in `finer`, and `finer` has more of
/// them (an equal-length or shorter `finer` can never be a proper
/// superset, so the length check alone rules out equality without a set
/// comparison).
fn is_strict_column_superset(finer: &[ColumnId], coarser: &[ColumnId]) -> bool {
    if finer.len() <= coarser.len() {
        return false;
    }
    let finer_set: HashSet<&ColumnId> = finer.iter().collect();
    coarser.iter().all(|id| finer_set.contains(id))
}

/// Wraps the group-by-lattice roll-up reuse (issue #254, part of #33) as a
/// [`ReplacementStrategy`]: given a coarser `Aggregate` [`TargetSubDAG`],
/// finds every already-known sibling `Aggregate` that shares the same child
/// `Rc` and is a legal, strictly finer roll-up source for it (per
/// [`is_legal_rollup_source`]), and proposes replacing the target with a
/// re-aggregation over that sibling instead of the shared raw source.
///
/// `siblings` is **caller-supplied, not discovered here** — see the module
/// docs' "Non-goals" on why finding the full sibling set across a workload
/// is a workload-wide traversal this strategy does not own.
pub struct RollupStrategy<'a> {
    siblings: &'a [Rc<QueryExpr>],
}

impl<'a> RollupStrategy<'a> {
    /// A strategy that considers every node in `siblings` as a candidate
    /// roll-up source (or target) — typically the full set of `Aggregate`
    /// nodes a workload-wide discovery pass (issue #252) already found
    /// sharing at least one child `Rc` with something else.
    pub fn new(siblings: &'a [Rc<QueryExpr>]) -> Self {
        Self { siblings }
    }

    /// Every sibling that is a legal, strictly finer roll-up source for
    /// `target` — shared between `matches` and `replacements` so the two
    /// can never disagree about which siblings qualify.
    fn finer_sources(&self, target: &TargetSubDAG<'_>) -> Vec<&'a Rc<QueryExpr>> {
        let Some((coarser_by, coarser_intent, coarser_child)) =
            bindable_grouped_aggregate(target.root)
        else {
            return Vec::new();
        };

        self.siblings
            .iter()
            .filter(|&candidate| {
                if Rc::ptr_eq(candidate, target.root) {
                    return false;
                }
                let Some((finer_by, finer_intent, finer_child)) =
                    bindable_grouped_aggregate(candidate)
                else {
                    return false;
                };
                if !Rc::ptr_eq(finer_child, coarser_child) {
                    return false;
                }
                let Ok(finer_schema) = candidate.output_schema() else {
                    return false;
                };
                is_legal_rollup_source(
                    finer_by,
                    &finer_schema,
                    finer_intent,
                    coarser_by,
                    coarser_intent,
                )
            })
            .collect()
    }
}

impl ReplacementStrategy for RollupStrategy<'_> {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        !self.finer_sources(target).is_empty()
    }

    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        let Some((coarser_by, coarser_intent, _)) = bindable_grouped_aggregate(target.root) else {
            return Vec::new();
        };
        let QueryExpr::Aggregate { output_names, .. } = target.root.as_ref() else {
            unreachable!("bindable_grouped_aggregate already confirmed Aggregate");
        };
        self.finer_sources(target)
            .into_iter()
            .filter_map(|finer| build_rollup(finer, coarser_by, coarser_intent, output_names))
            .collect()
    }
}

/// Build the coarser replacement: a new `QueryExpr::Aggregate` grouped by
/// `coarser_by`'s columns (repositioned into `finer`'s own output schema —
/// see below), computing `rollup_combinator(intent, ..)` over `finer`'s own
/// measure column, with `child = finer` instead of the original shared
/// source.
///
/// `coarser_by`'s `ColumnId`s are positions into the *shared child's*
/// schema (the same schema `finer_by`'s `ColumnId`s index into — see the
/// module docs' "`ColumnId` comparability" section). `finer`'s own output
/// schema is a *different* schema (`finer_by`'s columns, in order, followed
/// by its one measure column — `aggregate_output_schema`'s `by ++ measures`
/// shape), so each of `coarser_by`'s columns must be translated from its
/// position in the shared child to its position in `finer`'s output: the
/// index its `ColumnId` occupies within `finer_by`'s own ordered list.
fn build_rollup(
    finer: &Rc<QueryExpr>,
    coarser_by: &GroupKeys,
    intent: &AggIntent,
    output_names: &[String],
) -> Option<ReplacementSubDAG> {
    let (finer_by, _, _) = bindable_grouped_aggregate(finer)?;
    // `finer`'s own single measure sits right after its `by` columns in its
    // output schema (`aggregate_output_schema`'s `by ++ measures` layout).
    let finer_measure_col: ColumnId = finer_by.len();
    let combinator = rollup_combinator(intent, finer_measure_col)?;

    let remapped_by: Vec<ColumnId> = coarser_by
        .keys()
        .iter()
        .map(|id| finer_by.keys().iter().position(|f| f == id))
        .collect::<Option<Vec<_>>>()?;

    let rewritten = QueryExpr::Aggregate {
        reduction: Reduction::by(remapped_by),
        measures: vec![combinator],
        output_names: output_names.to_vec(),
        having: None,
        child: Rc::clone(finer),
    };

    Some(ReplacementSubDAG {
        replacement: Replacement::Rewrite(Rc::new(rewritten)),
        rationale: format!(
            "rolls up from the finer Aggregate grouped by {:?} (a strict superset of this \
             node's own {:?} grouping over the same shared source) instead of an independent \
             pass over the raw source — both compute {intent:?} over the same input, and the \
             finer side has a provable unique key (Schema::has_unique_key)",
            finer_by.keys(),
            coarser_by.keys(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::pre_asap::query_expr::Source;
    use asap_types::pre_asap::schema::{Column, DataType};
    use asap_types::types::AccuracyTarget;

    /// `[ts(0), value(1), job(2), region(3)]`.
    fn metric_scan() -> QueryExpr {
        QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("value", DataType::Float64, false),
                    Column::new("job", DataType::Utf8, true),
                    Column::new("region", DataType::Utf8, true),
                ],
                0,
                vec![],
            ),
        }
    }

    fn agg(by: Vec<ColumnId>, intent: AggIntent, child: &Rc<QueryExpr>) -> Rc<QueryExpr> {
        Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::by(by),
            measures: vec![intent],
            output_names: vec![],
            having: None,
            child: Rc::clone(child),
        })
    }

    fn without_agg(
        excluded: Vec<ColumnId>,
        intent: AggIntent,
        child: &Rc<QueryExpr>,
    ) -> Rc<QueryExpr> {
        Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::Reduce(GroupKeys::without(excluded)),
            measures: vec![intent],
            output_names: vec![],
            having: None,
            child: Rc::clone(child),
        })
    }

    // ── is_legal_rollup_source (the standalone predicate) ───────────────

    #[test]
    fn predicate_accepts_a_strict_superset_over_a_mergeable_intent_with_a_unique_key() {
        let finer_schema = Schema::with_time_index(vec![], 0, vec![vec![0, 1]]);
        assert!(is_legal_rollup_source(
            &GroupKeys::by(vec![2, 3]),
            &finer_schema,
            &AggIntent::Sum { col: None },
            &GroupKeys::by(vec![2]),
            &AggIntent::Sum { col: None },
        ));
    }

    #[test]
    fn predicate_rejects_mismatched_intents() {
        let finer_schema = Schema::with_time_index(vec![], 0, vec![vec![0, 1]]);
        assert!(!is_legal_rollup_source(
            &GroupKeys::by(vec![2, 3]),
            &finer_schema,
            &AggIntent::Sum { col: None },
            &GroupKeys::by(vec![2]),
            &AggIntent::Sum { col: Some(9) },
        ));
    }

    #[test]
    fn predicate_rejects_a_non_mergeable_intent() {
        let finer_schema = Schema::with_time_index(vec![], 0, vec![vec![0, 1]]);
        assert!(!is_legal_rollup_source(
            &GroupKeys::by(vec![2, 3]),
            &finer_schema,
            &AggIntent::Avg { col: None },
            &GroupKeys::by(vec![2]),
            &AggIntent::Avg { col: None },
        ));
    }

    #[test]
    fn predicate_rejects_an_intent_with_no_known_combinator() {
        // Rate is `agg_is_mergeable` but this module has no correct
        // self-/sum-combinator for it (see `rollup_combinator`'s doc).
        let finer_schema = Schema::with_time_index(vec![], 0, vec![vec![0, 1]]);
        assert!(!is_legal_rollup_source(
            &GroupKeys::by(vec![2, 3]),
            &finer_schema,
            &AggIntent::Rate,
            &GroupKeys::by(vec![2]),
            &AggIntent::Rate,
        ));
    }

    #[test]
    fn predicate_rejects_a_finer_side_with_no_unique_key() {
        // A schema with an empty `unique_keys` — as if `finer` were, e.g., a
        // `without(...)`-grouped or otherwise non-hoistable aggregate — even
        // though the `by` sets themselves are a clean strict superset.
        let no_unique_key_schema = Schema::new(vec![]);
        assert!(!is_legal_rollup_source(
            &GroupKeys::by(vec![2, 3]),
            &no_unique_key_schema,
            &AggIntent::Sum { col: None },
            &GroupKeys::by(vec![2]),
            &AggIntent::Sum { col: None },
        ));
    }

    #[test]
    fn predicate_rejects_equal_by_sets() {
        // Equality is `SharedSubtreeStrategy`'s question, not a roll-up.
        let finer_schema = Schema::with_time_index(vec![], 0, vec![vec![0]]);
        assert!(!is_legal_rollup_source(
            &GroupKeys::by(vec![2]),
            &finer_schema,
            &AggIntent::Sum { col: None },
            &GroupKeys::by(vec![2]),
            &AggIntent::Sum { col: None },
        ));
    }

    #[test]
    fn predicate_rejects_unrelated_by_sets() {
        let finer_schema = Schema::with_time_index(vec![], 0, vec![vec![0, 1]]);
        assert!(!is_legal_rollup_source(
            &GroupKeys::by(vec![3, 4]),
            &finer_schema,
            &AggIntent::Sum { col: None },
            &GroupKeys::by(vec![2]),
            &AggIntent::Sum { col: None },
        ));
    }

    #[test]
    fn predicate_rejects_a_without_grouping_on_either_side() {
        let finer_schema = Schema::with_time_index(vec![], 0, vec![vec![0, 1]]);
        assert!(!is_legal_rollup_source(
            &GroupKeys::without(vec![2, 3]),
            &finer_schema,
            &AggIntent::Sum { col: None },
            &GroupKeys::by(vec![2]),
            &AggIntent::Sum { col: None },
        ));
        assert!(!is_legal_rollup_source(
            &GroupKeys::by(vec![2, 3]),
            &finer_schema,
            &AggIntent::Sum { col: None },
            &GroupKeys::without(vec![2]),
            &AggIntent::Sum { col: None },
        ));
    }

    // ── RollupStrategy ────────────────────────────────────────────────────

    #[test]
    fn superset_by_over_identical_mergeable_intent_and_shared_child_rolls_up() {
        let scan = Rc::new(metric_scan());
        let fine = agg(vec![2, 3], AggIntent::Sum { col: Some(1) }, &scan);
        let coarse = agg(vec![2], AggIntent::Sum { col: Some(1) }, &scan);

        let siblings = vec![Rc::clone(&fine), Rc::clone(&coarse)];
        let strategy = RollupStrategy::new(&siblings);
        let target = TargetSubDAG::new(&coarse);

        assert!(strategy.matches(&target));
        let replacements = strategy.replacements(&target);
        assert_eq!(replacements.len(), 1, "{replacements:?}");

        let Replacement::Rewrite(rewritten) = &replacements[0].replacement else {
            panic!("expected a Rewrite replacement");
        };
        let QueryExpr::Aggregate {
            reduction,
            measures,
            child,
            having,
            ..
        } = rewritten.as_ref()
        else {
            panic!("expected an Aggregate rewrite, got {rewritten:?}");
        };
        assert!(having.is_none());
        assert!(
            Rc::ptr_eq(child, &fine),
            "child must be the finer aggregate"
        );
        assert_eq!(
            reduction.expect_reduce(),
            &vec![0usize],
            "coarser's `job` column (position 2 in the shared source) is the finer \
             aggregate's own column 0"
        );
        assert_eq!(
            measures,
            &vec![AggIntent::Sum { col: Some(2) }],
            "Sum is self-combining: re-Sum over the finer aggregate's own Sum output \
             column (position 2, right after its two `by` keys, job and region)"
        );
        assert!(!replacements[0].rationale.is_empty());
        assert!(replacements[0].rationale.contains("finer"));
    }

    #[test]
    fn count_rolls_up_via_sum_not_count() {
        // Count is not self-combining (see the module docs) — the rewritten
        // measure must be Sum over the finer Count's own output column, not
        // Count reapplied.
        let scan = Rc::new(metric_scan());
        let fine = agg(
            vec![2, 3],
            AggIntent::Count {
                accuracy: AccuracyTarget::Exact,
            },
            &scan,
        );
        let coarse = agg(
            vec![2],
            AggIntent::Count {
                accuracy: AccuracyTarget::Exact,
            },
            &scan,
        );

        let siblings = vec![Rc::clone(&fine), Rc::clone(&coarse)];
        let strategy = RollupStrategy::new(&siblings);
        let target = TargetSubDAG::new(&coarse);

        let replacements = strategy.replacements(&target);
        assert_eq!(replacements.len(), 1, "{replacements:?}");
        let Replacement::Rewrite(rewritten) = &replacements[0].replacement else {
            panic!("expected a Rewrite replacement");
        };
        let QueryExpr::Aggregate { measures, .. } = rewritten.as_ref() else {
            panic!("expected an Aggregate rewrite");
        };
        assert_eq!(
            measures,
            &vec![AggIntent::Sum { col: Some(2) }],
            "Count's own output column sits at position 2, right after its two `by` keys"
        );
    }

    #[test]
    fn rollup_preserves_the_coarser_output_name() {
        let scan = Rc::new(metric_scan());
        let fine = agg(vec![2, 3], AggIntent::Sum { col: Some(1) }, &scan);
        let coarse = Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::by(vec![2]),
            measures: vec![AggIntent::Sum { col: Some(1) }],
            output_names: vec!["total_requests".into()],
            having: None,
            child: Rc::clone(&scan),
        });
        let original_schema = coarse.output_schema().unwrap();

        let siblings = vec![Rc::clone(&fine), Rc::clone(&coarse)];
        let strategy = RollupStrategy::new(&siblings);
        let replacements = strategy.replacements(&TargetSubDAG::new(&coarse));
        let Replacement::Rewrite(rewritten) = &replacements[0].replacement else {
            panic!("expected a Rewrite replacement");
        };

        assert_eq!(rewritten.output_schema().unwrap(), original_schema);
        let QueryExpr::Aggregate { output_names, .. } = rewritten.as_ref() else {
            unreachable!();
        };
        assert_eq!(output_names, &vec!["total_requests".to_string()]);
    }

    #[test]
    fn non_mergeable_intent_does_not_roll_up() {
        let scan = Rc::new(metric_scan());
        let fine = agg(vec![2, 3], AggIntent::Avg { col: Some(1) }, &scan);
        let coarse = agg(vec![2], AggIntent::Avg { col: Some(1) }, &scan);

        let siblings = vec![Rc::clone(&fine), Rc::clone(&coarse)];
        let strategy = RollupStrategy::new(&siblings);
        let target = TargetSubDAG::new(&coarse);

        assert!(!strategy.matches(&target));
        assert!(strategy.replacements(&target).is_empty());
    }

    #[test]
    fn no_unique_key_on_the_finer_side_does_not_roll_up() {
        // `without(...)` groupings never carry a provable unique key
        // (`without_output_schema` always reports `unique_keys: []`) even
        // though its `keys()` (the *excluded* positions here) happens to be
        // numerically a superset of the coarser side's *kept* positions —
        // `is_legal_rollup_source` rejects any `without` grouping outright,
        // and would reject on the missing unique key regardless.
        let scan = Rc::new(metric_scan());
        let fine = without_agg(vec![2, 3], AggIntent::Sum { col: Some(1) }, &scan);
        let coarse = agg(vec![2], AggIntent::Sum { col: Some(1) }, &scan);

        assert!(
            !fine.output_schema().unwrap().has_unique_key(),
            "fixture sanity: a without(...) aggregate has no provable unique key"
        );

        let siblings = vec![Rc::clone(&fine), Rc::clone(&coarse)];
        let strategy = RollupStrategy::new(&siblings);
        let target = TargetSubDAG::new(&coarse);

        assert!(!strategy.matches(&target));
        assert!(strategy.replacements(&target).is_empty());
    }

    #[test]
    fn unrelated_by_sets_do_not_roll_up() {
        // Neither `[job]` nor `[region]` is a superset of the other.
        let scan = Rc::new(metric_scan());
        let a = agg(vec![2], AggIntent::Sum { col: Some(1) }, &scan);
        let b = agg(vec![3], AggIntent::Sum { col: Some(1) }, &scan);

        let siblings = vec![Rc::clone(&a), Rc::clone(&b)];
        let strategy = RollupStrategy::new(&siblings);

        let target_a = TargetSubDAG::new(&a);
        assert!(!strategy.matches(&target_a));
        assert!(strategy.replacements(&target_a).is_empty());

        let target_b = TargetSubDAG::new(&b);
        assert!(!strategy.matches(&target_b));
        assert!(strategy.replacements(&target_b).is_empty());
    }

    #[test]
    fn equal_by_sets_do_not_roll_up() {
        // Equal groupings are `SharedSubtreeStrategy`'s CSE-sharing
        // question (build once and share, or build independently) — a
        // roll-up requires a *strict* superset, not equality.
        let scan = Rc::new(metric_scan());
        let a = agg(vec![2], AggIntent::Sum { col: Some(1) }, &scan);
        let b = agg(vec![2], AggIntent::Sum { col: Some(1) }, &scan);

        let siblings = vec![Rc::clone(&a), Rc::clone(&b)];
        let strategy = RollupStrategy::new(&siblings);
        let target = TargetSubDAG::new(&a);
        assert!(!strategy.matches(&target));
    }

    #[test]
    fn different_child_does_not_roll_up_even_with_identical_shape() {
        // Two separately-allocated (not CSE-shared) scans: same shape, but
        // not the same `Rc`, so their `ColumnId`s are not comparable per
        // this module's own scope (see the module docs).
        let fine = agg(
            vec![2, 3],
            AggIntent::Sum { col: Some(1) },
            &Rc::new(metric_scan()),
        );
        let coarse = agg(
            vec![2],
            AggIntent::Sum { col: Some(1) },
            &Rc::new(metric_scan()),
        );

        let siblings = vec![Rc::clone(&fine), Rc::clone(&coarse)];
        let strategy = RollupStrategy::new(&siblings);
        let target = TargetSubDAG::new(&coarse);
        assert!(!strategy.matches(&target));
    }

    #[test]
    fn does_not_match_a_multi_measure_or_having_aggregate() {
        let scan = Rc::new(metric_scan());
        let fine = agg(vec![2, 3], AggIntent::Sum { col: Some(1) }, &scan);
        let multi = Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::by(vec![2]),
            measures: vec![
                AggIntent::Sum { col: Some(1) },
                AggIntent::Count {
                    accuracy: AccuracyTarget::Exact,
                },
            ],
            output_names: vec![],
            having: None,
            child: Rc::clone(&scan),
        });

        let siblings = vec![Rc::clone(&fine), Rc::clone(&multi)];
        let strategy = RollupStrategy::new(&siblings);
        let target = TargetSubDAG::new(&multi);
        assert!(!strategy.matches(&target));
        assert!(strategy.replacements(&target).is_empty());
    }
}
