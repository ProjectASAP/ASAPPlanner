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

use asap_frontend_promql::PromqlError as LoweringError;
#[path = "../support.rs"]
mod support;
use asap_types::ir::properties::AccuracyTarget;

const DOCS: &str = include_str!("data/promql_corpus_docs.txt");
const TESTDATA: &str = include_str!("data/promql_corpus_testdata.txt");

/// Non-comment, non-blank query lines.
fn queries(corpus: &str) -> impl Iterator<Item = &str> {
    corpus
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
}

/// The lowering error of a selector with no metric name (`{job="a"}`).
const NO_METRIC_NAME: &str = "selector without one exact metric name";

#[derive(Default, Debug)]
struct Tally {
    lowered: usize,
    /// Rejected because a selector has no metric name: expected, since a
    /// `Source::TimeSeries` names exactly one metric.
    no_metric_name: usize,
    /// Rejected for any other unsupported feature.
    rejected: usize,
    unparseable: usize,
}

impl Tally {
    fn total(&self) -> usize {
        self.lowered + self.no_metric_name + self.rejected + self.unparseable
    }
}

/// Lower every query; a panic here fails the test (the totality guarantee).
fn tally(corpus: &str) -> Tally {
    let mut t = Tally::default();
    for q in queries(corpus) {
        match asap_frontend_promql::lower_promql_query_workload(&support::workload(
            q,
            AccuracyTarget::exact(),
        )) {
            Ok(_) => t.lowered += 1,
            Err(LoweringError::Parse(_)) => t.unparseable += 1,
            Err(e) if e.to_string().contains(NO_METRIC_NAME) => t.no_metric_name += 1,
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

    // Coverage tripwire: docs 48 lowered / 1 rejected; testdata 1190 lowered
    // / 29 without a metric name / 371 rejected / 233 parser gaps. The corpus
    // declares no metric types, so `histogram_quantile(s)` over any metric
    // lowers as classic buckets, as the PromQL spec reads float samples
    // (issue #1165: 93 testdata queries more than when undeclared input
    // needed a `_bucket` name, `by (le)` or `le` matcher). Native histogram
    // accessors (`histogram_count`, ...) stay rejected: accepting them
    // requires a native histogram sample representation.
    //
    // Expected unsupported: the 29 testdata queries whose selectors name no
    // metric (`sum({idx="0"})`, `absent_over_time({job="grok"}[20m])`,
    // `rate({env="1"}[10m])`, ...). Such a selector reads every metric, and a
    // `Source::TimeSeries` names exactly one, so lowering rejects it as the
    // MetricsQL frontend does.
    assert_eq!(docs.no_metric_name, 0, "{docs:?}");
    assert_eq!(td.no_metric_name, 29, "{td:?}");
    assert!(
        docs.lowered >= 47,
        "docs lowering coverage regressed: {docs:?}"
    );
    assert!(
        td.lowered >= 1190,
        "testdata lowering coverage regressed: {td:?}"
    );
}
