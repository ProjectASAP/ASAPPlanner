//! End-to-end pre-ASAP CSE → workload-wide search pin (issue #212, #222,
//! #223).
//!
//! Drives the full staged pipeline this issue lands: two independently
//! lowered `QueryExpr` trees → `share_common_subtrees` (stage 1,
//! `asap-types::pre_asap::cse`, run internally by `search_workload`) →
//! `search_workload` (stage 2, `asap-aware-mapping`) — and asserts the
//! sharing that stage 1 decides survives into stage 2's discovered
//! `PlanSpace` as one genuinely shared `MemoGroup`, not just one shared
//! `Rc<QueryExpr>`. This is the "real caller" the issue's landing plan
//! requires before `share_common_subtrees` is allowed to exist at all (its
//! predecessor, `asap-plan::cse::dedupe_subtrees`, was deleted in #192 for
//! being unwired dead code).
//!
//! Committing to one final, physically-materialized answer for a whole
//! workload (the former `implement_workload`/`implement_workload_with`,
//! which this test file used to drive instead of `search_workload`) is out
//! of `asap-aware-mapping`'s scope — see that crate's `lib.rs` `## Status`
//! section — so these tests assert on the discovered `PlanSpace` shape
//! directly, the same way `asap-aware-mapping::replacement`'s own
//! `shared_aggregate_across_two_roots_gets_both_strategies_candidates` test
//! does, just exercised through the crate's public API from this external
//! integration-test crate.

use std::rc::Rc;

use asap_aware_mapping::{search_workload, Replacement};
use asap_frontend_promql::lower_promql;
use asap_types::pre_asap::query_expr::QueryExpr;
use asap_types::types::AccuracyTarget;

/// Two workload entries that happen to submit the exact same query (a
/// realistic case — two dashboards, or a query fired both standalone and as
/// part of a larger batch) collapse onto one shared `Rc<QueryExpr>` after
/// `search_workload`'s internal `share_common_subtrees` pass, and onto one
/// genuinely-shared [`MemoGroup`](asap_aware_mapping::MemoGroup) — carrying
/// every candidate discovered for it exactly once, not once per root — no
/// second structural-equality pass at the post-ASAP layer needed for this
/// kind of sharing.
#[test]
fn duplicate_workload_queries_collapse_onto_one_memo_group() {
    // Grouped (`by (job)`), so the shared `Aggregate`'s output schema carries
    // a provable unique key — the legality gate `share_common_subtrees`
    // enforces (see `asap-types::pre_asap::cse`'s module doc) — and its
    // `ExactAggregate(Sum)` realization is deterministic regardless of the
    // accuracy target, so this pins the sharing mechanism itself rather than
    // any one particular summary-family choice.
    let query = "sum by (job) (http_requests_total)";
    let a = lower_promql(query, AccuracyTarget::Exact).expect("query a failed to lower");
    let b = lower_promql(query, AccuracyTarget::Exact).expect("query b failed to lower");

    // Independently lowered: not yet sharing any `Rc`, even though they are
    // structurally identical (`resolve_root` gives each call its own fresh
    // tree).
    assert_eq!(
        a, b,
        "fixture sanity: identical query text lowers identically"
    );

    let space = search_workload(vec![("a", Rc::new(a)), ("b", Rc::new(b))]);

    // roots[0] and roots[1] must have merged onto the same Rc — the
    // `share_common_subtrees` pass `search_workload` runs internally.
    assert!(
        Rc::ptr_eq(&space.roots[0].1, &space.roots[1].1),
        "search_workload must collapse the two identical roots onto one Rc<QueryExpr>"
    );

    // The single shared root is one discovered TargetSubDAG, holding one
    // MemoGroup with consumer_count 2 — SketchFamilyStrategy's one
    // ExactAggregate candidate *and* SharedSubtreeStrategy's share-vs-
    // recompute pair, exactly as `shared_aggregate_across_two_roots_gets_both_strategies_candidates`
    // (asap-aware-mapping::replacement's own equivalent, internal test)
    // pins for the same fixture shape.
    let group = space
        .group_for(&space.roots[0].1)
        .expect("shared root must be a discovered target");
    assert_eq!(group.consumer_count, 2);
    assert_eq!(
        group.candidates.len(),
        3,
        "1 ExactAggregate Summary + 2 Rewrite (share/recompute): {:?}",
        group.candidates
    );

    let summary_count = group
        .candidates
        .iter()
        .filter(|c| matches!(c.replacement, Replacement::Summary(_)))
        .count();
    let rewrite_count = group
        .candidates
        .iter()
        .filter(|c| matches!(c.replacement, Replacement::Rewrite(_)))
        .count();
    assert_eq!(summary_count, 1);
    assert_eq!(rewrite_count, 2);

    // The two Rewrite candidates must NOT have collapsed into one (the
    // "false-positive dedup" failure mode `is_duplicate_rewrite` exists to
    // prevent): one shares the group's own target `Rc`, the other is a
    // structurally-identical but independently-built `Rc`.
    let one_is_the_target = group.candidates.iter().any(
        |c| matches!(&c.replacement, Replacement::Rewrite(rc) if Rc::ptr_eq(rc, &group.target)),
    );
    let one_is_not = group.candidates.iter().any(
        |c| matches!(&c.replacement, Replacement::Rewrite(rc) if !Rc::ptr_eq(rc, &group.target)),
    );
    assert!(one_is_the_target && one_is_not);
}

