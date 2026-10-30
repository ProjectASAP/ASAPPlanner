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
use support::{logical_plan, lower_promql};

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
        match asap_frontend_promql::lower_promql_query_workload(
            &support::workload(q, AccuracyTarget::exact()),
            &asap_frontend_promql::HistogramCatalog::default(),
        ) {
            Ok(_) => t.lowered += 1,
            Err(LoweringError::Parse(_)) => t.unparseable += 1,
            Err(e) if e.to_string().contains(NO_METRIC_NAME) => t.no_metric_name += 1,
            Err(_) => t.rejected += 1,
        }
    }
    t
}

/// Every query that lowers, additionally run through logical ASAP-aware
/// optimization ([`logical_plan`]: Pass 1 and composition), at an
/// approximate accuracy target so summary candidates actually appear (an
/// `Exact` target would only ever exercise the exact-accumulator arm).
#[derive(Default, Debug)]
struct BindTally {
    /// An ASAP operator was bound somewhere below the root — the pass did
    /// something.
    transformed: usize,
    /// The kept pre-ASAP dag — the pass left the query untouched.
    unchanged: usize,
    /// [`logical_plan`] returned `Err`: Pass 1 rejected the query, or its
    /// exact candidate did not compose.
    errored: usize,
}

fn bind_tally(corpus: &str, accuracy: AccuracyTarget) -> BindTally {
    let mut t = BindTally::default();
    for q in queries(corpus) {
        let Ok(dag) = lower_promql(q, accuracy.clone()) else {
            continue;
        };
        match logical_plan(&dag) {
            Ok(bound) if !bound.contains_asap() => t.unchanged += 1,
            Ok(_) => t.transformed += 1,
            Err(_) => t.errored += 1,
        }
    }
    t
}

#[test]
fn binding_is_total_over_the_entire_corpus() {
    let accuracy = AccuracyTarget::max_error(0.01);
    let docs = bind_tally(DOCS, accuracy.clone());
    let td = bind_tally(TESTDATA, accuracy);
    eprintln!("docs corpus post-ASAP binding:     {docs:?}");
    eprintln!("testdata corpus post-ASAP binding: {td:?}");

    // Same totality guarantee as lowering: reaching here means `logical_plan`
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

    // Coverage tripwire after rejecting unrepresented native histogram samples:
    // docs 48 lowered / 1 rejected; testdata 1097 lowered / 29 without a
    // metric name / 464 rejected / 233 parser gaps. Earlier coverage counted
    // native histogram operations incorrectly treated as float quantiles. Keep
    // the rejection cases in the corpus: accepting them requires a native
    // histogram sample representation.
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
        td.lowered >= 1097,
        "testdata lowering coverage regressed: {td:?}"
    );
}
