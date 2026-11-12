//! PromQL → the groups each query may read only exactly, through
//! production lowering: a query never reads a sketch readout whose error
//! metric its target does not allow, and a dead candidate makes no group
//! exact-only.
use crate::planning_common::promql_pass2;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{CandidateForMemoGroupId, LogicalASAPMemoCandidates, MemoGroupId};
use asap_types::summary_type::SketchAlgorithm;
use serde_json::json;

/// The readout groups (`SummaryEstimate`) over a state of `algorithm`.
fn readout_groups(
    logical: &LogicalASAPMemoCandidates,
    algorithm: SketchAlgorithm,
) -> Vec<MemoGroupId> {
    let memo = &logical.memo;
    (0..memo.groups().len())
        .map(|group| MemoGroupId(group as u32))
        .filter(|group| match &memo.groups()[group.0 as usize].expression {
            Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => memo.groups()
                [summary_input.0 as usize]
                .group_properties
                .schema
                .fields
                .iter()
                .any(|field| field.dtype == FieldDataType::Sketch(algorithm)),
            _ => false,
        })
        .collect()
}

/// Whether some candidate of `group` survives Pass 2.
fn live(logical: &LogicalASAPMemoCandidates, group: MemoGroupId) -> bool {
    (0..logical.memo.groups()[group.0 as usize].candidates.len()).any(|index_in_group| {
        let candidate = CandidateForMemoGroupId {
            group,
            index_in_group,
        };
        logical
            .pruned
            .iter()
            .all(|pruned| pruned.candidate != candidate)
    })
}

/// `quantile_over_time(0.9, m[5m])` twice, Q0 at rank error 0.01 and Q1 at
/// relative error 0.05: both share the quantile group and both the KLL
/// (rank) and DDSketch (relative) readouts survive, but Q0 reads only the
/// KLL readout and Q1 only the DDSketch one; each reads the other's
/// readout groups only exactly.
#[test]
fn each_query_reads_only_the_readouts_in_its_targets_metric() {
    let query = "quantile_over_time(0.9, m[5m])";
    let logical = promql_pass2(&[
        (query, json!({"max_error": 0.01, "accuracy_metric": "rank"})),
        (
            query,
            json!({"max_error": 0.05, "accuracy_metric": "relative"}),
        ),
    ]);
    let exact_only = &logical.exact_only_groups_of_query;
    let kll = readout_groups(&logical, SketchAlgorithm::Kll);
    let ddsketch = readout_groups(&logical, SketchAlgorithm::DDSketch);
    assert!(!kll.is_empty() && !ddsketch.is_empty());
    for (group, rank_query_reads_it) in kll
        .iter()
        .map(|&group| (group, true))
        .chain(ddsketch.iter().map(|&group| (group, false)))
    {
        assert!(live(&logical, group), "G{} survives", group.0);
        assert_eq!(
            !exact_only[0].contains(&group),
            rank_query_reads_it,
            "G{}",
            group.0
        );
        assert_eq!(
            !exact_only[1].contains(&group),
            !rank_query_reads_it,
            "G{}",
            group.0
        );
    }
}

/// `count(quantile_over_time(0.9, m[5m]))` at relative error 0.05: the
/// count reads the quantiles through an exact count, whose contract
/// passes every metric, while Pass 1's Count-Min and UnivMon builds over
/// the quantiles (no contract) are dead. A dead candidate makes no group
/// exact-only: the quantile group and the DDSketch readout stay readable.
#[test]
fn a_dead_candidate_makes_no_group_exact_only() {
    let logical = promql_pass2(&[(
        "count(quantile_over_time(0.9, m[5m]))",
        json!({"max_error": 0.05, "failure_probability": 0.01, "accuracy_metric": "relative"}),
    )]);
    let memo = &logical.memo;
    let quantile = (0..memo.groups().len())
        .map(|group| MemoGroupId(group as u32))
        .find(|group| {
            matches!(
                memo.groups()[group.0 as usize].expression,
                Operator::NonASAP(NonASAPOp::Aggregate { ref measures, .. })
                    if matches!(measures[..], [asap_types::ir::operator::AggIntent::Quantile { .. }])
            )
        })
        .expect("the quantile group");
    let ddsketch = readout_groups(&logical, SketchAlgorithm::DDSketch);
    assert!(!ddsketch.is_empty());
    for group in ddsketch.into_iter().chain([quantile]) {
        assert!(live(&logical, group), "G{} survives", group.0);
        assert!(
            !logical.exact_only_groups_of_query[0].contains(&group),
            "G{} is exact-only",
            group.0
        );
    }
}
