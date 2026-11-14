use super::*;
use crate::planner_fuzz::generate::{generated_workload, SqlSpec};
use asap_types::accuracy::AccuracyTarget;

fn promql(spec: PromqlSpec, accuracy: AccuracyTarget) -> FuzzWorkload {
    generated_workload(
        Language::Promql,
        vec![FuzzQuery::generated(
            QuerySpec::Promql(spec),
            accuracy,
            10_000,
            None,
        )],
        1_000,
        15_000,
    )
}

fn sum_by(labels: &[&str]) -> PromqlSpec {
    PromqlSpec {
        metric: "m_counter".into(),
        matchers: vec![],
        range: Some(RangeFunction::Rate),
        window_ms: 300_000,
        outer: Some(OuterAggregation {
            op: OuterOp::Sum,
            labels: labels.iter().map(|l| l.to_string()).collect(),
            without: false,
        }),
    }
}

/// Each marginal relation derives the documented `q'` from `q`.
#[test]
fn marginal_relations_derive_the_documented_query() {
    let w = promql(sum_by(&["job", "instance"]), AccuracyTarget::exact());
    let text = |r| derive(r, &w, 0).map(|d| d.query.text().to_string());
    assert_eq!(
        text(Relation::FilterOnGroupKey).as_deref(),
        Some("sum by (job, instance) (rate(m_counter{job=\"job1\"}[5m]))")
    );
    let coarser = derive(Relation::CoarserGrouping, &w, 0).unwrap();
    assert_eq!(coarser.query.text(), "sum by (job) (rate(m_counter[5m]))");
    assert_eq!(
        coarser.witnesses[0],
        Witness::ExactRollup {
            labels: vec!["job".into()],
            op: RollupOp::Sum
        }
    );
    // A counter's rate reads more than merged tumbling windows: no nested window.
    assert_eq!(text(Relation::NestedShorterWindow), None);
    assert_eq!(text(Relation::OtherQuantileRank), None);
    assert_eq!(
        text(Relation::OuterOverResult).as_deref(),
        Some("topk(3, sum by (job, instance) (rate(m_counter[5m])))")
    );
    assert_eq!(
        text(Relation::ExactDuplicate).as_deref(),
        Some(w.queries[0].text())
    );
    let looser = derive(Relation::LooserAccuracy, &w, 0).unwrap();
    assert_eq!(looser.query.accuracy().max_error, 0.01);
    let interval = derive(Relation::OtherRepeatInterval, &w, 0).unwrap();
    assert_eq!(repeat_interval_ms(&interval.query), Some(30_000));
}

/// Quantile, top-k, count and union derivations, PromQL and SQL.
#[test]
fn statistic_and_selection_relations_derive() {
    let mut quantile = sum_by(&["job"]);
    quantile.metric = "m_gauge".into();
    quantile.range = Some(RangeFunction::QuantileOverTime(0.9));
    quantile.outer = None;
    quantile.matchers = vec![("job".into(), "job1".into())];
    let w = promql(quantile, AccuracyTarget::max_error(0.01));
    let rank = derive(Relation::OtherQuantileRank, &w, 0).unwrap();
    assert_eq!(
        rank.query.text(),
        "quantile_over_time(0.5, m_gauge{job=\"job1\"}[5m])"
    );
    assert_eq!(
        rank.witnesses,
        vec![Witness::QuantileRank { from: 0.9, to: 0.5 }]
    );
    // A quantile sketch answers only quantiles: (e), not (h).
    assert!(derive(Relation::OtherStatistic, &w, 0).is_none());
    let union = derive(Relation::UnionOfSelections, &w, 0).unwrap();
    assert_eq!(
        union.base_addition.unwrap().text(),
        "quantile_over_time(0.9, m_gauge{job=\"job2\"}[5m])"
    );
    assert_eq!(
        union.query.text(),
        "quantile_over_time(0.9, m_gauge{job=~\"job1|job2\"}[5m])"
    );

    let mut topk = sum_by(&["job"]);
    topk.outer.as_mut().unwrap().op = OuterOp::Topk(10);
    let w = promql(topk, AccuracyTarget::exact());
    let smaller = derive(Relation::SmallerTopK, &w, 0).unwrap();
    assert_eq!(
        smaller.query.text(),
        "topk by (job) (3, rate(m_counter[5m]))"
    );

    let sql = SqlSpec {
        table: "metrics".into(),
        aggregate: SqlAggregate::Sum,
        group_by: vec!["city".into(), "host".into()],
        filters: vec![],
        window_ms: 300_000,
    };
    let w = generated_workload(
        Language::Sql,
        vec![FuzzQuery::generated(
            QuerySpec::Sql(sql),
            AccuracyTarget::exact(),
            10_000,
            None,
        )],
        100,
        10_000,
    );
    let filtered = derive(Relation::FilterOnGroupKey, &w, 0).unwrap();
    assert!(filtered
        .query
        .text()
        .contains("AND city = 'city1' GROUP BY city, host"));
    let coarser = derive(Relation::CoarserGrouping, &w, 0).unwrap();
    assert!(coarser.query.text().ends_with("GROUP BY city"));
}

