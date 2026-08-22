//! End-to-end pre-ASAP CSE → implementation pin (issue #212, #222, #223).
//!
//! Drives the full staged pipeline this issue lands: two independently
//! lowered `QueryExpr` trees → `share_common_subtrees` (stage 1,
//! `asap-types::pre_asap::cse`) → `implement_workload` (stage 2,
//! `asap-aware-mapping`) — and asserts the sharing that stage 1 decides
//! survives into stage 2's bound output as one genuinely shared
//! `Rc<SummaryNode>`, not just one shared `Rc<QueryExpr>`. This is the "real
//! caller" the issue's landing plan requires before `share_common_subtrees`
//! is allowed to exist at all (its predecessor,
//! `asap-plan::cse::dedupe_subtrees`, was deleted in #192 for being unwired
//! dead code).

use std::rc::Rc;

use asap_aware_mapping::implement_workload;
use asap_frontend_promql::lower_promql;
use asap_types::pre_asap::cse::share_common_subtrees;
use asap_types::pre_asap::query_expr::QueryExpr;
use asap_types::types::AccuracyTarget;

/// Two workload entries that happen to submit the exact same query (a
/// realistic case — two dashboards, or a query fired both standalone and as
/// part of a larger batch) collapse onto one shared `Rc<QueryExpr>` after
/// `share_common_subtrees`, and then onto one genuinely-shared, single build
/// `Rc<SummaryNode>` after `implement_workload` — no second structural-
/// equality pass at the post-ASAP layer needed for this kind of sharing.
#[test]
fn duplicate_workload_queries_share_one_bound_summary() {
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

    let shared = share_common_subtrees(vec![("a", a), ("b", b)]);
    let [(_, qa), (_, qb)] = shared.as_slice() else {
        panic!("expected 2 roots");
    };
    assert!(
        Rc::ptr_eq(qa, qb),
        "share_common_subtrees must collapse the two identical roots onto one Rc<QueryExpr>"
    );

    let bound = implement_workload(shared);
    let [(_, ra), (_, rb)] = bound.as_slice() else {
        panic!("expected 2 bound results");
    };
    let ra = ra.as_ref().expect("query a failed to bind");
    let rb = rb.as_ref().expect("query b failed to bind");
    assert!(
        Rc::ptr_eq(ra, rb),
        "implement_workload must reuse one bound SummaryNode for the shared Rc<QueryExpr>, \
         got two independently-built nodes: {ra:?} vs {rb:?}"
    );
}

/// The negative control: two workload entries whose queries are NOT
/// structurally identical must not be conflated by `implement_workload`'s
/// `Rc::as_ptr` memoization — different `Rc<QueryExpr>` roots always bind
/// independently.
#[test]
fn distinct_workload_queries_do_not_share_a_bound_summary() {
    let a = lower_promql("sum by (job) (http_requests_total)", AccuracyTarget::Exact)
        .expect("query a failed to lower");
    let b = lower_promql("sum by (job) (http_response_total)", AccuracyTarget::Exact)
        .expect("query b failed to lower");
    assert_ne!(a, b, "fixture sanity: the two queries differ");

    let shared = share_common_subtrees(vec![("a", a), ("b", b)]);
    let [(_, qa), (_, qb)] = shared.as_slice() else {
        panic!("expected 2 roots");
    };
    assert!(!Rc::ptr_eq(qa, qb));

    let bound = implement_workload(shared);
    let [(_, ra), (_, rb)] = bound.as_slice() else {
        panic!("expected 2 bound results");
    };
    let ra = ra.as_ref().expect("query a failed to bind");
    let rb = rb.as_ref().expect("query b failed to bind");
    assert!(
        !Rc::ptr_eq(ra, rb),
        "distinct queries must bind independently"
    );
}

/// Single-query CSE (a repeated sub-expression within one query) also
/// survives through `implement_workload`: the two grouped-`Aggregate`
/// branches of a `BinaryOp` collapse to one shared `Rc<QueryExpr>` in
/// `share_common_subtrees`, and to one shared bound `Rc<SummaryNode>` here.
#[test]
fn single_query_repeated_subexpression_shares_one_bound_summary() {
    let query = "sum by (job) (http_requests_total) / sum by (job) (http_requests_total)";
    let expr = lower_promql(query, AccuracyTarget::Exact).expect("query failed to lower");

    let shared = share_common_subtrees(vec![("q", expr)]);
    let [(_, root)] = shared.as_slice() else {
        panic!("expected 1 root");
    };
    let QueryExpr::BinaryOp { lhs, rhs, .. } = root.as_ref() else {
        panic!("expected a BinaryOp root, got {root:?}");
    };
    assert!(
        Rc::ptr_eq(lhs, rhs),
        "the two identical sum-by-job branches must collapse onto one Rc<QueryExpr>"
    );

    // Bind both branches (as if they were two roots of a tiny sub-workload)
    // through the same memoized `implement_workload`, and confirm they bind
    // to the identical `Rc<SummaryNode>`.
    let bound = implement_workload(vec![("lhs", Rc::clone(lhs)), ("rhs", Rc::clone(rhs))]);
    let [(_, ra), (_, rb)] = bound.as_slice() else {
        panic!("expected 2 bound results");
    };
    let ra = ra.as_ref().expect("lhs failed to bind");
    let rb = rb.as_ref().expect("rhs failed to bind");
    assert!(
        Rc::ptr_eq(ra, rb),
        "the shared branch must bind to one shared SummaryNode"
    );
}