/// The negative control: two workload entries whose queries are NOT
/// structurally identical must not be conflated — `search_workload`
/// discovers two independent roots, each its own `TargetSubDAG` with its
/// own `MemoGroup` and `consumer_count == 1`.
#[test]
fn distinct_workload_queries_get_independent_memo_groups() {
    let a = lower_promql("sum by (job) (http_requests_total)", AccuracyTarget::Exact)
        .expect("query a failed to lower");
    let b = lower_promql("sum by (job) (http_response_total)", AccuracyTarget::Exact)
        .expect("query b failed to lower");
    assert_ne!(a, b, "fixture sanity: the two queries differ");

    let space = search_workload(vec![("a", Rc::new(a)), ("b", Rc::new(b))]);
    assert!(!Rc::ptr_eq(&space.roots[0].1, &space.roots[1].1));

    let group_a = space
        .group_for(&space.roots[0].1)
        .expect("root a must be a discovered target");
    let group_b = space
        .group_for(&space.roots[1].1)
        .expect("root b must be a discovered target");
    assert!(
        !Rc::ptr_eq(&group_a.target, &group_b.target),
        "distinct queries must land in distinct MemoGroups"
    );
    assert_eq!(group_a.consumer_count, 1);
    assert_eq!(group_b.consumer_count, 1);
}

/// Single-query CSE (a repeated sub-expression within one query) also
/// survives through `search_workload`: the two grouped-`Aggregate` branches
/// of a `BinaryOp` collapse to one shared `Rc<QueryExpr>` in the internal
/// `share_common_subtrees` pass, and to one shared `MemoGroup` (with
/// `consumer_count == 2`, one per branch) here.
#[test]
fn single_query_repeated_subexpression_shares_one_memo_group() {
    let query = "sum by (job) (http_requests_total) / sum by (job) (http_requests_total)";
    let expr = lower_promql(query, AccuracyTarget::Exact).expect("query failed to lower");

    let space = search_workload(vec![("q", Rc::new(expr))]);
    let [(_, root)] = space.roots.as_slice() else {
        panic!("expected 1 root");
    };
    let QueryExpr::BinaryOp { lhs, rhs, .. } = root.as_ref() else {
        panic!("expected a BinaryOp root, got {root:?}");
    };
    assert!(
        Rc::ptr_eq(lhs, rhs),
        "the two identical sum-by-job branches must collapse onto one Rc<QueryExpr>"
    );

    let group = space
        .group_for(lhs)
        .expect("the shared branch must be a discovered target");
    assert_eq!(
        group.consumer_count, 2,
        "the shared branch is referenced from both BinaryOp operand positions"
    );
    assert!(
        !group.candidates.is_empty(),
        "the shared branch must carry at least one candidate"
    );
}
