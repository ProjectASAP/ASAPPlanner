//! Pass 1's tests. Each builds its input memo through `Memo::find_or_add_group`
//! and `Memo::add_query`, as Stage 0 does, and reads the candidates
//! [`pass1_memo`](crate::pass1_memo) adds.
use std::fmt::Write as _;

use asap_types::ir::operator::operator_properties::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{Memo, MemoGroupId};

use crate::pass1_memo;
use crate::test_fixtures::{aggregate_group, query, scan_group};

/// A compact printout of every group and its candidates.
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
            let _ = writeln!(
                out,
                "  [{position}] {} inputs=[{}] θ={:?} floors={:?} fused={:?}",
                label(&candidate.expression.operator),
                inputs.join(", "),
                candidate.logical_properties.parameters(),
                candidate
                    .input_requirements
                    .iter()
                    .map(|r| r.parameter_floor_for_required_accuracy().cloned())
                    .collect::<Vec<_>>(),
                candidate.fused_groups,
            );
        }
    }
    out
}

fn label(operator: &Operator<MemoGroupId>) -> String {
    match operator {
        Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) => format!("SummaryAgg[{family:?}]"),
        Operator::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
            format!("SummaryEstimate[{query:?}]")
        }
        operator => operator.kind_name().to_string(),
    }
}

fn quantile(q: f64) -> AggIntent {
    AggIntent::Quantile {
        col: None,
        q,
        accuracy: AccuracyTarget::max_error(0.01).failure_probability(0.01),
    }
}

/// The running example (concepts doc, section 1).
#[test]
fn running_example() {
    let mut memo = Memo::default();
    let g1 = scan_group(&mut memo, "latency", &[]);
    let g3 = aggregate_group(&mut memo, g1, Reduction::by(vec![]), vec![quantile(0.95)]);
    query(&mut memo, g3);
    let memo = pass1_memo(memo, None).unwrap();
    println!("{}", memo_printout(&memo));
}
