//! Step 2, top-k reuse: an exact top-k reads a larger top-k of the same
//! input and partition, whose ranked rows hold its own.

use crate::rules::add_candidate_if_output_matches;
use crate::Pass2RuleError;
use asap_types::ir::operator::{AggIntent, Reduction};
use asap_types::ir::{NonASAPOp, Operator};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};

/// To each top-k group (`Aggregate` of one `TopK` measure, partitioned `by`
/// keys, without filters or `HAVING`) another top-k group over the same
/// input group and partition with the largest `k` serves, the candidate
/// `TopK(k)` over that group's ranked rows, partitioned by its leading key
/// columns: the `k` highest of the `k_max` highest rows are the `k`
/// highest. Only exact ranked rows give the reuse its answer: a ranking has
/// no error contract (`asap_accuracy::error_propagation`), so the
/// metric rule gives an approximate producer (a heap's readout) no edge
/// when a query with an accuracy target reaches it.
pub(crate) fn add_topk_candidates(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let topks: Vec<(MemoGroupId, MemoGroupId, Reduction, usize)> = (0..memo.groups().len())
        .filter_map(|index| {
            let group = MemoGroupId(index as u32);
            let (child, reduction, k) = topk(&memo.groups()[index].expression)?;
            Some((group, child, reduction, k))
        })
        .collect();
    for (group, child, reduction, k) in &topks {
        let Some((largest, k_max)) = topks
            .iter()
            .filter(|(_, other_child, other_reduction, _)| {
                other_child == child && other_reduction == reduction
            })
            .map(|(other, _, _, other_k)| (*other, *other_k))
            .max_by_key(|&(other, other_k)| (other_k, std::cmp::Reverse(other)))
        else {
            continue;
        };
        if k_max <= *k {
            continue;
        }
        let Operator::NonASAP(NonASAPOp::Aggregate {
            measures,
            output_names,
            filters,
            ..
        }) = memo.groups()[group.0 as usize].expression.clone()
        else {
            continue;
        };
        let keys = reduction.group_keys().map_or(0, |by| by.keys().len());
        add_candidate_if_output_matches(
            memo,
            *group,
            Operator::NonASAP(NonASAPOp::Aggregate {
                child: largest,
                reduction: Reduction::by((0..keys).collect()),
                measures,
                output_names,
                filters,
                having: None,
            }),
            None,
            OriginOfCandidateForMemoGroup::TopKReuse,
        )?;
    }
    Ok(())
}

/// The input group, partition and `k` of a top-k group: one `TopK` measure,
/// `by` keys (not `without`), no measure filter, no `HAVING`.
fn topk(operator: &Operator<MemoGroupId>) -> Option<(MemoGroupId, Reduction, usize)> {
    let Operator::NonASAP(NonASAPOp::Aggregate {
        child,
        reduction: reduction @ Reduction::Reduce(by),
        measures,
        filters,
        having: None,
        ..
    }) = operator
    else {
        return None;
    };
    let [AggIntent::TopK { k, .. }] = measures.as_slice() else {
        return None;
    };
    (!by.is_without() && filters.iter().all(Option::is_none))
        .then(|| (*child, reduction.clone(), *k))
}
