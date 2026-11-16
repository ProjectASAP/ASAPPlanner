//! SQL → Pass 2's readout requirements, through production lowering.
use crate::planning_common::{sql_pass2, sql_pass2_over};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{CandidateForMemoGroupId, LogicalASAPMemoCandidates, MemoGroupId};
use asap_types::summary_type::{SketchAlgorithm, SketchParams};
use serde_json::json;

/// `SELECT count(DISTINCT dst) FROM flows` at relative error 0.05 and
/// δ = 0.01 reads one distinct count per evaluation, so its KMV readout
/// keeps δ = 0.01 rather than splitting it over the 1000 groups of rows
/// scanned (1e-5), and a KMV build of k = 4323, within 0.05 at δ = 0.01,
/// survives.
#[test]
fn a_global_distinct_count_keeps_its_whole_failure_probability() {
    let logical = sql_pass2(&[(
        "SELECT count(DISTINCT dst) FROM flows",
        json!({"max_error": 0.05, "failure_probability": 0.01, "accuracy_metric": "relative"}),
    )]);
    let memo = &logical.memo;
    let mut kmv_readouts = 0;
    let mut live_kmv_builds = Vec::new();
    for (group, memo_group) in memo.groups().iter().enumerate() {
        for (index_in_group, candidate) in memo_group.candidates.iter().enumerate() {
            let id = CandidateForMemoGroupId {
                group: MemoGroupId(group as u32),
                index_in_group,
            };
            let live = logical.pruned.iter().all(|pruned| pruned.candidate != id);
            match &candidate.expression.operator {
                Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. })
                    if memo.groups()[summary_input.0 as usize]
                        .group_properties
                        .sketch_algorithm_of_state()
                        == Some(SketchAlgorithm::Kmv) =>
                {
                    let required = candidate.input_requirements[0]
                        .required_accuracy
                        .expect("a requirement");
                    assert!(
                        (required.failure_probability - 0.01).abs() < 1e-12,
                        "{required:?}"
                    );
                    kmv_readouts += 1;
                }
                Operator::ASAP(ASAPOp::SummaryAgg {
                    summary_type: FieldDataType::Sketch(SketchAlgorithm::Kmv),
                    ..
                }) if live => {
                    if let Some(SketchParams::Kmv { k }) = candidate.logical_properties.parameters()
                    {
                        live_kmv_builds.push(*k);
                    }
                }
                _ => {}
            }
        }
    }
    assert!(kmv_readouts > 0);
    assert!(live_kmv_builds.contains(&4323), "{live_kmv_builds:?}");
}

/// Each KLL readout candidate of `logical`: whether Pass 2 keeps it, and
/// the failure probability its state must meet, if it is given one.
fn kll_readouts(logical: &LogicalASAPMemoCandidates) -> Vec<(bool, Option<f64>)> {
    let memo = &logical.memo;
    let mut readouts = Vec::new();
    for (group, memo_group) in memo.groups().iter().enumerate() {
        for (index_in_group, candidate) in memo_group.candidates.iter().enumerate() {
            let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) =
                &candidate.expression.operator
            else {
                continue;
            };
            if memo.groups()[summary_input.0 as usize]
                .group_properties
                .sketch_algorithm_of_state()
                != Some(SketchAlgorithm::Kll)
            {
                continue;
            }
            let id = CandidateForMemoGroupId {
                group: MemoGroupId(group as u32),
                index_in_group,
            };
            let live = logical.pruned.iter().all(|pruned| pruned.candidate != id);
            let failure_probability = candidate.input_requirements[0]
                .required_accuracy
                .map(|required| required.failure_probability);
            readouts.push((live, failure_probability));
        }
    }
    assert!(!readouts.is_empty(), "a KLL readout");
    readouts
}

/// A 0.9-quantile of `bytes` at rank error 0.01 and δ = 0.01 over the
/// whole result, as `SELECT {keys}, approx_percentile_cont(bytes, 0.9)`
/// over the last 15 minutes grouped by `keys`, where `flows` declares 6
/// groups of rows, and `src_values` distinct `src` values when given.
fn whole_result_quantile_grouped_by(
    keys: &str,
    src_values: Option<u64>,
) -> LogicalASAPMemoCandidates {
    let query = format!(
        "SELECT {keys}, approx_percentile_cont(bytes, 0.9) AS q FROM flows \
         WHERE ts >= now() - INTERVAL '15 minutes' GROUP BY {keys}"
    );
    whole_result_quantile(
        &query,
        json!({"flows": {
            "series_count": 6,
            "distinct_values_per_label": src_values.map_or(json!({}), |src| json!({"src": src})),
        }}),
    )
}

