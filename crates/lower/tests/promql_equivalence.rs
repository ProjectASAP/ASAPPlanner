//! PromQL **semantic-equivalence proving** for the L1→L3 lowering.
//!
//! The lowering is a *normalizer*: it should map a whole class of
//! semantically-equivalent PromQL strings to **one** canonical L3 tree, and
//! must keep semantically-*distinct* queries distinct. This suite proves:
//!
//!   1. Equivalence classes collapse to identical L3  (`assert_equiv`).
//!   2. Distinct meanings stay distinct                (`assert_distinct`).
//!   3. The lowering never *wrongly* equates distinct semantics — the cases it
//!      cannot faithfully distinguish are **rejected**, not silently merged.
//!
//! Equivalences are grammar/spec-level facts, drawn from:
//!   - PromQL basics: <https://prometheus.io/docs/prometheus/latest/querying/basics/>
//!   - PromLabs cheat sheet: <https://promlabs.com/promql-cheat-sheet/>
//!   - Prometheus engine tests (operators.test, aggregators.test, selectors.test):
//!     <https://github.com/prometheus/prometheus/tree/main/promql/promqltest/testdata>

#![allow(non_snake_case)]

use asap_control_core::intent_algebra::QueryExpr;
use asap_control_core::types::AccuracyTarget;
use asap_control_lower::lower_promql;

fn lo(q: &str) -> QueryExpr {
    lower_promql(q, AccuracyTarget::Exact).unwrap_or_else(|e| panic!("{q:?} should lower: {e}"))
}

/// Every member of an equivalence class must lower to the *same* L3 tree.
fn assert_equiv(class: &[&str]) {
    let first = lo(class[0]);
    for q in &class[1..] {
        assert_eq!(
            lo(q),
            first,
            "expected {q:?} ≡ {:?}, but they lowered to different L3",
            class[0]
        );
    }
}

/// Two semantically-distinct queries must lower to *different* L3 trees.
fn assert_distinct(a: &str, b: &str) {
    assert_ne!(
        lo(a),
        lo(b),
        "{a:?} and {b:?} must not collapse to the same L3"
    );
}

/// A query whose semantics we can't faithfully represent must be rejected
/// (never silently mislowered into a different meaning).
fn assert_rejected(q: &str) {
    assert!(
        lower_promql(q, AccuracyTarget::Exact).is_err(),
        "{q:?} should be rejected, not silently lowered"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. Equivalence classes the lowering canonicalises to one L3.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn aggregation_modifier_placement_is_equivalent() {
    // `<agg> by (..) (expr)` and `<agg>(expr) by (..)` are the same query.
    assert_equiv(&["sum by (job) (up)", "sum(up) by (job)"]);
    assert_equiv(&["count by (instance) (up)", "count(up) by (instance)"]);
}

#[test]
fn parentheses_are_transparent() {
    assert_equiv(&[
        "sum(rate(http_requests_total[5m]))",
        "(sum(rate(http_requests_total[5m])))",
        "sum((rate(http_requests_total[5m])))",
    ]);
}

#[test]
fn whitespace_is_irrelevant() {
    assert_equiv(&[
        "rate(http_requests_total[5m])",
        "rate( http_requests_total [5m] )",
        "rate(http_requests_total[5m]  )",
    ]);
}

#[test]
fn label_matcher_order_is_equivalent() {
    // A matcher set is unordered: same series, so same L3 (FIX: predicates are
    // now canonicalised by (name, value) at lowering time).
    assert_equiv(&[r#"up{job="a",env="prod"}"#, r#"up{env="prod",job="a"}"#]);
}

#[test]
fn group_key_order_is_equivalent() {
    // Grouping labels are a set: `by (a, b)` ≡ `by (b, a)` (FIX: keys sorted).
    assert_equiv(&["sum by (instance, job) (up)", "sum by (job, instance) (up)"]);
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. Distinct semantics must NOT collapse.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn distinct_semantics_stay_distinct() {
    // Outer aggregation matters (the sum(rate) two-level fix).
    assert_distinct("sum(rate(m[5m]))", "rate(m[5m])");
    // Window size matters.
    assert_distinct("rate(m[5m])", "rate(m[10m])");
    // Operand order matters for non-commutative binary ops.
    assert_distinct("a / b", "b / a");
    // Operator identity matters.
    assert_distinct("a and b", "a or b");
    // Quantile parameter matters.
    assert_distinct(
        "quantile_over_time(0.9, m[5m])",
        "quantile_over_time(0.5, m[5m])",
    );
    // Grouping dimension matters.
    assert_distinct("sum by (job) (up)", "sum by (instance) (up)");
    // Aggregator identity matters.
    assert_distinct("sum(up)", "avg(up)");
    // Heavy-hitter topk vs generic bottomk are different plans.
    assert_distinct(
        "topk(5, count_over_time(m[5m]))",
        "bottomk(5, count_over_time(m[5m]))",
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. Intentional intent-level equivalence (documented, not a bug).
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn rate_and_irate_share_the_same_intent() {
    // L3 captures *intent* ("per-second rate of a counter"), not the estimation
    // method. `rate` (windowed average) and `irate` (last two samples) differ
    // only in HOW the rate is estimated — an L4/execution concern — so they
    // share one L3 intent by design.
    assert_equiv(&["rate(m[5m])", "irate(m[5m])"]);
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. Distinct semantics we cannot faithfully represent are REJECTED, not
//    silently merged into a wrong intent. (Each previously mislowered.)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn changes_and_resets_are_not_count_over_time() {
    // PromQL: count_over_time = #samples, changes = #value-changes,
    // resets = #counter-resets. They previously all collapsed to `Count`; now
    // each lowers to its own intent (issue #44), so all three are pairwise
    // distinct L3 rather than being rejected or merged.
    assert_distinct("changes(m[5m])", "count_over_time(m[5m])");
    assert_distinct("resets(m[5m])", "count_over_time(m[5m])");
    assert_distinct("changes(m[5m])", "resets(m[5m])");
}

#[test]
fn group_is_not_sum() {
    // PromQL `group` returns a constant 1 per group; it previously collapsed
    // onto `sum` (sum of values).
    assert_rejected("group(up)");
    assert_rejected("group by (job) (up)");
}

#[test]
fn offset_and_at_are_not_dropped() {
    // Time-shift modifiers change the query's meaning; they previously lowered
    // identically to the un-shifted query (silent loss).
    assert_rejected("http_requests_total offset 5m");
    assert_rejected("http_requests_total @ 1609746000");
    assert_rejected("rate(http_requests_total[5m] offset 1h)");
}
