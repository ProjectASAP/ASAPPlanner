//! `tools/dag-viewer` ↔ `asap_types::dag_export` contract: the viewer's
//! `KIND_CATEGORY_JSON` must categorize exactly the `kind` strings
//! [`asap_types::dag_export::export`] can emit — `Operator::kind_name()` of
//! every `NonASAPOp` and `ASAPOp` variant — no more (a stale kind the IR no
//! longer has) and no less (an exported kind the viewer would render
//! uncategorized).

use std::collections::{BTreeMap, BTreeSet};

use asap_types::ir::{ASAPOp, NonASAPOp};

/// Every `NonASAPOp::kind_name()`.
const NON_ASAP_KINDS: &[&str] = &[
    "Scan",
    "Values",
    "Filter",
    "Project",
    "Aggregate",
    "Join",
    "SetOp",
    "Concat",
    "Dedup",
    "Sort",
    "Limit",
    "BinaryOp",
    "SQLWindowFunc",
    "TimeRange",
    "TimeShift",
    "PromqlVectorFromScalar",
    "PromqlRelabel",
    "PromqlInfoEnrich",
    "PromqlSeriesSample",
    "PromqlSubquery",
    "ScalarBridge",
];

/// Every `ASAPOp::kind_name()`.
const ASAP_KINDS: &[&str] = &[
    "SummaryAgg",
    "SummaryEstimate",
    "FinalizeExactAccumulator",
    "MaintainPopulation",
    "ReadPopulation",
    "SummaryMerge",
    "SummarySubtract",
    "SummaryDelete",
    "SummaryJoin",
    "Extension",
];

/// Compile-time tripwire: adding an operator variant fails these exhaustive
/// matches until the matching `*_KINDS` list above is extended too. Never
/// called; the match arms are the point.
#[allow(dead_code)]
fn kind_lists_track_every_variant(non_asap: &NonASAPOp, asap: &ASAPOp) {
    let listed = |name: &str, list: &[&str]| assert!(list.contains(&name));
    listed(
        match non_asap {
            NonASAPOp::Scan { .. } => "Scan",
            NonASAPOp::Values { .. } => "Values",
            NonASAPOp::Filter { .. } => "Filter",
            NonASAPOp::Project { .. } => "Project",
            NonASAPOp::Aggregate { .. } => "Aggregate",
            NonASAPOp::Join { .. } => "Join",
            NonASAPOp::SetOp { .. } => "SetOp",
            NonASAPOp::Concat { .. } => "Concat",
            NonASAPOp::Dedup { .. } => "Dedup",
            NonASAPOp::Sort { .. } => "Sort",
            NonASAPOp::Limit { .. } => "Limit",
            NonASAPOp::BinaryOp { .. } => "BinaryOp",
            NonASAPOp::SQLWindowFunc { .. } => "SQLWindowFunc",
            NonASAPOp::TimeRange { .. } => "TimeRange",
            NonASAPOp::TimeShift { .. } => "TimeShift",
            NonASAPOp::PromqlVectorFromScalar(_) => "PromqlVectorFromScalar",
            NonASAPOp::PromqlRelabel { .. } => "PromqlRelabel",
            NonASAPOp::PromqlInfoEnrich { .. } => "PromqlInfoEnrich",
            NonASAPOp::PromqlSeriesSample { .. } => "PromqlSeriesSample",
            NonASAPOp::PromqlSubquery { .. } => "PromqlSubquery",
            NonASAPOp::ScalarBridge(_) => "ScalarBridge",
        },
        NON_ASAP_KINDS,
    );
    listed(
        match asap {
            ASAPOp::SummaryAgg { .. } => "SummaryAgg",
            ASAPOp::SummaryEstimate { .. } => "SummaryEstimate",
            ASAPOp::FinalizeExactAccumulator { .. } => "FinalizeExactAccumulator",
            ASAPOp::MaintainPopulation { .. } => "MaintainPopulation",
            ASAPOp::ReadPopulation { .. } => "ReadPopulation",
            ASAPOp::SummaryMerge { .. } => "SummaryMerge",
            ASAPOp::SummarySubtract { .. } => "SummarySubtract",
            ASAPOp::SummaryDelete { .. } => "SummaryDelete",
            ASAPOp::SummaryJoin { .. } => "SummaryJoin",
            ASAPOp::Extension { .. } => "Extension",
        },
        ASAP_KINDS,
    );
}

/// The viewer's `kind -> category` table, parsed out of the JS source the
/// same way the viewer itself does (`JSON.parse(KIND_CATEGORY_JSON)`).
fn viewer_kind_categories() -> BTreeMap<String, String> {
    const START: &str = "const KIND_CATEGORY_JSON = `";
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/dag-viewer/node-style.js"
    ));
    let json = source
        .split_once(START)
        .expect("node-style.js must declare KIND_CATEGORY_JSON")
        .1
        .split_once("`;")
        .expect("KIND_CATEGORY_JSON must be a template literal")
        .0;
    serde_json::from_str(json).expect("KIND_CATEGORY_JSON must be valid JSON")
}

#[test]
fn viewer_categorizes_exactly_the_exported_node_kinds() {
    let expected: BTreeSet<&str> = NON_ASAP_KINDS.iter().chain(ASAP_KINDS).copied().collect();
    assert_eq!(
        expected.len(),
        NON_ASAP_KINDS.len() + ASAP_KINDS.len(),
        "exported kind names must be unique"
    );
    let categories = viewer_kind_categories();
    let actual: BTreeSet<&str> = categories.keys().map(String::as_str).collect();

    assert_eq!(actual, expected);
}