/// `query`, a 0.9-quantile at rank error 0.01 and δ = 0.01 over the whole
/// result, over the facts `sources` declares.
fn whole_result_quantile(query: &str, sources: serde_json::Value) -> LogicalASAPMemoCandidates {
    sql_pass2_over(
        &[(
            query,
            json!({"max_error": 0.01, "failure_probability": 0.01, "accuracy_metric": "rank"}),
        )],
        sources,
    )
}

/// `GROUP BY date_trunc('minute', ts)` returns one quantile per minute
/// bucket: about 16 over the last 15 minutes, not at most the 6 groups of
/// rows `flows` declares. The readout values per evaluation are unknown
/// (the bucket is a computed key), and so is the query's value count, so
/// no per-value failure probability follows and the readout is read
/// exactly, rather than at δ / 6 (KLL k = 547, whose guarantee at δ / 16
/// is above the target).
#[test]
fn a_whole_result_grouped_by_a_time_bucket_gives_its_readout_no_level() {
    let logical = whole_result_quantile_grouped_by("date_trunc('minute', ts)", Some(6));
    for (live, failure_probability) in kll_readouts(&logical) {
        assert_eq!((live, failure_probability), (false, None));
    }
}

/// Grouped by `src` and the minute bucket, the result holds up to 6 × 16
/// values; the bucket key still leaves them unbounded, so no level.
#[test]
fn a_whole_result_grouped_by_a_source_column_and_a_time_bucket_gives_its_readout_no_level() {
    let logical = whole_result_quantile_grouped_by("src, date_trunc('minute', ts)", Some(6));
    for (live, failure_probability) in kll_readouts(&logical) {
        assert_eq!((live, failure_probability), (false, None));
    }
}

/// Grouped by the source column `src`, with 4 distinct `src` values
/// declared, the result holds at most 4 values: the readout's δ splits
/// over them, 0.01 / 4.
#[test]
fn a_whole_result_grouped_by_a_declared_source_column_splits_over_its_distinct_values() {
    let logical = whole_result_quantile_grouped_by("src", Some(4));
    assert_levels(&logical, &[0.01 / 4.0]);
}

/// `GROUP BY bytes` keys each quantile by a measure value: one group per
/// distinct `bytes`, and no count of distinct `bytes` is declared. The 6
/// groups of rows `flows` declares do not bound it (nothing says which
/// columns form them); the rows the query reads do, as there are never
/// more groups than rows. With no sampling interval declared, the rows
/// are unknown too, so the readout gets no per-value failure probability,
/// rather than δ / 6. Sampled every minute, the last 15 minutes hold at
/// most 6 × 16 = 96 rows: δ splits over 96.
#[test]
fn a_whole_result_grouped_by_an_undeclared_measure_column_splits_over_the_rows_read() {
    let by_bytes = |sample_interval: Option<u64>| {
        whole_result_quantile(
            "SELECT bytes, approx_percentile_cont(bytes, 0.9) AS q FROM flows \
             WHERE ts >= now() - INTERVAL '15 minutes' GROUP BY bytes",
            json!({"flows": {
                "series_count": 6,
                "sample_interval": sample_interval,
                "distinct_values_per_label": {"src": 6},
            }}),
        )
    };
    for (live, failure_probability) in kll_readouts(&by_bytes(None)) {
        assert_eq!((live, failure_probability), (false, None));
    }
    assert_levels(&by_bytes(Some(60_000)), &[0.01 / 96.0]);
}

/// `flows JOIN hosts` grouped by `f.src` and `h.region`: with 6 groups of
/// rows and 6 `src` values in `flows`, and 6 groups of rows, 6 hosts and
/// 6 regions in `hosts`, the result can hold 6 × 6 = 36 values (6 flows
/// to one host, and 6 host rows, one per region, for that host), not
/// 6 + 6 = 12: the readout's δ splits over 36. When the join reads fewer rows, they bound
/// the values instead: with `flows` holding 6 rows and `hosts` 5, the join
/// reads at most 30, and δ splits over 30.
#[test]
fn a_whole_result_over_a_join_splits_over_the_product_of_its_sides_values() {
    let join = |flows_rows: Option<u64>, hosts_rows: Option<u64>| {
        whole_result_quantile(
            "SELECT f.src, h.region, approx_percentile_cont(f.bytes, 0.9) AS q \
             FROM flows f JOIN hosts h ON f.dst = h.host \
             WHERE f.ts >= now() - INTERVAL '15 minutes' GROUP BY f.src, h.region",
            json!({
                "flows": {
                    "series_count": 6,
                    "rows_held": flows_rows,
                    "distinct_values_per_label": {"src": 6, "dst": 300},
                },
                "hosts": {
                    "series_count": 6,
                    "rows_held": hosts_rows,
                    "distinct_values_per_label": {"host": 6, "region": 6},
                },
            }),
        )
    };
    assert_levels(&join(None, None), &[0.01 / 36.0]);
    assert_levels(&join(Some(6), Some(5)), &[0.01 / 30.0]);
}

