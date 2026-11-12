//! SQL → Pass 2's readout requirements, through production lowering.
use crate::planning_common::sql_pass2;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{CandidateForMemoGroupId, MemoGroupId};
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
