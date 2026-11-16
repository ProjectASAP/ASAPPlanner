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
    sql_pass2_over(
        &[(
            &query,
            json!({"max_error": 0.01, "failure_probability": 0.01, "accuracy_metric": "rank"}),
        )],
        json!({"flows": {
            "series_count": 6,
            "distinct_values_per_label": src_values.map_or(json!({}), |src| json!({"src": src})),
        }}),
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

/// Grouped by the source column `src` alone, the result holds at most the
/// 6 groups of rows `flows` declares. With no `src` count declared, the
/// query's value count is unknown, but the readout's δ still splits over
/// R = 6: 0.01 / 6.
#[test]
fn a_whole_result_grouped_by_a_source_column_splits_over_the_groups_of_rows() {
    let logical = whole_result_quantile_grouped_by("src", None);
    let readouts = kll_readouts(&logical);
    assert!(readouts.iter().any(|(live, _)| *live), "{readouts:?}");
    for (_, failure_probability) in readouts {
        let failure_probability = failure_probability.expect("a level");
        assert!(
            (failure_probability - 0.01 / 6.0).abs() < 1e-12,
            "{failure_probability}"
        );
    }
}
