//! Exhaustive PromQL **corpus** — every query string from the three sources,
//! run through the lowerer.
//!
//! Two corpora (in `tests/data/`):
//!   - `promql_corpus_docs.txt`     — verbatim examples from the PromQL basics
//!     docs and the PromLabs cheat sheet.
//!   - `promql_corpus_testdata.txt` — every `eval` expression (deduped) from the
//!     Prometheus engine test suite.
//!
//! We *lower* (not execute), so the property under test is **totality**: for
//! every real-world PromQL string, `lower_promql` returns `Ok` or a clean
//! `Err` and **never panics**. A panic anywhere in the loop fails the test —
//! that is the guarantee. A coverage floor guards against a change silently
//! tanking how much of the corpus we can lower.

use std::rc::Rc;

use asap_aware_mapping::bind::{keep_pre_asap, ImplementError};
use asap_aware_mapping::{
    Replacement, ReplacementStrategy, ReplacementSubDAG, SketchFamilyStrategy, TargetSubDAG,
};
use asap_frontend_promql::{lower_promql, PromqlError as LoweringError};
use asap_types::post_asap::{SummaryExpr, SummaryNode};
use asap_types::pre_asap::query_expr::QueryExpr;
use asap_types::types::AccuracyTarget;

/// This crate has no "bind me one tree" public API any more —
/// `SketchFamilyStrategy::replacements` always returns every candidate, and
/// a caller decides what to keep. This test-only helper reproduces the
/// take-the-first-(`cost_model`-preferred)-candidate pattern so [`bind_tally`]
/// gets one representative `Result` per query, matching what a totality
/// check over the whole corpus wants.
fn bind(expr: &QueryExpr) -> Result<Rc<SummaryNode>, ImplementError> {
    let root = Rc::new(expr.clone());
    let target = TargetSubDAG::new(&root);
    match SketchFamilyStrategy::default_cost_model()
        .replacements(&target)
        .into_iter()
        .next()
    {
        Some(ReplacementSubDAG {
            replacement: Replacement::Summary(node),
            ..
        }) => Ok(node),
        _ => keep_pre_asap(&root),
    }
}

const DOCS: &str = include_str!("data/promql_corpus_docs.txt");
const TESTDATA: &str = include_str!("data/promql_corpus_testdata.txt");

/// Non-comment, non-blank query lines.
fn queries(corpus: &str) -> impl Iterator<Item = &str> {
    corpus
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
}

#[derive(Default, Debug)]
struct Tally {
    lowered: usize,
    rejected: usize,
    unparseable: usize,
}

impl Tally {
    fn total(&self) -> usize {
        self.lowered + self.rejected + self.unparseable
    }
}

/// Lower every query; a panic here fails the test (the totality guarantee).
fn tally(corpus: &str) -> Tally {
    let mut t = Tally::default();
    for q in queries(corpus) {
        match lower_promql(q, AccuracyTarget::Exact) {
            Ok(_) => t.lowered += 1,
            Err(LoweringError::Parse(_)) => t.unparseable += 1,
            Err(_) => t.rejected += 1,
        }
    }
    t
}

/// Every query that lowers, additionally run through the pre-ASAP →
/// post-ASAP `asap-aware-mapping` binding pass (issue #98), at an
/// approximate accuracy target so the sketch-selection boundary actually
/// fires (an `Exact` target would only ever exercise the exact-accumulator
/// arm).
#[derive(Default, Debug)]
struct BindTally {
    /// Root bound to `SummaryAgg`/`SummaryEstimate` — the pass did something.
    transformed: usize,
    /// Root stayed `KeepPreAsap` — the pass left the query untouched.
    unchanged: usize,
    /// [`bind`] returned `Err` (schema derivation failed).
    errored: usize,
}

fn bind_tally(corpus: &str, accuracy: AccuracyTarget) -> BindTally {
    let mut t = BindTally::default();
    for q in queries(corpus) {
        let Ok(tree) = lower_promql(q, accuracy.clone()) else {
            continue;
        };
        match bind(&tree) {
            Ok(bound) if matches!(bound.expr, SummaryExpr::KeepPreAsap(_)) => t.unchanged += 1,
            Ok(_) => t.transformed += 1,
            Err(_) => t.errored += 1,
        }
    }
    t
}

#[test]
fn binding_is_total_over_the_entire_corpus() {
    let accuracy = AccuracyTarget::Epsilon(0.01);
    let docs = bind_tally(DOCS, accuracy.clone());
    let td = bind_tally(TESTDATA, accuracy);
    eprintln!("docs corpus post-ASAP binding:     {docs:?}");
    eprintln!("testdata corpus post-ASAP binding: {td:?}");

    // Same totality guarantee as lowering: reaching here means `bind`
    // never panicked over any lowerable query in the corpus.
    assert_eq!(docs.errored, 0, "post-ASAP binding errored: {docs:?}");
    assert_eq!(td.errored, 0, "post-ASAP binding errored: {td:?}");
}

#[test]
fn lowering_is_total_over_the_entire_corpus() {
    let docs = tally(DOCS);
    let td = tally(TESTDATA);
    eprintln!("docs corpus:     {docs:?}");
    eprintln!("testdata corpus: {td:?}");

    // Totality: reaching here means no query panicked. Sanity-check that every
    // query was classified into exactly one bucket.
    assert!(
        docs.total() >= 45,
        "docs corpus unexpectedly small: {docs:?}"
    );
    assert!(
        td.total() > 1500,
        "testdata corpus unexpectedly small: {td:?}"
    );

    // Coverage tripwire: a code change that breaks lowering for a large slice of
    // real PromQL trips this. Current numbers on the private promql-parser `asap`
    // branch: docs 48 lowered / 1 rejected, testdata 1512 lowered / 76 rejected /
    // 235 unparseable. The floors sit ~1% under those, so they guard regressions
    // rather than pin an exact count — ratchet them up as coverage lands.
    //
    // The 235 unparseable are parser-fork gaps (issue #108); the rejections are
    // lowering gaps (#109). Both shrink over time, so these floors only ever rise.
    assert!(
        docs.lowered >= 47,
        "docs lowering coverage regressed: {docs:?}"
    );
    assert!(
        td.lowered >= 1495,
        "testdata lowering coverage regressed: {td:?}"
    );
}