/// The relations taken from the ASAPWorkloads query catalog derive its
/// shapes: a ratio over q, a longer window (rolled up), `rate` as
/// `increase`, and an outer operator that follows q's own.
#[test]
fn workload_catalog_relations_derive() {
    let rate = promql(sum_by(&["job", "instance"]), AccuracyTarget::exact());
    let text = |w: &FuzzWorkload, r| derive(r, w, 0).map(|d| d.query.text().to_string());
    assert_eq!(
        text(&rate, Relation::BinaryOverResult).as_deref(),
        Some(
            "sum by (job, instance) (rate(m_counter{region=\"region1\"}[5m])) / \
             sum by (job, instance) (rate(m_counter[5m]))"
        )
    );
    let increase = derive(Relation::EquivalentRangeFunction, &rate, 0).unwrap();
    assert_eq!(
        increase.query.text(),
        "sum by (job, instance) (increase(m_counter[5m]))"
    );
    assert_eq!(
        increase.witnesses,
        vec![Witness::Graft {
            text: "(sum by (job, instance) (rate(m_counter[5m]))) * 300".into()
        }]
    );
    // As for (d), a rate's window is not merged tumbling-window state.
    assert_eq!(text(&rate, Relation::LongerWindow), None);

    let mut sum = sum_by(&["job", "instance"]);
    sum.metric = "m_gauge".into();
    sum.range = Some(RangeFunction::SumOverTime);
    sum.outer.as_mut().unwrap().op = OuterOp::Max;
    let w = promql(sum, AccuracyTarget::exact());
    assert_eq!(
        text(&w, Relation::BinaryOverResult).as_deref(),
        // Not an average under max: the filtered share instead.
        Some(
            "max by (job, instance) (sum_over_time(m_gauge{region=\"region1\"}[5m])) / \
             max by (job, instance) (sum_over_time(m_gauge[5m]))"
        )
    );
    assert_eq!(
        text(&w, Relation::OuterOverResult).as_deref(),
        Some("max(max by (job, instance) (sum_over_time(m_gauge[5m])))")
    );
    assert_eq!(
        text(&w, Relation::LongerWindow).as_deref(),
        Some("max by (job, instance) (sum_over_time(m_gauge[10m]))")
    );
    let rollup = derive(Relation::LongerWindowRollup, &w, 0).unwrap();
    assert_eq!(
        rollup.query.text(),
        "max by (job) (sum_over_time(m_gauge[10m]))"
    );
    assert_eq!(
        rollup.witnesses[0],
        Witness::LongerWindow {
            window_ms: 600_000,
            rollup: Some(Box::new(Witness::ExactRollup {
                labels: vec!["job".into()],
                op: RollupOp::Max
            }))
        }
    );
    // (h): one UnivMon answers the count, distinct count, L2 and
    // entropy; q' asks for another than q.
    let entropy = SqlSpec {
        table: "metrics".into(),
        aggregate: SqlAggregate::FrequencyEntropy("src_ip".into()),
        group_by: vec![],
        filters: vec![],
        window_ms: 60_000,
    };
    let w = generated_workload(
        Language::Sql,
        vec![FuzzQuery::generated(
            QuerySpec::Sql(entropy),
            AccuracyTarget::max_error(0.05),
            10_000,
            None,
        )],
        100_000,
        15_000,
    );
    let h = derive(Relation::OtherStatistic, &w, 0).unwrap();
    assert_eq!(
        h.query.text(),
        "SELECT SQRT(SUM(CAST(c AS DOUBLE) * CAST(c AS DOUBLE))) AS v FROM \
         (SELECT src_ip, COUNT(*) AS c FROM metrics WHERE ts > now() - INTERVAL '1' \
         MINUTE GROUP BY src_ip)"
    );
    assert_eq!(h.witnesses[1], Witness::UnivMonOnly);
    assert_eq!(longer_window(600_000), Some(1_800_000));
    assert_eq!(longer_window(3_600_000), None);
}

/// Renaming renames sources, labels, facts and tables; a disjoint copy
/// doubles the queries, sources and arrival rate.
#[test]
fn rename_and_copy_rewrite_facts() {
    let w = promql(sum_by(&["job"]), AccuracyTarget::exact());
    let r = renamed(&w, "_r", true).unwrap();
    assert_eq!(
        r.queries[0].text(),
        "sum by (job_r) (rate(m_counter_r[5m]))"
    );
    assert!(r.data.sources["m_counter_r"]
        .distinct_values_per_label
        .contains_key("job_r"));
    let both = disjoint_copy(&w).unwrap();
    assert_eq!(both.queries.len(), 2);
    assert_eq!(both.data.sources.len(), 4);
    assert_eq!(
        both.data.arrival.rows_per_second(),
        w.data.arrival.rows_per_second().map(|r| 2.0 * r)
    );
}

/// Every relation has a distinct name that parses back, and letters
/// parse for the marginal ones.
#[test]
fn relation_names_parse() {
    for relation in Relation::ALL {
        assert_eq!(Relation::parse(relation.name()), Some(relation));
    }
    assert_eq!(Relation::parse("a"), Some(Relation::LooserAccuracy));
    assert_eq!(Relation::parse("k"), Some(Relation::UnionOfSelections));
    assert_eq!(Relation::parse("z"), None);
}
