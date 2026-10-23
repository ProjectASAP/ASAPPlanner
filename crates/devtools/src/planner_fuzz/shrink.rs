//! Greedy shrinking of a violated check: drop queries and simplify
//! expressions while the violation still reproduces.

use asap_types::ir::properties::AccuracyTarget;

use super::generate::{QuerySpec, RangeFunction};
use super::relations::{Check, Relation};

/// The one-step simplifications of `spec`, simplest first: fewer filters,
/// fewer grouping labels, no outer aggregation, a plain range function.
pub fn simplified_specs(spec: &QuerySpec) -> Vec<QuerySpec> {
    let mut out = Vec::new();
    match spec {
        QuerySpec::Promql(spec) => {
            for i in 0..spec.matchers.len() {
                let mut s = spec.clone();
                s.matchers.remove(i);
                out.push(QuerySpec::Promql(s));
            }
            if let Some(outer) = &spec.outer {
                for i in 0..outer.labels.len() {
                    let mut s = spec.clone();
                    s.outer.as_mut().expect("checked").labels.remove(i);
                    out.push(QuerySpec::Promql(s));
                }
                let mut s = spec.clone();
                s.outer = None;
                out.push(QuerySpec::Promql(s));
            }
            if matches!(
                spec.range,
                Some(
                    RangeFunction::AvgOverTime
                        | RangeFunction::MaxOverTime
                        | RangeFunction::CountOverTime
                )
            ) {
                let mut s = spec.clone();
                s.range = Some(RangeFunction::SumOverTime);
                out.push(QuerySpec::Promql(s));
            }
        }
        QuerySpec::Sql(spec) => {
            for i in 0..spec.filters.len() {
                let mut s = spec.clone();
                s.filters.remove(i);
                out.push(QuerySpec::Sql(s));
            }
            for i in 0..spec.group_by.len() {
                let mut s = spec.clone();
                s.group_by.remove(i);
                out.push(QuerySpec::Sql(s));
            }
        }
    }
    out
}

/// The one-step smaller checks of `check`: each other query dropped, then
/// per query its latency bound dropped, its accuracy made exact and its
/// spec simplified, then the added query's spec simplified.
pub fn smaller_checks(check: &Check) -> Vec<Check> {
    let mut out = Vec::new();
    let queries = &check.workload.queries;
    for i in (0..queries.len()).filter(|&i| i != check.target || !check.relation.per_query()) {
        if queries.len() == 1 {
            break;
        }
        let mut smaller = check.clone();
        smaller.workload = check.workload.without_query(i);
        if check.relation.per_query() && i < check.target {
            smaller.target -= 1;
        }
        out.push(smaller);
    }
    for (i, query) in queries.iter().enumerate() {
        if query.entry.requirements.max_latency_ms.is_some()
            && check.relation != Relation::RelaxedLatency
        {
            let mut unbounded = query.clone();
            unbounded.entry.requirements.max_latency_ms = None;
            let mut smaller = check.clone();
            smaller.workload = check.workload.with_query_replaced(i, unbounded);
            out.push(smaller);
        }
        if !query.accuracy().is_exact() {
            let mut smaller = check.clone();
            smaller.workload = check
                .workload
                .with_query_replaced(i, query.with_accuracy(AccuracyTarget::exact()));
            out.push(smaller);
        }
        for spec in query.spec.iter().flat_map(simplified_specs) {
            let mut smaller = check.clone();
            smaller.workload = check.workload.with_query_replaced(i, query.with_spec(spec));
            out.push(smaller);
        }
    }
    if let Some(added) = &check.added {
        for spec in added.spec.iter().flat_map(simplified_specs) {
            let mut smaller = check.clone();
            smaller.added = Some(added.with_spec(spec));
            out.push(smaller);
        }
    }
    out
}

/// Greedily shrinks `check`: takes the first smaller check that
/// `still_violated` accepts, until none does or `max_attempts` checks were
/// tried. Returns the smallest check found and the attempts made.
pub fn shrink(
    check: &Check,
    still_violated: &mut dyn FnMut(&Check) -> bool,
    max_attempts: usize,
) -> (Check, usize) {
    let mut current = check.clone();
    let mut attempts = 0;
    'outer: loop {
        for candidate in smaller_checks(&current) {
            if attempts == max_attempts {
                break 'outer;
            }
            attempts += 1;
            if still_violated(&candidate) {
                current = candidate;
                continue 'outer;
            }
        }
        break;
    }
    (current, attempts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner_fuzz::generate::{
        generated_workload, OuterAggregation, OuterOp, PromqlSpec,
    };
    use crate::planner_fuzz::{FuzzQuery, Language};

    fn query(metric: &str, labels: &[&str]) -> FuzzQuery {
        FuzzQuery::generated(
            QuerySpec::Promql(PromqlSpec {
                metric: metric.into(),
                matchers: vec![("region".into(), "region1".into())],
                range: Some(RangeFunction::Rate),
                window_ms: 300_000,
                outer: Some(OuterAggregation {
                    op: OuterOp::Sum,
                    labels: labels.iter().map(|l| l.to_string()).collect(),
                    without: false,
                }),
            }),
            AccuracyTarget::exact(),
            10_000,
            None,
        )
    }

    /// The shrinker drops the queries the violation does not need and
    /// simplifies the target as far as the violation allows: here the
    /// "violation" needs query `m_counter` grouped by `job`.
    #[test]
    fn shrinks_to_the_queries_and_parts_the_violation_needs() {
        let workload = generated_workload(
            Language::Promql,
            vec![
                query("m_gauge", &["job"]),
                query("m_counter", &["job", "instance"]),
                query("m_gauge", &["instance"]),
            ],
            1_000,
            15_000,
        );
        let check = Check {
            relation: Relation::FilterOnGroupKey,
            workload,
            target: 1,
            added: None,
        };
        let mut violated = |c: &Check| {
            let q = &c.workload.queries[c.target];
            q.text().contains("m_counter") && q.text().contains("by (job")
        };
        let (shrunk, attempts) = shrink(&check, &mut violated, 100);
        assert_eq!(shrunk.workload.queries.len(), 1);
        assert_eq!(shrunk.target, 0);
        assert_eq!(
            shrunk.workload.queries[0].text(),
            "sum by (job) (rate(m_counter[5m]))"
        );
        assert!(attempts <= 100);
    }

    /// The attempt budget bounds the work.
    #[test]
    fn shrinking_stops_at_the_attempt_budget() {
        let workload = generated_workload(
            Language::Promql,
            vec![query("m_gauge", &["job"]), query("m_counter", &["job"])],
            1_000,
            15_000,
        );
        let check = Check {
            relation: Relation::ExactDuplicate,
            workload,
            target: 0,
            added: None,
        };
        let (_, attempts) = shrink(&check, &mut |_| true, 2);
        assert_eq!(attempts, 2);
    }
}
