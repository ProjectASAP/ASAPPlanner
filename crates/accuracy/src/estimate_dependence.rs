//! Which estimates an output value depends on (`accuracy-models.md`,
//! "Which estimates a result depends on"): the operator classes and the
//! "error reaches the output" test that Pass 2's failure-probability split
//! (`readout_caps`) and Stage 3's constraint (K) both count by, so the
//! level Pass 2 gives a query meets (K) for that query.
//!
//! An estimate is one sketch readout value in one evaluation. An output
//! value depends on one value of each input an operator reads value by
//! value ([`inputs_read_value_by_value`]), on one input's value when the
//! operator copies each value from one input
//! ([`takes_each_value_from_one_input`]), and on every value of any other
//! input; only through inputs whose error reaches the output
//! ([`input_error_reaches_output`]) or whose values are chosen by
//! estimates ([`values_chosen_by_estimate`]); both read through
//! `ErrorMetricsOfMemo::input_estimates_reach_output`.

use asap_types::ir::operator::arguments::BinaryOpKind;
use asap_types::ir::{ASAPOp, BinaryOperator, NonASAPOp, Operator};
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::summary_type::SketchStatistic;

use crate::metric_rule::ErrorContractPerMetric;

/// Whether input `input` of a candidate with the error contracts
/// `contract_per_metric` can make its output miss: its error reaches the
/// output in some metric ($\beta > 0$). A count's input and the right
/// side of `and` and `unless` cannot ($\beta = 0$: only which series or
/// rows they hold is read), nor any input of an operator with no contract
/// (a subquery's operand, which may not carry error at all).
pub(crate) fn input_error_reaches_output(
    contract_per_metric: &ErrorContractPerMetric,
    input: usize,
) -> bool {
    contract_per_metric
        .iter()
        .flatten()
        .any(|contract| contract.input_error_reaches_output(input))
}

/// Per group of `memo`, by id: whether which values its output holds is
/// chosen by estimates, so that a consumer keeping or dropping a value by
/// its labels depends on every estimate that choice read. A top-k readout
/// chooses its keys by all the estimates it ranks
/// ([`ranks_by_estimate`]), and an operator reading such an input value by
/// value ([`inputs_read_value_by_value`]) passes the choice on: `topk(3,
/// q) * 2` holds the 3 keys the top-k chose. A reduction does not: `sum`
/// returns its one value whichever keys it reads. `order` is the memo's
/// topological order (`Memo::topological_order`).
///
/// Example: in `p90 and topk(3, q)`, the top-k's group and the `and`'s
/// are chosen by estimate, `p90`'s is not.
pub fn values_chosen_by_estimate(memo: &Memo, order: &[MemoGroupId]) -> Vec<bool> {
    let mut chosen = vec![false; memo.groups().len()];
    for &group in order {
        chosen[group.0 as usize] =
            memo.groups()[group.0 as usize]
                .candidates
                .iter()
                .any(|candidate| {
                    let operator = &candidate.expression.operator;
                    ranks_by_estimate(operator)
                        || inputs_read_value_by_value(operator).is_some_and(|value_by_value| {
                            operator
                                .children()
                                .iter()
                                .take(value_by_value)
                                .any(|child| chosen[child.0 as usize])
                        })
                });
    }
    chosen
}

/// Whether `operator`, a readout, chooses its output values by the
/// estimates it computes: a top-k ranks every item of its state, so each
/// of its output values depends on all of them.
pub fn ranks_by_estimate(operator: &Operator<MemoGroupId>) -> bool {
    matches!(
        operator,
        Operator::ASAP(ASAPOp::SummaryEstimate {
            query: SketchStatistic::TopK { .. },
            ..
        })
    )
}

/// How many direct inputs `operator` reads value by value: each output
/// value from one value (row or series) of each of them; `None` for an
/// operator one of whose output values may read many values of an input
/// (an aggregate, a window function, a time range or subquery, a `limit`
/// choosing rows by value, a deduplication). A filter, projection, sort,
/// time shift, relabel, label enrichment or series sample: 1, each output
/// value one input value. A binary operator, join or relational set
/// operator: 2, one value of each side; a concatenation: each of its
/// inputs. Operators its scalar expressions reference (`scalar(r)`) follow
/// the direct inputs and are read whole. Both counts are per evaluation,
/// not per subquery step: `max_over_time(q[1h:1m])` reads each readout
/// value of `q` at 60 steps, which they do not multiply in (no cap passes
/// through a subquery to the readouts below it).
pub fn inputs_read_value_by_value(operator: &Operator<MemoGroupId>) -> Option<usize> {
    match operator {
        Operator::NonASAP(
            NonASAPOp::Filter { .. }
            | NonASAPOp::Project { .. }
            | NonASAPOp::Sort { .. }
            | NonASAPOp::TimeShift { .. }
            | NonASAPOp::PromqlRelabel { .. }
            | NonASAPOp::PromqlInfoEnrich { .. }
            | NonASAPOp::PromqlSeriesSample { .. },
        ) => Some(1),
        Operator::NonASAP(
            NonASAPOp::BinaryOp { .. } | NonASAPOp::Join { .. } | NonASAPOp::SetOp { .. },
        ) => Some(2),
        Operator::NonASAP(NonASAPOp::Concat { children }) => Some(children.len()),
        _ => None,
    }
}

/// Whether `operator` copies each output value from exactly one of its
/// inputs: a PromQL set operator (`q or r` takes a series' value from `q`
/// when `q` has the series, else from `r`; `and` and `unless` only from
/// the left) or a concatenation (each row from one of its inputs).
pub fn takes_each_value_from_one_input(operator: &Operator<MemoGroupId>) -> bool {
    matches!(
        operator,
        Operator::NonASAP(
            NonASAPOp::BinaryOp {
                operator: BinaryOperator {
                    kind: BinaryOpKind::Set(_),
                    ..
                },
                ..
            } | NonASAPOp::Concat { .. }
        )
    )
}
