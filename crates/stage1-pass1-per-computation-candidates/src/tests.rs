//! Pass 1's tests. Each builds its input memo through `Memo::find_or_add_group`
//! and `Memo::add_query`, as Stage 0 does, and reads the candidates
//! [`pass1_memo`](crate::pass1_memo) adds; no other stage crate is called.
mod candidates;
mod fused;
mod having;
mod intents;
mod nested_merges;
mod running_example;
mod series_identity;
mod summary_realization;
mod targets;
mod unsupported_ways;
mod update_weights;

use std::fmt::Write as _;

use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, TimeRangeKind};
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::summary_type::{SketchAlgorithm, SummaryUpdate};
use asap_types::test_support::memo_builders::{group, groups_where, scan_group};
use std::time::Duration;

/// A compact printout of every group and its candidates: the expression's
/// operator, then per candidate its operator, input groups, θ and fused
/// groups.
fn memo_printout(memo: &Memo) -> String {
    let mut out = String::new();
    for (index, group) in memo.groups().iter().enumerate() {
        let _ = writeln!(out, "G{index}: {}", label(&group.expression));
        for (position, candidate) in group.candidates.iter().enumerate() {
            let inputs: Vec<String> = candidate
                .expression
                .operator
                .children()
                .iter()
                .map(|g| format!("G{}", g.0))
                .collect();
            let mut line = format!(
                "  [{position}] {} inputs=[{}]",
                label(&candidate.expression.operator),
                inputs.join(", ")
            );
            if let Some(parameters) = candidate.logical_properties.parameters() {
                let _ = write!(line, " θ={parameters:?}");
            }
            if !candidate.fused_groups.is_empty() {
                let _ = write!(line, " fused_groups={:?}", candidate.fused_groups);
            }
            let _ = writeln!(out, "{line}");
        }
    }
    out
}

fn label(operator: &Operator<MemoGroupId>) -> String {
    match operator {
        Operator::ASAP(ASAPOp::SummaryAgg { summary_type, .. }) => {
            format!("SummaryAgg[{summary_type:?}]")
        }
        Operator::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
            format!("SummaryEstimate[{query:?}]")
        }
        Operator::NonASAP(NonASAPOp::Aggregate { measures, .. }) => {
            let names: Vec<String> = measures
                .iter()
                .map(|m| {
                    format!("{m:?}")
                        .split([' ', '{'])
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
                .collect();
            format!("Aggregate[{}]", names.join(", "))
        }
        operator => operator.kind_name().to_string(),
    }
}

/// The state group of `algorithm` in `memo`, and its build's update.
fn state_group(memo: &Memo, algorithm: SketchAlgorithm) -> (MemoGroupId, SummaryUpdate) {
    let found = groups_where(
        memo,
        |expression| matches!(expression, Operator::ASAP(ASAPOp::SummaryAgg { summary_type: FieldDataType::Sketch(a), .. }) if *a == algorithm),
    );
    let [state] = found[..] else {
        panic!(
            "one {algorithm:?} state group: {found:?}\n{}",
            memo_printout(memo)
        )
    };
    let Operator::ASAP(ASAPOp::SummaryAgg { input, .. }) =
        &memo.groups()[state.0 as usize].expression
    else {
        unreachable!()
    };
    (state, input.clone())
}

fn quantile(q: f64) -> AggIntent {
    AggIntent::Quantile { col: None, q }
}

fn sum() -> AggIntent {
    AggIntent::Sum { col: None }
}

fn topk(k: usize) -> AggIntent {
    AggIntent::TopK { k }
}

/// `metric[5m]` (`Range`) or `metric` (`Instant`, which a cross-series
/// aggregate reads): a selector over `metric{labels}`.
fn selector(memo: &mut Memo, kind: TimeRangeKind, metric: &str, labels: &[&str]) -> MemoGroupId {
    let scan = scan_group(memo, metric, labels, vec![]);
    group(
        memo,
        Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind,
            child: scan,
        }),
    )
}
