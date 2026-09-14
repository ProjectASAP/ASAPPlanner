use std::rc::Rc;

use asap_aware_mapping::replacement::{ReplacementStrategy, TargetSubDAG};
use asap_aware_mapping::rewrite::AvgToSumOverCountStrategy;
use asap_frontend_promql::lower_promql;
use asap_types::types::AccuracyTarget;

/// Unbounded Float64 ranges must retain AVG: finite samples can overflow SUM.
/// The independent Prometheus oracle is fixtures/avg_over_time_overflow.test.yml.
#[test]
fn range_average_does_not_offer_unconditional_sum_count() {
    for query in [
        "avg_over_time(latency[5m])",
        "avg_over_time(latency{job=\"api\"}[5m])",
        "avg_over_time(latency[5m:1m])",
    ] {
        let root = Rc::new(lower_promql(query, AccuracyTarget::Exact).unwrap());
        let target = TargetSubDAG::new(&root);
        assert!(
            !AvgToSumOverCountStrategy.matches(&target),
            "unbounded average must not match: {query}"
        );
        assert!(
            AvgToSumOverCountStrategy.replacements(&target).is_empty(),
            "unbounded average must not produce a sum/count rewrite: {query}"
        );
    }
}
