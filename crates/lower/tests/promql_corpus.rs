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

use asap_ir::types::AccuracyTarget;
use asap_control_lower::{lower_promql, LoweringError};

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
    // real PromQL trips this. Set well below the current numbers (docs≈29,
    // testdata≈574 lowered / 1014 rejected / 235 unparseable on the private
    // promql-parser `asap` branch); it guards regressions, not an exact count.
    assert!(
        docs.lowered >= 20,
        "docs lowering coverage regressed: {docs:?}"
    );
    assert!(
        td.lowered >= 520,
        "testdata lowering coverage regressed: {td:?}"
    );
}
