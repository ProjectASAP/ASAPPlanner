//! PromQL conformance over a small corpus derived from **grafana/o11y-bench**
//! task-grading rubrics.
//!
//! Source: <https://github.com/grafana/o11y-bench> — 27 deduplicated `fact.query`
//! values (`backend: prometheus`) pulled from `tasks-spec/**/*.yaml` grading
//! rubrics (`tests/data/o11y_bench_promql.txt`). These are SRE-style
//! incident/investigation queries: error ratios, burn rate, cache-lag and
//! retry-backlog triage, capacity checks.
//!
//! LICENSE NOTE: o11y-bench is AGPL-3.0, unlike this repo's other vendored
//! corpora (MIT/Apache-2.0). Whether copying these short query fixtures
//! verbatim into a differently-licensed test suite is fine as-is is an open
//! question — tracked in issue #135, not resolved by this file's existence.
//!
//! We *lower* (parse → the canonical tree), we do not execute. Totality: every query
//! returns `Ok` or a clean `LoweringError` and never panics. Given the corpus
//! is small and hand-picked from realistic incident-response queries, we also
//! assert full lowering coverage — a regression here means a real pattern
//! broke, not statistical noise.

use asap_frontend_promql::{lower_promql, PromqlError as LoweringError};
use asap_types::types::AccuracyTarget;

const CORPUS: &str = include_str!("data/o11y_bench_promql.txt");

/// Non-comment, non-blank query lines.
fn queries() -> impl Iterator<Item = &'static str> {
    CORPUS
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
}

#[test]
fn lowering_is_total_over_the_o11y_bench_corpus() {
    let mut lowered = 0;
    let mut rejected = Vec::new();

    for q in queries() {
        match lower_promql(q, AccuracyTarget::Exact) {
            Ok(_) => lowered += 1,
            Err(LoweringError::Parse(e)) => panic!("unexpected parse failure for {q:?}: {e}"),
            Err(e) => rejected.push((q, e)),
        }
    }

    assert!(
        rejected.is_empty(),
        "expected every o11y-bench query to lower cleanly, but {} were rejected: {rejected:#?}",
        rejected.len()
    );
    assert_eq!(
        lowered, 27,
        "corpus size changed — update this assertion if the corpus was intentionally edited"
    );
}