/// Asserts that the KLL readout candidates of `logical` keep at least
/// one, and that the failure probabilities they must meet are `expected`
/// (distinct, ascending), each candidate having one.
fn assert_levels(logical: &LogicalASAPMemoCandidates, expected: &[f64]) {
    let readouts = kll_readouts(logical);
    assert!(readouts.iter().any(|(live, _)| *live), "{readouts:?}");
    let mut levels: Vec<f64> = readouts
        .iter()
        .map(|(_, failure_probability)| failure_probability.expect("a level"))
        .collect();
    levels.sort_by(f64::total_cmp);
    levels.dedup_by(|a, b| (*a - *b).abs() < 1e-15);
    assert_eq!(levels.len(), expected.len(), "{levels:?}");
    for (level, expected) in levels.iter().zip(expected) {
        assert!((level - expected).abs() < 1e-12, "{levels:?}");
    }
}

/// $n_q$, the values one evaluation of the only query of `logical`
/// returns at most.
fn values_per_evaluation(logical: &LogicalASAPMemoCandidates) -> Option<u64> {
    logical.memo.queries()[0].number_of_values_per_query_evaluation
}

/// `GROUP BY dst`, with 300 distinct `dst` values declared, returns up to
/// 300 values: the key's declared values, not the 6 groups of rows
/// `flows` declares (nothing says which columns form them). The readout's
/// δ splits over all 300. Sampled every minute, the last 15 minutes hold
/// at most 6 × 16 = 96 rows, which bound the groups instead: δ / 96.
#[test]
fn a_result_grouped_by_a_declared_key_returns_up_to_its_declared_values() {
    let by_dst = |sample_interval: Option<u64>| {
        whole_result_quantile(
            "SELECT dst, approx_percentile_cont(bytes, 0.9) AS q FROM flows \
             WHERE ts >= now() - INTERVAL '15 minutes' GROUP BY dst",
            json!({"flows": {
                "series_count": 6,
                "sample_interval": sample_interval,
                "distinct_values_per_label": {"dst": 300},
            }}),
        )
    };
    let unsampled = by_dst(None);
    assert_eq!(values_per_evaluation(&unsampled), Some(300));
    assert_levels(&unsampled, &[0.01 / 300.0]);
    let sampled = by_dst(Some(60_000));
    assert_eq!(values_per_evaluation(&sampled), Some(96));
    assert_levels(&sampled, &[0.01 / 96.0]);
}

/// `GROUP BY bytes` keys each value by a measure with no declared count of
/// distinct values: only the rows read bound the result. Unknown without a
/// sampling interval; sampled every minute, at most 6 × 16 = 96 rows over
/// the last 15 minutes.
#[test]
fn a_result_grouped_by_an_undeclared_key_returns_at_most_the_rows_read() {
    let by_bytes = |sample_interval: Option<u64>| {
        whole_result_quantile(
            "SELECT bytes, approx_percentile_cont(bytes, 0.9) AS q FROM flows \
             WHERE ts >= now() - INTERVAL '15 minutes' GROUP BY bytes",
            json!({"flows": {
                "series_count": 6,
                "sample_interval": sample_interval,
                "distinct_values_per_label": {"src": 6},
            }}),
        )
    };
    assert_eq!(values_per_evaluation(&by_bytes(None)), None);
    assert_eq!(values_per_evaluation(&by_bytes(Some(60_000))), Some(96));
}

/// `flows JOIN hosts` grouped by `f.src` (6 declared) and `h.region` (6
/// declared) returns up to 6 × 6 = 36 values, the keys of different scans
/// multiplying; with `flows` holding 6 rows and `hosts` 5, the join reads
/// at most 30 rows, which bound it instead.
#[test]
fn a_result_grouped_by_keys_of_joined_scans_returns_up_to_their_product() {
    let join = |flows_rows: Option<u64>, hosts_rows: Option<u64>| {
        whole_result_quantile(
            "SELECT f.src, h.region, approx_percentile_cont(f.bytes, 0.9) AS q \
             FROM flows f JOIN hosts h ON f.dst = h.host \
             WHERE f.ts >= now() - INTERVAL '15 minutes' GROUP BY f.src, h.region",
            json!({
                "flows": {
                    "series_count": 6,
                    "rows_held": flows_rows,
                    "distinct_values_per_label": {"src": 6, "dst": 300},
                },
                "hosts": {
                    "series_count": 6,
                    "rows_held": hosts_rows,
                    "distinct_values_per_label": {"host": 6, "region": 6},
                },
            }),
        )
    };
    assert_eq!(values_per_evaluation(&join(None, None)), Some(36));
    assert_eq!(values_per_evaluation(&join(Some(6), Some(5))), Some(30));
}
